//! The body scanner: walks a fn body to find the errors it returns directly, the calls whose errors
//! it forwards, and the places where an error leaves the fn but can't be attributed.

use std::collections::HashMap;
use std::collections::HashSet;

use proc_macro2::TokenTree;
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::diagnostic::Position;
use crate::rules::errors_doc::file_index::ExitCollector;
use crate::rules::errors_doc::file_index::FileIndex;
use crate::rules::errors_doc::file_index::HelperFn;
use crate::rules::errors_doc::items::FnItem;
use crate::rules::errors_doc::known_apis;
use crate::rules::errors_doc::macros;
use crate::rules::errors_doc::macros::MacroKind;
use crate::rules::errors_doc::pass_through;
use crate::rules::errors_doc::pass_through::PassThrough;
use crate::syntax::span;
use crate::syntax::symbol_path::SymbolPath;
use crate::syntax::symbol_path::is_upper_camel_case;
use crate::syntax::types::peel_type_parens;

/// What a fn body says about the errors that leave it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodyFacts {
    /// The errors the body must document, in source order.
    pub obligations: Vec<Obligation>,

    /// Whether an error leaves the fn from a source the scan can't name, such as a `?` on a
    /// variable.
    pub has_opaque_source: bool,

    /// Whether an error leaves the fn through a mapper, constructor or conversion the scan can't
    /// resolve.
    pub has_opaque_error: bool,

    /// Every path, method name and macro identifier the body mentions, including inside closures.
    pub mentions: HashSet<SymbolPath>,

    /// The types `X` that an `X::from` path converts into, including inside closures.
    pub conversion_targets: HashSet<String>,

    /// The callees whose errors leave a closure or an async block of the body, which would be
    /// obligations if they left the fn, and the callees of possibly recursive calls, which a trait
    /// impl method may forward.
    pub nested_callees: HashSet<SymbolPath>,
}

impl BodyFacts {
    /// # Returns
    ///
    /// Whether no error leaves the fn: the body has no obligation and no opaque source or error.
    pub const fn is_infallible(&self) -> bool {
        self.obligations.is_empty() && !self.has_opaque_source && !self.has_opaque_error
    }
}

/// One error the body must document, and where it leaves the fn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obligation {
    pub kind: ObligationKind,
    pub position: Position,
}

/// The kinds of [`Obligation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObligationKind {
    /// The body constructs `error` and returns it.
    Direct { error: Spellings },

    /// The body propagates `callee`'s error unchanged, up to `From`.
    Forward { callee: Spellings },

    /// The body maps `callee`'s error into `errors`, so either may be documented.
    Either {
        callee: Spellings,
        errors: Vec<Spellings>,
    },

    /// The body maps `callee`'s error with a mapper the scan can't resolve, which `mapper`
    /// describes for messages.
    EitherUnknown { callee: Spellings, mapper: String },

    /// The body passes closures to `callee`, whose result leaves the fn, and the closures build and
    /// return `closure_errors`. Either the callee or the errors may be documented.
    HigherOrder {
        callee: Spellings,
        closure_errors: Vec<Spellings>,
    },

    /// The body returns an error that `helper` builds, which the scan can't resolve. The helper
    /// can't fail, so only a bullet for an error of `error_type`, or for any error if the type is
    /// unknown, documents it.
    HelperError {
        helper: Spellings,
        error_type: Option<SymbolPath>,
    },
}

impl ObligationKind {
    /// # Returns
    ///
    /// The errors of a `Direct` (one), an `Either` (the mapped ones) or a `HigherOrder` (the
    /// closures' ones), and none otherwise.
    pub fn errors(&self) -> &[Spellings] {
        match self {
            Self::Direct { error } => std::slice::from_ref(error),
            Self::Either { errors, .. }
            | Self::HigherOrder {
                closure_errors: errors,
                ..
            } => errors,
            Self::Forward { .. } | Self::EitherUnknown { .. } | Self::HelperError { .. } => &[],
        }
    }

    /// # Returns
    ///
    /// The callee of any kind but a `Direct` or a `HelperError`, or [`None`] for those, whose
    /// helpers can't fail.
    pub const fn callee(&self) -> Option<&Spellings> {
        match self {
            Self::Forward { callee }
            | Self::Either { callee, .. }
            | Self::EitherUnknown { callee, .. }
            | Self::HigherOrder { callee, .. } => Some(callee),
            Self::Direct { .. } | Self::HelperError { .. } => None,
        }
    }
}

/// The paths that may document one error or callee.
///
/// * A variant wrapped in another one, such as `CacheError::Internal(InternalError::X)`, is spelled
///   as the inner variant first and the wrapping one second.
/// * A constructor call `T::new_thing(..)` is spelled `T` and `T::NewThing`.
/// * A method at the end of a builder chain, such as `find(..).with_options(..)`, is spelled as the
///   last method first and the earlier calls of the chain after it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Spellings {
    paths: Vec<SymbolPath>,

    /// The type of the error value, such as `ProtocolError` for
    /// `ProtocolError::MalformedFrame(..)`, if the expression that built it names it.
    error_type: Option<SymbolPath>,
}

impl Spellings {
    /// # Returns
    ///
    /// The path that messages use.
    pub fn primary(&self) -> &SymbolPath {
        &self.paths[0]
    }

    /// # Returns
    ///
    /// Every spelling, the primary one first.
    pub fn paths(&self) -> &[SymbolPath] {
        &self.paths
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The spellings holding only `primary`.
    fn new(primary: SymbolPath) -> Self {
        Self {
            paths: vec![primary],
            error_type: None,
        }
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The spellings of an error value of `error_type`, holding only `primary`.
    fn of_error(primary: SymbolPath, error_type: Option<SymbolPath>) -> Self {
        Self {
            paths: vec![primary],
            error_type,
        }
    }

    /// Adds `alternative` as a further spelling, unless it is already one.
    fn push(&mut self, alternative: SymbolPath) {
        if !self.paths.contains(&alternative) {
            self.paths.push(alternative);
        }
    }
}

/// Scans the body of `fn_item`. A recursive call of the fn forwards nothing.
///
/// # Type Parameters
///
/// * `'ast` - The lifetime of the syntax tree the fn is in.
///
/// # Returns
///
/// The facts of the fn's body. Paths are normalized with the fn's scope, and helpers are resolved
/// through `file_index`.
pub fn scan<'ast>(fn_item: &FnItem<'ast>, file_index: &FileIndex<'ast>) -> BodyFacts {
    let mut body_scanner = BodyScanner {
        fn_item,
        file_index,
        nested_scope_depth: 0,
        closure_bindings: HashMap::new(),
        error_bindings: HashMap::new(),
        call_bindings: HashMap::new(),
        spawned_bindings: HashSet::new(),
        builder_bindings: HashMap::new(),
        forwarded_sites: HashSet::new(),
        err_binding_callees: HashMap::new(),
        in_place_return_site: None,
        facts: BodyFacts::default(),
    };
    if let Some(block) = fn_item.block {
        body_scanner.visit_block(block);
        body_scanner.classify_block_value(block, Site::Tail);
    }
    body_scanner.facts
}

/// The errors an error expression can evaluate to.
#[derive(Debug, Clone, Default)]
struct ErrorSet {
    errors: Vec<Spellings>,

    /// Whether some branch calls a helper or mapper that can't be resolved.
    has_unknown: bool,

    /// Whether some branch is a value the scan can't name as an error, such as a variable.
    has_value: bool,

    /// The input indices of a resolved helper's parameters that some branch passes on unchanged,
    /// such as the `error` of a helper that returns `error` when it can't classify it.
    passed_params: Vec<usize>,
}

impl ErrorSet {
    /// # Returns
    ///
    /// The set of an unresolved helper or mapper.
    const fn unknown() -> Self {
        Self {
            errors: Vec::new(),
            has_unknown: true,
            has_value: false,
            passed_params: Vec::new(),
        }
    }

    /// # Returns
    ///
    /// The set of a value the scan can't name as an error, such as a variable.
    const fn value() -> Self {
        Self {
            errors: Vec::new(),
            has_unknown: false,
            has_value: true,
            passed_params: Vec::new(),
        }
    }

    /// # Returns
    ///
    /// The set holding only `error`.
    fn single(error: Spellings) -> Self {
        Self {
            errors: vec![error],
            has_unknown: false,
            has_value: false,
            passed_params: Vec::new(),
        }
    }

    /// Adds the errors and passed parameters of `other` that aren't in `self` yet, and its unknown
    /// and value branches.
    fn union(&mut self, other: Self) {
        for error in other.errors {
            if !self.errors.contains(&error) {
                self.errors.push(error);
            }
        }
        for param_index in other.passed_params {
            self.pass_param(param_index);
        }
        self.has_unknown |= other.has_unknown;
        self.has_value |= other.has_value;
    }

    /// Adds a branch that passes on the parameter at `param_index`.
    fn pass_param(&mut self, param_index: usize) {
        if !self.passed_params.contains(&param_index) {
            self.passed_params.push(param_index);
        }
    }

    /// # Returns
    ///
    /// The errors, or [`None`] if there are none or some branch is unknown.
    fn into_resolved_errors(self) -> Option<Vec<Spellings>> {
        (!self.has_unknown && !self.errors.is_empty()).then_some(self.errors)
    }
}

/// A called function or method, as written in the body.
#[derive(Debug, Clone)]
struct Callee {
    spellings: Spellings,

    /// The position of the callee's last path segment or method name.
    position: Position,

    /// How certainly the call calls the scanned fn.
    recursion: Recursion,

    /// The errors that the closures passed to the call build and return. They leave the fn through
    /// the call.
    closure_error_set: ErrorSet,
}

/// How certainly a call calls the scanned fn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recursion {
    /// The call calls another fn.
    No,

    /// The call calls the scanned fn, so it forwards nothing.
    Certain,

    /// The call may call the scanned fn or an inherent fn of the same name that another file
    /// declares, so its callee is a nested callee.
    Possible,
}

/// What the scan knows about a closure bound with `let name = |..| ..;`.
#[derive(Debug, Clone)]
struct ClosureBinding {
    /// The errors the closure's body evaluates to, when the closure is used as an error mapper.
    error_set: ErrorSet,

    /// The callee of the closure body's tail, which a call of the closure forwards.
    tail_callee: Option<Callee>,
}

/// The bindings of one name, as [`BodyScanner`] keeps them.
struct NameBindings {
    closure: Option<ClosureBinding>,
    error: Option<ErrorSet>,
    call: Option<Callee>,
    is_spawned: bool,
    builder: Option<&'static str>,
    err_binding_callee: Option<Callee>,
}

/// How a `map_err` argument transforms an error.
enum Mapper {
    /// `Into::into`, `From::from`, `X::from`, or a closure returning its parameter converted.
    Conversion,

    /// A constructor or a resolved helper that builds `errors`.
    Errors { errors: Vec<Spellings> },

    /// A mapper the scan can't resolve, with a `description` for messages.
    Unknown { description: String },
}

/// Where a fallible expression leaves the fn.
#[derive(Debug, Clone, Copy)]
enum Site {
    /// The operand of a `?`.
    Try { question_position: Position },

    /// The fn's tail expression or a `return` value.
    Tail,
}

impl Site {
    /// # Returns
    ///
    /// The `?` position for a [`Site::Try`], or `fallback` for a [`Site::Tail`].
    const fn position_or(self, fallback: Position) -> Position {
        match self {
            Self::Try { question_position } => question_position,
            Self::Tail => fallback,
        }
    }

    /// # Returns
    ///
    /// Whether the site is the operand of a `?`.
    const fn is_try(self) -> bool {
        matches!(self, Self::Try { .. })
    }
}

/// The `syn` visitor behind [`scan`].
///
/// # Type Parameters
///
/// * `'context` - The lifetime of the fn and the file index the scan reads.
/// * `'ast` - The lifetime of the syntax tree the fn is in.
struct BodyScanner<'context, 'ast> {
    fn_item: &'context FnItem<'ast>,
    file_index: &'context FileIndex<'ast>,

    /// The number of closures, async or try blocks and generator macros around the current node.
    /// `?` inside them doesn't leave the fn.
    nested_scope_depth: usize,

    /// The closures bound with `let name = |..| ..;`, by name.
    closure_bindings: HashMap<String, ClosureBinding>,

    /// The errors bound with `let name = <error constructor>;`, by name.
    error_bindings: HashMap<String, ErrorSet>,

    /// The callees of the calls bound with `let name = <call>;`, by name.
    call_bindings: HashMap<String, Callee>,

    /// The names bound with `let name = spawn(..);`. `?` converts their join errors into some
    /// unknown variant of the fn's error type.
    spawned_bindings: HashSet<String>,

    /// The query builders bound with `let name = <builder>;`, by name, with their type's name.
    builder_bindings: HashMap<String, &'static str>,

    /// The positions of `Err(..)` calls and anyhow macros that pass on a binding whose forwarding
    /// is already recorded.
    forwarded_sites: HashSet<Position>,

    /// The callees whose error the `Err(binding)` pattern of the enclosing `match` arms and
    /// `if let` branches binds, by binding name, such as `foo` for `e` in `Err(e) => ..` of
    /// `match foo()`.
    err_binding_callees: HashMap<String, Callee>,

    /// The site where the value of the in-place closure being visited leaves the fn, if any.
    /// `return`s in its body leave the fn there too.
    in_place_return_site: Option<Site>,

    facts: BodyFacts,
}

impl<'ast> BodyScanner<'_, 'ast> {
    /// Records an obligation. Inside a nested scope, only the obligation's callee is recorded, as a
    /// nested callee.
    fn record(&mut self, kind: ObligationKind, position: Position) {
        if self.nested_scope_depth == 0 {
            self.facts.obligations.push(Obligation { kind, position });
        } else if let Some(callee) = kind.callee() {
            self.facts
                .nested_callees
                .extend(callee.paths().iter().cloned());
        }
    }

    /// Records the obligation of a call to `callee` whose error leaves the fn at `site`:
    ///
    /// * A recursive call records nothing.
    /// * A possibly recursive call records only its callee, as a nested callee.
    /// * A call that passes closures building errors is a `HigherOrder` obligation.
    /// * Any other call is a `Forward` obligation.
    fn record_forward(&mut self, callee: Callee, site: Site) {
        match callee.recursion {
            Recursion::No => {}
            Recursion::Certain => return,
            Recursion::Possible => {
                self.facts
                    .nested_callees
                    .extend(callee.spellings.paths().iter().cloned());
                return;
            }
        }
        let position = site.position_or(callee.position);
        let ErrorSet {
            errors,
            has_unknown,
            has_value,
            ..
        } = callee.closure_error_set;
        if has_unknown {
            self.record_opaque_error();
        }
        if has_value {
            self.record_opaque_source();
        }
        let kind = if errors.is_empty() {
            ObligationKind::Forward {
                callee: callee.spellings,
            }
        } else {
            ObligationKind::HigherOrder {
                callee: callee.spellings,
                closure_errors: self.convert(errors, site.is_try()),
            }
        };
        self.record(kind, position);
    }

    /// Records that an error leaves the fn from a source the scan can't name, unless the current
    /// node is inside a nested scope.
    const fn record_opaque_source(&mut self) {
        if self.nested_scope_depth == 0 {
            self.facts.has_opaque_source = true;
        }
    }

    /// Records that an error leaves the fn through a mapping the scan can't resolve, unless the
    /// current node is inside a nested scope.
    const fn record_opaque_error(&mut self) {
        if self.nested_scope_depth == 0 {
            self.facts.has_opaque_error = true;
        }
    }

    /// Records the errors that leave the fn at `position`:
    ///
    /// * Each error of `error_set` is a `Direct` obligation. If `converts` is set, it is converted
    ///   into the fn's own error type first, as [`Self::convert`] describes.
    /// * An unknown branch is an opaque error.
    /// * A value branch, such as a variable, is an opaque source.
    fn record_error_set(&mut self, error_set: ErrorSet, position: Position, converts: bool) {
        if error_set.has_unknown {
            self.record_opaque_error();
        }
        if error_set.has_value {
            self.record_opaque_source();
        }
        for error in self.convert(error_set.errors, converts) {
            self.record(ObligationKind::Direct { error }, position);
        }
    }

    /// Converts errors into the fn's own error type, as `?` and `.into()` do, if `converts` is set.
    ///
    /// An error whose type has a `From` conversion into the own error type in the file, through an
    /// `impl From` item or a `#[from]` field, is spelled as the variants that the conversion builds
    /// first, and as before after them. Other errors are left as they are.
    ///
    /// # Returns
    ///
    /// The converted errors.
    fn convert(&self, errors: Vec<Spellings>, converts: bool) -> Vec<Spellings> {
        let Some(own_error_type) = self.fn_item.own_error_type.as_ref().filter(|_| converts) else {
            return errors;
        };
        errors
            .into_iter()
            .map(|error| {
                let Some(variants) = error.error_type.as_ref().and_then(|error_type| {
                    self.file_index.convert(error_type, own_error_type.last())
                }) else {
                    return error;
                };
                let mut paths = variants;
                for path in error.paths {
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
                Spellings {
                    paths,
                    error_type: Some(own_error_type.clone()),
                }
            })
            .collect()
    }

    /// # Returns
    ///
    /// `path` as a symbol path normalized in the fn's scope, or [`None`] if nothing is left of it.
    fn symbol(&self, path: &syn::Path) -> Option<SymbolPath> {
        SymbolPath::from_syn_path(path).map(|symbol_path| self.fn_item.scope.normalize(symbol_path))
    }

    /// Classifies an expression whose value leaves the fn at `site`:
    ///
    /// * An awaited `spawn(..)` or `spawn_blocking(..)` fails with a join error, which is an opaque
    ///   error.
    /// * A conversion such as `x.into()` is classified through `x`. A converted value that isn't a
    ///   call is an opaque source.
    /// * `Err(..)` returns its argument's errors directly, and `Ok(..)` and `Some(..)` return none.
    /// * A call that runs a closure in place, as [`in_place_closure`] finds it, is classified by
    ///   [`Self::classify_in_place_closure`].
    /// * The sink fns, such as `try_join_all(..)`, are classified through the items of their
    ///   argument, as [`Self::classify_produced_items`] describes.
    /// * Other calls forward their callee, which [`Self::record_forward`] records. A call whose
    ///   callee can't be named, such as a field, is an opaque source.
    /// * Method calls are classified by [`Self::classify_method_call`].
    /// * The outer `?` of `timeout(..)??` forwards the timed-out call. Any other inner `?` is an
    ///   opaque source.
    /// * Blocks, `if` and `match` are classified through their tail expressions.
    /// * Macros are classified by [`Self::classify_macro_value`].
    /// * A variable bound with `let x = <call>;` forwards the call's callee.
    /// * A variable bound with `let x = spawn(..);` is an opaque error, since `?` converts its join
    ///   error.
    /// * Any other variable or field is an opaque source.
    /// * Anything else is an opaque source at a `?` site, and nothing at a tail site.
    fn classify_fallible(&mut self, expr: &syn::Expr, site: Site) {
        if is_awaited_spawn(expr) {
            self.record_opaque_error();
            return;
        }
        let operand = peel_await_and_parens(expr);
        if let Some(converted) = conversion_operand(operand) {
            if matches!(
                peel_await_and_parens(converted),
                syn::Expr::Call(_) | syn::Expr::MethodCall(_)
            ) {
                self.classify_fallible(converted, site);
            } else {
                self.record_opaque_source();
            }
            return;
        }
        match operand {
            syn::Expr::Call(call) => {
                if is_err_call(call) {
                    self.record_err_call(call, site);
                } else if is_ok_or_some_call(call) {
                } else if let Some(closure) = in_place_closure(call) {
                    self.classify_in_place_closure(closure, site);
                } else if call_path_last(call)
                    .is_some_and(|last| known_apis::SINK_FNS.contains(&last.as_str()))
                {
                    let is_classified = call
                        .args
                        .first()
                        .is_some_and(|items| self.classify_produced_items(items, site));
                    if !is_classified {
                        self.record_opaque_source();
                    }
                } else {
                    match self.callee_of(operand) {
                        Some(callee) => self.record_forward(callee, site),
                        None => self.record_opaque_source(),
                    }
                }
            }
            syn::Expr::MethodCall(method_call) => {
                self.classify_method_call(method_call, operand, site);
            }
            syn::Expr::Try(inner_try) => {
                if site.is_try() {
                    self.classify_inner_try(inner_try, site);
                }
            }
            syn::Expr::Block(expr_block) => self.classify_block_value(&expr_block.block, site),
            syn::Expr::Unsafe(expr_unsafe) => self.classify_block_value(&expr_unsafe.block, site),
            syn::Expr::If(expr_if) => {
                let err_bindings = self.if_let_err_bindings(expr_if);
                self.with_err_bindings(err_bindings, |body_scanner| {
                    body_scanner.classify_block_value(&expr_if.then_branch, site);
                });
                if let Some((_, else_branch)) = &expr_if.else_branch {
                    self.classify_fallible(else_branch, site);
                }
            }
            syn::Expr::Match(expr_match) => {
                for arm in &expr_match.arms {
                    let err_bindings = self
                        .err_binding_callee(&arm.pat, &expr_match.expr)
                        .into_iter()
                        .collect();
                    self.with_err_bindings(err_bindings, |body_scanner| {
                        body_scanner.classify_fallible(&arm.body, site);
                    });
                }
            }
            syn::Expr::Macro(expr_macro) => self.classify_macro_value(&expr_macro.mac, site),
            syn::Expr::Return(_) => {}
            syn::Expr::Path(expr_path)
                if let Some(ident) = expr_path.path.get_ident()
                    && self.spawned_bindings.contains(&ident.to_string()) =>
            {
                self.record_opaque_error();
            }
            syn::Expr::Path(expr_path)
                if let Some(ident) = expr_path.path.get_ident()
                    && let Some(callee) = self.call_bindings.get(&ident.to_string()) =>
            {
                self.record_forward(callee.clone(), site);
            }
            syn::Expr::Path(_) | syn::Expr::Field(_) => self.record_opaque_source(),
            _ if site.is_try() => self.record_opaque_source(),
            _ => {}
        }
    }

    /// Classifies the value of `block`, which leaves the fn at `site`: its tail expression, or a
    /// brace-delimited macro without `;` at its end, such as a `select! { .. }`.
    fn classify_block_value(&mut self, block: &syn::Block, site: Site) {
        match block.stmts.last() {
            Some(syn::Stmt::Expr(expr, None)) => self.classify_fallible(expr, site),
            Some(syn::Stmt::Macro(stmt_macro)) if stmt_macro.semi_token.is_none() => {
                self.classify_macro_value(&stmt_macro.mac, site);
            }
            _ => {}
        }
    }

    /// Classifies a closure that a call runs in place, whose value is the call's value and leaves
    /// the fn at `site`. The closure's body is visited at the current depth, so its `?`, `return`
    /// and anyhow-macro sites leave the fn too, and its value and `return` values are classified at
    /// `site`.
    fn classify_in_place_closure(&mut self, closure: &syn::ExprClosure, site: Site) {
        self.with_hidden_bindings(closure_parameters(closure), |body_scanner| {
            let outer_return_site = body_scanner.in_place_return_site.replace(site);
            body_scanner.visit_expr(&closure.body);
            body_scanner.classify_fallible(&closure.body, site);
            body_scanner.in_place_return_site = outer_return_site;
        });
    }

    /// Classifies a macro whose value leaves the fn at `site`:
    ///
    /// * The arm bodies of a `select!` are classified as the macro's value.
    /// * The futures of a `try_join!` are classified as the calls whose errors leave the fn.
    /// * An anyhow macro gives nothing here, since [`Visit::visit_macro`] records it.
    /// * Any other macro is an opaque source at a `?` site, and nothing at a tail site.
    fn classify_macro_value(&mut self, mac: &syn::Macro, site: Site) {
        let macro_kind = macros::kind(mac);
        let macro_body = match macro_kind {
            MacroKind::Anyhow => return,
            MacroKind::Select | MacroKind::TryJoin => macros::parse_body(mac, macro_kind),
            MacroKind::Generator | MacroKind::PatternTaking | MacroKind::Other => None,
        };
        let Some(macro_body) = macro_body else {
            if site.is_try() {
                self.record_opaque_source();
            }
            return;
        };
        let values = if macro_kind == MacroKind::Select {
            &macro_body.arm_bodies
        } else {
            &macro_body.exprs
        };
        for value in values {
            self.classify_fallible(value, site);
        }
    }

    /// Classifies a fallible method call:
    ///
    /// * `.ok_or(e)` and `.ok_or_else(|| e)` return `e` directly. Their receiver is an `Option`.
    /// * `.map_err(f)` is classified by [`Self::classify_map_err`].
    /// * `.context(..)` and `.with_context(..)` map the receiver's error into `anyhow::Error`. On a
    ///   receiver without a callee, they are an opaque source.
    /// * `.and_then(f)` is classified through its receiver and through `f`.
    /// * `.or_else(f)` replaces the receiver's error, so it is classified through `f` only.
    /// * Pass-through adaptors such as `.map(..)` are classified through their receiver.
    /// * `.try_for_each(f)` and `.try_fold(init, f)` are classified through the value of `f`.
    /// * Sink methods such as `.collect()` are classified through the items of their receiver, as
    ///   [`Self::classify_produced_items`] describes.
    /// * Either of them is an opaque source when the scan can't tell where the items come from.
    /// * Methods such as `.unwrap_or(..)` are opaque sources when they may return the receiver's
    ///   value, and are classified through their arguments when they may return one.
    /// * Any other method call forwards the method.
    fn classify_method_call(
        &mut self,
        method_call: &syn::ExprMethodCall,
        operand: &syn::Expr,
        site: Site,
    ) {
        let method = method_call.method.to_string();
        let method = method.as_str();
        let method_position = span::start_position(method_call.method.span());
        match method {
            "ok_or" => {
                let error_set = method_call
                    .args
                    .first()
                    .map_or_else(ErrorSet::unknown, |arg| self.classify_error(arg, true));
                self.record_error_set(error_set, method_position, site.is_try());
            }
            "ok_or_else" => {
                let error_set = match method_call
                    .args
                    .first()
                    .map(|arg| self.classify_mapper(arg))
                {
                    Some(Mapper::Errors { errors }) => ErrorSet {
                        errors,
                        ..ErrorSet::default()
                    },
                    _ => ErrorSet::unknown(),
                };
                self.record_error_set(error_set, method_position, site.is_try());
            }
            "map_err" => self.classify_map_err(method_call, site),
            "context" | "with_context" => match self.callee_of(&method_call.receiver) {
                Some(callee) => self.record(
                    ObligationKind::Either {
                        callee: callee.spellings,
                        errors: vec![anyhow_error_spellings()],
                    },
                    site.position_or(callee.position),
                ),
                None => self.record_opaque_source(),
            },
            "and_then" => {
                self.classify_fallible(&method_call.receiver, site);
                self.classify_chained_fn(method_call.args.first(), site);
            }
            "or_else" => self.classify_chained_fn(method_call.args.first(), site),
            _ if known_apis::PASS_THROUGH_METHODS.contains(&method) => {
                self.classify_fallible(&method_call.receiver, site);
            }
            "try_for_each" | "try_fold" | "try_for_each_concurrent" => {
                let is_classified = method_call
                    .args
                    .last()
                    .is_some_and(|callable| self.classify_callable_value(callable, site));
                if !is_classified {
                    self.record_opaque_source();
                }
            }
            _ if known_apis::SINK_METHODS.contains(&method) => {
                if !self.classify_produced_items(&method_call.receiver, site) {
                    self.record_opaque_source();
                }
            }
            _ if is_value_method(method) => {
                if known_apis::RECEIVER_VALUE_METHODS.contains(&method) {
                    self.record_opaque_source();
                }
                if known_apis::ARGUMENT_VALUE_METHODS.contains(&method) {
                    for arg in &method_call.args {
                        self.classify_fallible(closure_value(arg), site);
                    }
                }
            }
            _ => {
                if let Some(callee) = self.callee_of(operand) {
                    self.record_forward(callee, site);
                }
            }
        }
    }

    /// Classifies the fn argument of `.and_then(f)` or `.or_else(f)`, whose error leaves the fn at
    /// `site`:
    ///
    /// * A closure is classified through its body's value.
    /// * A path to a fn forwards that fn.
    /// * Anything else is an opaque source.
    fn classify_chained_fn(&mut self, chained_fn: Option<&syn::Expr>, site: Site) {
        match chained_fn.map(peel_references) {
            Some(syn::Expr::Closure(closure)) => self.classify_fallible(&closure.body, site),
            Some(syn::Expr::Path(expr_path)) => match self.path_callee(&expr_path.path) {
                Some(callee) => self.record_forward(callee, site),
                None => self.record_opaque_source(),
            },
            _ => self.record_opaque_source(),
        }
    }

    /// Classifies `receiver.map_err(f)` by the receiver's callee and by what `f` does:
    ///
    /// * A conversion forwards the callee.
    /// * A constructor maps the callee's error into the errors it builds.
    /// * An unknown mapper maps it into something unknown, which is also an opaque error.
    /// * Without a callee (the receiver is a variable), constructed errors are returned directly, a
    ///   conversion is an opaque source, and an unknown mapper is an opaque error.
    fn classify_map_err(&mut self, method_call: &syn::ExprMethodCall, site: Site) {
        let callee = self.callee_of(&method_call.receiver);
        let mapper = method_call.args.first().map_or_else(
            || Mapper::Unknown {
                description: "an unparsable mapper".to_owned(),
            },
            |arg| self.classify_mapper(arg),
        );
        let method_position = span::start_position(method_call.method.span());
        match (callee, mapper) {
            (Some(callee), Mapper::Conversion) => self.record_forward(callee, site),
            (Some(callee), Mapper::Errors { errors }) => self.record(
                ObligationKind::Either {
                    callee: callee.spellings,
                    errors: self.convert(errors, site.is_try()),
                },
                site.position_or(callee.position),
            ),
            (
                Some(callee),
                Mapper::Unknown {
                    description: mapper,
                },
            ) => {
                self.record_opaque_error();
                self.record(
                    ObligationKind::EitherUnknown {
                        callee: callee.spellings,
                        mapper,
                    },
                    site.position_or(callee.position),
                );
            }
            (None, Mapper::Conversion) => self.record_opaque_source(),
            (None, Mapper::Errors { errors }) => {
                for error in self.convert(errors, site.is_try()) {
                    self.record(
                        ObligationKind::Direct { error },
                        site.position_or(method_position),
                    );
                }
            }
            (None, Mapper::Unknown { .. }) => self.record_opaque_error(),
        }
    }

    /// Classifies the outer `?` of `inner?`: it forwards the timed-out call when `inner`'s chain
    /// starts with `timeout(_, x)` or `timeout_at(_, x)`, and is an opaque source otherwise.
    fn classify_inner_try(&mut self, inner_try: &syn::ExprTry, site: Site) {
        let timed_out_call = match chain_root(&inner_try.expr) {
            syn::Expr::Call(call)
                if call_path_last(call)
                    .is_some_and(|last| last == "timeout" || last == "timeout_at") =>
            {
                call.args.iter().nth(1)
            }
            _ => None,
        };
        match timed_out_call.and_then(|future| self.callee_of(future)) {
            Some(callee) => self.record_forward(callee, site),
            None => self.record_opaque_source(),
        }
    }

    /// Finds the callee of a call or method call, after peeling `.await`, parentheses and the
    /// adaptors that [`peel_error_adaptors`] peels.
    ///
    /// * The receiver of an adaptor gives the callee, so `load(key).context(..)` and
    ///   `load(key).map_err(Error::Wrap)` give the callee of `load`.
    /// * A call to a path gives the callee of [`Self::path_callee`].
    /// * A method call gives its method name. On a query builder that [`Self::builder_type`] names,
    ///   the name is qualified by the builder's type, such as `QueryAs::fetch_all`. The earlier
    ///   method calls of its chain are alternative spellings, so `find(..).with_options(..)` is
    ///   spelled `with_options` and `find`. A call at the chain's root, such as
    ///   `SpiderClient::builder(..)` in `SpiderClient::builder(..).connect()`, is not. A possibly
    ///   recursive `self.name(..)` call, as [`Self::self_call_recursion`] decides, is spelled
    ///   `Self::name` instead.
    /// * `block_on(x)` and `.block_on(x)` give the callee of `x`.
    /// * A call that runs a closure in place, as [`in_place_closure`] finds it, gives the callee of
    ///   the closure's value, so `std::thread::scope(|_| load())` gives the callee of `load`.
    /// * A call that passes callables is completed by [`Self::with_callable_args`].
    ///
    /// # Returns
    ///
    /// The callee, or [`None`] if `expr` isn't a call, calls something other than a path (such as a
    /// field), calls a value method such as `.expect(..)`, or runs a closure in place whose value
    /// has no callee.
    fn callee_of(&self, expr: &syn::Expr) -> Option<Callee> {
        match peel_error_adaptors(expr) {
            syn::Expr::Call(call) => {
                if let Some(closure) = in_place_closure(call) {
                    return self.callee_of(peel_block_tail(&closure.body));
                }
                let syn::Expr::Path(expr_path) = peel_parens(&call.func) else {
                    return None;
                };
                if expr_path
                    .path
                    .segments
                    .last()
                    .is_some_and(|segment| segment.ident == "block_on")
                    && let Some(future) = call.args.last()
                {
                    return self.callee_of(future);
                }
                let callee = self.path_callee(&expr_path.path)?;
                Some(self.with_callable_args(callee, &call.args))
            }
            syn::Expr::MethodCall(method_call) => {
                let method = method_call.method.to_string();
                if method == "block_on" && method_call.args.len() == 1 {
                    return method_call
                        .args
                        .first()
                        .and_then(|future| self.callee_of(future));
                }
                if is_value_method(&method) {
                    return None;
                }
                let recursion = if method == self.fn_item.name
                    && is_ident_path(peel_parens(&method_call.receiver), "self")
                {
                    self.self_call_recursion(None)
                } else {
                    Recursion::No
                };
                let spellings = if recursion == Recursion::Possible {
                    Spellings::new(SymbolPath::parse(&format!("Self::{method}"))?)
                } else {
                    let mut spellings = Spellings::new(self.method_path(method_call));
                    for chain_method in chain_methods(&method_call.receiver) {
                        spellings.push(chain_method);
                    }
                    spellings
                };
                let callee = Callee {
                    spellings,
                    position: span::start_position(method_call.method.span()),
                    recursion,
                    closure_error_set: ErrorSet::default(),
                };
                Some(self.with_callable_args(callee, &method_call.args))
            }
            _ => None,
        }
    }

    /// Completes the callee of a call whose arguments include callables: inline closures and async
    /// blocks, and paths that [`Self::callable_path`] names.
    ///
    /// * The callees of the closures' and async blocks' values and the callable paths become the
    ///   callee's first spellings, so `get_or_try_init(Self::init)` is spelled `Self::init` first
    ///   and `get_or_try_init` second.
    /// * The errors that the closures and async blocks build and return make up the callee's
    ///   closure error set.
    ///
    /// # Returns
    ///
    /// The completed callee.
    fn with_callable_args(
        &self,
        mut callee: Callee,
        args: &syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>,
    ) -> Callee {
        let mut leading_paths: Vec<SymbolPath> = Vec::new();
        for arg in args {
            if let syn::Expr::Closure(closure) = peel_references(arg) {
                if let Some(value_callee) = self.callee_of(closure_result(closure)) {
                    leading_paths.extend(value_callee.spellings.paths);
                }
                callee
                    .closure_error_set
                    .union(self.closure_error_set(&closure.body));
            } else if let syn::Expr::Async(expr_async) = peel_references(arg) {
                if let Some(value_callee) = block_tail(&expr_async.block)
                    .and_then(|tail| self.callee_of(peel_block_tail(tail)))
                {
                    leading_paths.extend(value_callee.spellings.paths);
                }
                callee
                    .closure_error_set
                    .union(self.closure_error_set(peel_references(arg)));
            } else if let Some(arg_callee) = self.callable_path(arg) {
                leading_paths.extend(arg_callee.spellings.paths);
            }
        }
        if !leading_paths.is_empty() {
            let mut spellings = Spellings::new(leading_paths.remove(0));
            for path in leading_paths.into_iter().chain(callee.spellings.paths) {
                spellings.push(path);
            }
            callee.spellings = spellings;
        }
        callee
    }

    /// # Returns
    ///
    /// The callee of `expr` if it is a path to a fn: a qualified path whose last segment isn't in
    /// upper camel case, such as `Self::init`, or the name of a free fn of the file that no
    /// parameter shadows, of a `let` closure binding or of a callable parameter. A reference to
    /// such a path counts too.
    fn callable_path(&self, expr: &syn::Expr) -> Option<Callee> {
        let syn::Expr::Path(expr_path) = peel_references(expr) else {
            return None;
        };
        let symbol_path = self.symbol(&expr_path.path)?;
        let name = symbol_path.last();
        let is_free_fn = !self.fn_item.param_names.iter().any(|param| param == name)
            && self
                .file_index
                .helper_fns(name)
                .iter()
                .any(|helper_fn| helper_fn.owner.is_none());
        let is_callable = !is_upper_camel_case(name)
            && (symbol_path.qualifier().is_some()
                || is_free_fn
                || self.closure_bindings.contains_key(name)
                || self
                    .fn_item
                    .callable_params
                    .iter()
                    .any(|param| param == name));
        if is_callable {
            self.path_callee(&expr_path.path)
        } else {
            None
        }
    }

    /// Collects the errors that a closure's body or an async block builds and lets leave it: in its
    /// value, in its `return` expressions, and in the operands of its `?` expressions, as
    /// [`Self::value_errors`] finds them. Calls inside aren't counted, since their errors belong to
    /// the closure or the async block.
    ///
    /// # Returns
    ///
    /// The errors.
    fn closure_error_set(&self, body: &syn::Expr) -> ErrorSet {
        let mut exit_collector = ExitCollector::default();
        let value = match peel_parens(body) {
            syn::Expr::Async(expr_async) => {
                exit_collector.visit_block(&expr_async.block);
                block_tail(&expr_async.block).map(peel_block_tail)
            }
            body => {
                exit_collector.visit_expr(body);
                Some(peel_block_tail(body))
            }
        };
        let mut error_set = value.map_or_else(ErrorSet::default, |value| self.value_errors(value));
        for exit in exit_collector.returned.iter().chain(&exit_collector.tried) {
            error_set.union(self.value_errors(exit));
        }
        error_set
    }

    /// Finds the errors that a value built in place returns, without counting calls:
    ///
    /// * `Err(e)` returns the errors of `e`.
    /// * `.ok_or(e)`, `.ok_or_else(f)` and `.map_err(f)` return the errors `e` or `f` builds.
    /// * Blocks, `if` and `match` return the errors of their branches.
    ///
    /// # Returns
    ///
    /// The errors of `expr`.
    fn value_errors(&self, expr: &syn::Expr) -> ErrorSet {
        let operand = peel_await_and_parens(expr);
        if let Some(converted) = conversion_operand(operand) {
            return self.value_errors(converted);
        }
        match operand {
            syn::Expr::Call(call) if is_err_call(call) => call
                .args
                .first()
                .map_or_else(ErrorSet::value, |arg| self.classify_error(arg, true)),
            syn::Expr::MethodCall(method_call) => {
                let Some(arg) = method_call.args.first() else {
                    return ErrorSet::default();
                };
                match method_call.method.to_string().as_str() {
                    "ok_or" => self.classify_error(arg, true),
                    "ok_or_else" | "map_err" => match self.classify_mapper(arg) {
                        Mapper::Errors { errors } => ErrorSet {
                            errors,
                            ..ErrorSet::default()
                        },
                        Mapper::Conversion => ErrorSet::default(),
                        Mapper::Unknown { .. } => ErrorSet::unknown(),
                    },
                    _ => ErrorSet::default(),
                }
            }
            syn::Expr::Block(expr_block) => self.block_value_errors(&expr_block.block),
            syn::Expr::Unsafe(expr_unsafe) => self.block_value_errors(&expr_unsafe.block),
            syn::Expr::If(expr_if) => {
                let mut error_set = self.block_value_errors(&expr_if.then_branch);
                if let Some((_, else_branch)) = &expr_if.else_branch {
                    error_set.union(self.value_errors(else_branch));
                }
                error_set
            }
            syn::Expr::Match(expr_match) => {
                let mut error_set = ErrorSet::default();
                for arm in &expr_match.arms {
                    error_set.union(self.value_errors(&arm.body));
                }
                error_set
            }
            _ => ErrorSet::default(),
        }
    }

    /// # Returns
    ///
    /// The errors that the tail expression of `block` returns, as [`Self::value_errors`] finds
    /// them.
    fn block_value_errors(&self, block: &syn::Block) -> ErrorSet {
        block_tail(block).map_or_else(ErrorSet::default, |tail| self.value_errors(tail))
    }

    /// Classifies the items of an iterator or stream whose results a sink such as `.collect()` or
    /// `try_join_all(..)` gathers, at `site`. The receiver chain of `items` is walked back past
    /// adaptors that keep the items, such as `.filter(..)`, to the adaptor that produces them, such
    /// as `.map(f)`, whose callable is classified by [`Self::classify_callable_value`].
    ///
    /// # Returns
    ///
    /// Whether the items were classified.
    fn classify_produced_items(&mut self, items: &syn::Expr, site: Site) -> bool {
        let mut expr = items;
        loop {
            let syn::Expr::MethodCall(method_call) = peel_await_and_parens(expr) else {
                return false;
            };
            let method = method_call.method.to_string();
            if known_apis::ITEM_PRODUCING_METHODS.contains(&method.as_str()) {
                return method_call
                    .args
                    .last()
                    .is_some_and(|callable| self.classify_callable_value(callable, site));
            }
            if !known_apis::ITEM_KEEPING_METHODS.contains(&method.as_str()) {
                return false;
            }
            expr = &method_call.receiver;
        }
    }

    /// Classifies the value of a callable whose result leaves the fn at `site`:
    ///
    /// * A closure's value and its `return` expressions are classified by
    ///   [`Self::classify_fallible`].
    /// * A path whose last segment isn't in upper camel case forwards that fn. In a callable's
    ///   position, even a name that the file doesn't declare, such as `.map(parse)`, is a fn.
    ///
    /// # Returns
    ///
    /// Whether `callable` is a closure or a callable path.
    fn classify_callable_value(&mut self, callable: &syn::Expr, site: Site) -> bool {
        if let syn::Expr::Closure(closure) = peel_references(callable) {
            let mut exit_collector = ExitCollector::default();
            match peel_parens(&closure.body) {
                syn::Expr::Async(expr_async) => exit_collector.visit_block(&expr_async.block),
                body => exit_collector.visit_expr(body),
            }
            self.classify_fallible(closure_result(closure), site);
            for returned in exit_collector.returned {
                self.classify_fallible(returned, site);
            }
            return true;
        }
        let callee =
            match peel_references(callable) {
                syn::Expr::Path(expr_path)
                    if expr_path.path.segments.last().is_some_and(|segment| {
                        !is_upper_camel_case(&segment.ident.to_string())
                    }) =>
                {
                    self.path_callee(&expr_path.path)
                }
                _ => None,
            };
        let Some(callee) = callee else {
            return false;
        };
        self.record_forward(callee, site);
        true
    }

    /// # Returns
    ///
    /// The path of a method call's method: its name, qualified by the receiver's type if
    /// [`Self::builder_type`] names it.
    fn method_path(&self, method_call: &syn::ExprMethodCall) -> SymbolPath {
        let method = method_call.method.to_string();
        self.builder_type(&method_call.receiver)
            .and_then(|builder_type| SymbolPath::parse(&format!("{builder_type}::{method}")))
            .unwrap_or_else(|| SymbolPath::single(&method))
    }

    /// Names the type of a query builder from its construction, through the builder fns that
    /// [`known_apis::BUILDER_FNS`] lists, the methods of [`known_apis::BUILDER_KEEPING_METHODS`]
    /// that return the builder, and the `let` bindings of builders.
    ///
    /// # Returns
    ///
    /// The builder's type name, such as `QueryAs` for `sqlx::query_as::<_, T>(..).bind(x)`, or
    /// [`None`] if `expr` isn't such a builder.
    fn builder_type(&self, expr: &syn::Expr) -> Option<&'static str> {
        match peel_await_and_parens(expr) {
            syn::Expr::MethodCall(method_call)
                if known_apis::BUILDER_KEEPING_METHODS
                    .contains(&method_call.method.to_string().as_str()) =>
            {
                self.builder_type(&method_call.receiver)
            }
            syn::Expr::Call(call) => {
                let syn::Expr::Path(expr_path) = peel_parens(&call.func) else {
                    return None;
                };
                let segments = &expr_path.path.segments;
                let is_builder_path = segments.len() == 1
                    || segments.first().is_some_and(|first| first.ident == "sqlx");
                let last = segments.last()?.ident.to_string();
                known_apis::BUILDER_FNS
                    .iter()
                    .find(|(builder_fn, _)| is_builder_path && *builder_fn == last)
                    .map(|(_, builder_type)| *builder_type)
            }
            syn::Expr::Path(expr_path) => expr_path
                .path
                .get_ident()
                .and_then(|ident| self.builder_bindings.get(&ident.to_string()).copied()),
            _ => None,
        }
    }

    /// Names the fn that a call to `path` calls:
    ///
    /// * `Ok`, `Some` and `Err` aren't callees.
    /// * A `let` closure binding gives the callee of the closure body's tail, if it has one.
    /// * A call of the scanned fn's own name is recursive if the path is unqualified and the
    ///   scanned fn is a free fn. If the path's qualifier is a name of `Self`,
    ///   [`Self::self_call_recursion`] decides.
    ///
    /// # Returns
    ///
    /// The callee, or [`None`] if `path` names no fn the scan can name.
    fn path_callee(&self, path: &syn::Path) -> Option<Callee> {
        let symbol_path = self.symbol(path)?;
        if matches!(symbol_path.last(), "Ok" | "Some" | "Err") {
            return None;
        }
        if symbol_path.len() == 1
            && let Some(closure_binding) = self.closure_bindings.get(symbol_path.last())
        {
            return closure_binding.tail_callee.clone();
        }
        let last_segment = path.segments.last()?;
        let recursion = if symbol_path.last() == self.fn_item.name {
            match symbol_path.qualifier() {
                None if !self.fn_item.is_associated => Recursion::Certain,
                Some(qualifier) if self.fn_item.scope.is_self_name(qualifier) => {
                    self.self_call_recursion(Some(qualifier))
                }
                _ => Recursion::No,
            }
        } else {
            Recursion::No
        };
        Some(Callee {
            spellings: Spellings::new(symbol_path),
            position: span::start_position(last_segment.ident.span()),
            recursion,
            closure_error_set: ErrorSet::default(),
        })
    }

    /// Decides how certainly a call of the scanned fn's name through `qualifier`, a name of `Self`,
    /// calls the fn. A [`None`] qualifier is a call through `self.`, as in `self.name(..)`.
    ///
    /// In a trait impl, a call through `Self`, the self type or `self.` calls the type's inherent
    /// fn of the name if there is one:
    ///
    /// * If an inherent impl of the file has it, the call isn't recursive.
    /// * Otherwise another file may have it, so the call is possibly recursive.
    ///
    /// Any other call, such as one through the trait's name or one outside a trait impl, is
    /// recursive.
    ///
    /// # Returns
    ///
    /// The call's recursion.
    fn self_call_recursion(&self, qualifier: Option<&str>) -> Recursion {
        let Some(self_type) = self
            .fn_item
            .trait_impl_self_type
            .as_deref()
            .filter(|self_type| {
                qualifier.is_none_or(|qualifier| qualifier == "Self" || qualifier == *self_type)
            })
        else {
            return Recursion::Certain;
        };
        if self
            .file_index
            .has_inherent_fn(self_type, &self.fn_item.name)
        {
            Recursion::No
        } else {
            Recursion::Possible
        }
    }

    /// Classifies an error expression, after peeling parentheses, `.await`, `.into()`,
    /// `Into::into(x)`, `X::from(x)` and `Box::new(x)`:
    ///
    /// * A path whose last segment is upper camel case is that unit variant.
    /// * A call to a path is classified by [`Self::classify_constructor`].
    /// * A struct literal is its path.
    /// * `anyhow!(..)` and `format_err!(..)` are `anyhow::Error`.
    /// * Blocks, `if` and `match` are the union of their tail expressions.
    /// * A method call, such as `self.classify(error)`, or a call to something other than a path,
    ///   is unknown.
    /// * Anything else, such as a variable, is a value.
    ///
    /// # Returns
    ///
    /// The errors `expr` can evaluate to.
    fn classify_error(&self, expr: &syn::Expr, allow_helpers: bool) -> ErrorSet {
        match peel_conversions(expr) {
            syn::Expr::Path(expr_path) => match self.symbol(&expr_path.path) {
                Some(path) if path.last() == "None" => ErrorSet::default(),
                Some(path) if is_upper_camel_case(path.last()) => {
                    let error_type = path.parent();
                    ErrorSet::single(Spellings::of_error(path, error_type))
                }
                Some(path) if path.len() == 1 => self
                    .error_bindings
                    .get(path.last())
                    .cloned()
                    .unwrap_or_else(ErrorSet::value),
                _ => ErrorSet::value(),
            },
            syn::Expr::Call(call) => match peel_parens(&call.func) {
                syn::Expr::Path(expr_path) => self
                    .symbol(&expr_path.path)
                    .map_or_else(ErrorSet::unknown, |path| {
                        self.classify_constructor(path, &call.args, allow_helpers)
                    }),
                _ => ErrorSet::unknown(),
            },
            syn::Expr::MethodCall(method_call)
                if is_ident_path(peel_parens(&method_call.receiver), "self") =>
            {
                SymbolPath::parse(&format!("Self::{}", method_call.method))
                    .map_or_else(ErrorSet::unknown, |path| {
                        self.resolve_helper(&path, allow_helpers)
                    })
            }
            syn::Expr::MethodCall(_) => ErrorSet::unknown(),
            syn::Expr::Struct(expr_struct) => {
                self.symbol(&expr_struct.path)
                    .map_or_else(ErrorSet::unknown, |path| {
                        let error_type = path.parent();
                        ErrorSet::single(Spellings::of_error(path, error_type))
                    })
            }
            syn::Expr::Macro(expr_macro) if is_anyhow_constructor(&expr_macro.mac) => {
                ErrorSet::single(anyhow_error_spellings())
            }
            syn::Expr::Block(expr_block) => {
                self.classify_block_error(&expr_block.block, allow_helpers)
            }
            syn::Expr::Unsafe(expr_unsafe) => {
                self.classify_block_error(&expr_unsafe.block, allow_helpers)
            }
            syn::Expr::If(expr_if) => {
                let mut error_set = self.classify_block_error(&expr_if.then_branch, allow_helpers);
                if let Some((_, else_branch)) = &expr_if.else_branch {
                    error_set.union(self.classify_error(else_branch, allow_helpers));
                }
                error_set
            }
            syn::Expr::Match(expr_match) => {
                let mut error_set = ErrorSet::default();
                for arm in &expr_match.arms {
                    error_set.union(self.classify_error(&arm.body, allow_helpers));
                }
                error_set
            }
            _ => ErrorSet::value(),
        }
    }

    /// # Returns
    ///
    /// The errors of `block`'s tail expression, or nothing if it has none.
    fn classify_block_error(&self, block: &syn::Block, allow_helpers: bool) -> ErrorSet {
        block_tail(block).map_or_else(ErrorSet::default, |tail| {
            self.classify_error(tail, allow_helpers)
        })
    }

    /// Classifies a call to `path` with `args`, an error constructor or a helper:
    ///
    /// * An upper camel case last segment is that tuple variant. If its only argument is itself a
    ///   variant, the inner variant is the primary spelling and the tuple variant an alternative.
    /// * `T::ctor` with an upper camel case `T` other than a name of `Self` is the type `T`, with
    ///   `T::Ctor` in upper camel case and `T::ctor` itself as alternative spellings.
    /// * Anything else is resolved as a helper if `allow_helpers` is set, and is unknown otherwise.
    ///
    /// # Returns
    ///
    /// The errors the call can evaluate to.
    fn classify_constructor(
        &self,
        path: SymbolPath,
        args: &syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>,
        allow_helpers: bool,
    ) -> ErrorSet {
        if is_upper_camel_case(path.last()) {
            if matches!(path.last(), "Ok" | "Some" | "Err") {
                return ErrorSet::default();
            }
            let error_type = path.parent();
            let inner = only_argument(args)
                .map_or_else(ErrorSet::default, |arg| self.classify_error(arg, false));
            let Some(inner_errors) = inner.into_resolved_errors() else {
                return ErrorSet::single(Spellings::of_error(path, error_type));
            };
            let mut error_set = ErrorSet::default();
            for mut error in inner_errors {
                error.push(path.clone());
                error.error_type.clone_from(&error_type);
                error_set.union(ErrorSet::single(error));
            }
            return error_set;
        }
        if let Some(qualifier) = path.qualifier()
            && is_upper_camel_case(qualifier)
            && !self.fn_item.scope.is_self_name(qualifier)
            && let Some(type_path) = path.parent()
        {
            let mut spellings = Spellings::of_error(type_path.clone(), Some(type_path));
            if let Some(variant_path) =
                SymbolPath::parse(&format!("{qualifier}::{}", upper_camel_case(path.last())))
            {
                spellings.push(variant_path);
            }
            spellings.push(path);
            return ErrorSet::single(spellings);
        }
        self.resolve_helper(&path, allow_helpers)
    }

    /// Resolves a helper one level deep. The helper is a `let` closure binding named `path`, or
    /// else the same-file fns that [`Self::helper_fns`] finds. Nested helper calls inside the
    /// helper count as unknown, and a fn's outcomes that pass on one of its parameters, as
    /// [`PassThrough::passed_param`] decides after peeling conversions, are passed parameters
    /// rather than errors.
    ///
    /// # Returns
    ///
    /// The union of the errors of the helper's tail and `return` expressions, or an unknown set if
    /// `allow_helpers` is unset or no helper is found.
    fn resolve_helper(&self, path: &SymbolPath, allow_helpers: bool) -> ErrorSet {
        if !allow_helpers {
            return ErrorSet::unknown();
        }
        if path.len() == 1
            && let Some(closure_binding) = self.closure_bindings.get(path.last())
        {
            return closure_binding.error_set.clone();
        }
        let helper_fns = self.helper_fns(path);
        if helper_fns.is_empty() {
            return ErrorSet::unknown();
        }
        let mut error_set = ErrorSet::default();
        for helper_fn in helper_fns {
            let pass_through = PassThrough::new(helper_fn.sig, helper_fn.block);
            let mut exit_collector = ExitCollector::default();
            exit_collector.visit_block(helper_fn.block);
            let outcomes = block_tail(helper_fn.block)
                .into_iter()
                .chain(exit_collector.returned)
                .flat_map(pass_through::outcome_leaves);
            for outcome in outcomes {
                match pass_through.passed_param(peel_conversions(outcome)) {
                    Some(param_index) => error_set.pass_param(param_index),
                    None => error_set.union(self.classify_error(outcome, false)),
                }
            }
        }
        error_set
    }

    /// Finds the same-file fns that a call to `path` may call:
    ///
    /// * An unqualified path calls a free fn.
    /// * A path qualified by a name of `Self` calls a fn of the scanned fn's impl or trait.
    /// * A path qualified by another name calls a fn of an impl of that type, or of that trait.
    /// * When no fn fits, every fn named after `path`'s last segment is a candidate.
    ///
    /// # Returns
    ///
    /// The candidate fns.
    fn helper_fns(&self, path: &SymbolPath) -> Vec<HelperFn<'ast>> {
        let helper_fns = self.file_index.helper_fns(path.last());
        let scope = &self.fn_item.scope;
        let fits = |helper_fn: &&HelperFn<'ast>| {
            let owner = helper_fn.owner.map(ToString::to_string);
            match path.qualifier() {
                None => owner.is_none(),
                Some(qualifier) if scope.is_self_name(qualifier) => {
                    owner.is_some_and(|owner| scope.is_self_name(&owner))
                }
                Some(qualifier) => owner.as_deref() == Some(qualifier),
            }
        };
        let fitting: Vec<HelperFn<'ast>> = helper_fns.iter().filter(fits).copied().collect();
        if fitting.is_empty() {
            helper_fns.to_vec()
        } else {
            fitting
        }
    }

    /// Classifies the argument of `.map_err(..)` or `.ok_or_else(..)`.
    ///
    /// # Returns
    ///
    /// How the argument transforms an error.
    fn classify_mapper(&self, mapper: &syn::Expr) -> Mapper {
        match peel_parens(mapper) {
            syn::Expr::Path(expr_path) => {
                let Some(path) = self.symbol(&expr_path.path) else {
                    return Mapper::Unknown {
                        description: "an unknown mapper".to_owned(),
                    };
                };
                let is_conversion = matches!(
                    (path.qualifier(), path.last()),
                    (Some("Into"), "into") | (Some(_), "from") | (Some("Box"), "new")
                );
                if is_conversion {
                    return Mapper::Conversion;
                }
                let description = format!("`{path}`");
                self.classify_constructor(path, &syn::punctuated::Punctuated::new(), true)
                    .into_resolved_errors()
                    .map_or(Mapper::Unknown { description }, |errors| Mapper::Errors {
                        errors,
                    })
            }
            syn::Expr::Closure(closure) => {
                let body = peel_block_tail(&closure.body);
                if let Some(parameter) = closure_parameter(closure)
                    && is_conversion_of(body, &parameter)
                {
                    return Mapper::Conversion;
                }
                self.classify_error(body, true)
                    .into_resolved_errors()
                    .map_or_else(
                        || Mapper::Unknown {
                            description: "a closure".to_owned(),
                        },
                        |errors| Mapper::Errors { errors },
                    )
            }
            _ => Mapper::Unknown {
                description: "an expression".to_owned(),
            },
        }
    }

    /// Records the errors of an `Err(..)` call that leaves the fn at `site`, unless it passes on a
    /// binding whose forwarding is already recorded:
    ///
    /// * The errors its argument classifies to are returned directly. They are converted into the
    ///   fn's own error type when the argument is converted, or when `site` is a `?`.
    /// * The errors that a helper call such as `Err(self.classify(error))` builds are returned
    ///   directly too. The helper returns an error rather than a `Result`, so it can't fail and is
    ///   never a callee to forward.
    /// * A converted value, such as `Err(error.into())`, is an opaque error: it becomes some
    ///   variant of the fn's error type.
    /// * A plain value, such as `Err(error)`, is an opaque source.
    /// * An unknown helper is both, since it may also pass on an error it is given. A helper call
    ///   whose errors are all unknown is also a `HelperError`, of the type that
    ///   [`Self::helper_error_type`] finds.
    /// * A helper that may pass on an argument unchanged forwards the callee whose error the
    ///   argument binds, as [`Self::record_passed_arguments`] describes.
    fn record_err_call(&mut self, call: &syn::ExprCall, site: Site) {
        let Some(position) = err_position(call) else {
            return;
        };
        if self.forwarded_sites.contains(&position) {
            return;
        }
        let Some(arg) = call.args.first() else {
            self.record_opaque_source();
            return;
        };
        let mut error_set = self.classify_error(arg, true);
        let passed_params = std::mem::take(&mut error_set.passed_params);
        self.record_passed_arguments(arg, &passed_params);
        if error_set.errors.is_empty()
            && !error_set.has_unknown
            && error_set.has_value
            && is_conversion(arg)
        {
            self.record_opaque_error();
            return;
        }
        if error_set.has_unknown {
            self.record_opaque_source();
        }
        let converts = is_conversion(arg) || site.is_try();
        if error_set.errors.is_empty()
            && error_set.has_unknown
            && let Some(helper) = self.helper_callee(arg)
        {
            let error_type = self.helper_error_type(arg, converts);
            self.record(
                ObligationKind::HelperError {
                    helper: helper.spellings,
                    error_type,
                },
                position,
            );
        }
        self.record_error_set(error_set, position, converts);
    }

    /// Records the errors that the helper call `helper_call` passes on unchanged, for each input
    /// index in `passed_params`:
    ///
    /// * An argument that an enclosing `Err(binding)` pattern binds forwards the callee whose error
    ///   the pattern binds, such as `foo` for `Err(e) => Err(classify(e))` of `match foo()`.
    /// * Any other argument is an opaque source.
    fn record_passed_arguments(&mut self, helper_call: &syn::Expr, passed_params: &[usize]) {
        for param_index in passed_params {
            let callee = helper_argument(helper_call, *param_index)
                .and_then(|argument| match peel_references(argument) {
                    syn::Expr::Path(expr_path) => expr_path.path.get_ident(),
                    _ => None,
                })
                .and_then(|ident| self.err_binding_callees.get(&ident.to_string()))
                .cloned();
            match callee {
                Some(callee) => self.record_forward(callee, self.return_site()),
                None => self.record_opaque_source(),
            }
        }
    }

    /// # Returns
    ///
    /// The binding of an `Err(binding)` `pat` matched against `scrutinee`, with the callee whose
    /// error it binds, or [`None`] if `pat` isn't such a pattern or `scrutinee` isn't a call the
    /// scan can name.
    fn err_binding_callee(
        &self,
        pat: &syn::Pat,
        scrutinee: &syn::Expr,
    ) -> Option<(String, Callee)> {
        let binding = err_binding(pat)?;
        if !is_call(scrutinee) {
            return None;
        }
        let callee = self.callee_of(scrutinee)?;
        Some((binding, callee))
    }

    /// # Returns
    ///
    /// The bindings of the `Err(binding)` patterns of `expr_if`'s `let` expressions, with the
    /// callees whose errors they bind, as [`Self::err_binding_callee`] finds them.
    fn if_let_err_bindings(&self, expr_if: &syn::ExprIf) -> Vec<(String, Callee)> {
        let_chain(&expr_if.cond)
            .into_iter()
            .filter_map(|expr_let| self.err_binding_callee(&expr_let.pat, &expr_let.expr))
            .collect()
    }

    /// Runs `visit` with `err_bindings` as the callees of their `Err(binding)` patterns, and
    /// restores the earlier callees of the same names afterwards.
    fn with_err_bindings(
        &mut self,
        err_bindings: Vec<(String, Callee)>,
        visit: impl FnOnce(&mut Self),
    ) {
        let shadowed: Vec<(String, Option<Callee>)> = err_bindings
            .into_iter()
            .map(|(binding, callee)| {
                let earlier = self.err_binding_callees.insert(binding.clone(), callee);
                (binding, earlier)
            })
            .collect();
        visit(self);
        for (binding, earlier) in shadowed.into_iter().rev() {
            match earlier {
                Some(callee) => self.err_binding_callees.insert(binding, callee),
                None => self.err_binding_callees.remove(&binding),
            };
        }
    }

    /// Runs `visit` with the bindings of `names` hidden, and restores them afterwards.
    fn with_hidden_bindings(&mut self, names: Vec<String>, visit: impl FnOnce(&mut Self)) {
        let hidden: Vec<(String, NameBindings)> = names
            .into_iter()
            .map(|name| {
                let bindings = self.take_bindings(&name);
                (name, bindings)
            })
            .collect();
        visit(self);
        for (name, bindings) in hidden {
            self.restore_bindings(name, bindings);
        }
    }

    /// Removes the bindings of `name`.
    ///
    /// # Returns
    ///
    /// The removed bindings.
    fn take_bindings(&mut self, name: &str) -> NameBindings {
        NameBindings {
            closure: self.closure_bindings.remove(name),
            error: self.error_bindings.remove(name),
            call: self.call_bindings.remove(name),
            is_spawned: self.spawned_bindings.remove(name),
            builder: self.builder_bindings.remove(name),
            err_binding_callee: self.err_binding_callees.remove(name),
        }
    }

    /// Replaces the bindings of `name` with `bindings`.
    fn restore_bindings(&mut self, name: String, bindings: NameBindings) {
        restore_binding(&mut self.closure_bindings, &name, bindings.closure);
        restore_binding(&mut self.error_bindings, &name, bindings.error);
        restore_binding(&mut self.call_bindings, &name, bindings.call);
        restore_binding(&mut self.builder_bindings, &name, bindings.builder);
        restore_binding(
            &mut self.err_binding_callees,
            &name,
            bindings.err_binding_callee,
        );
        if bindings.is_spawned {
            self.spawned_bindings.insert(name);
        } else {
            self.spawned_bindings.remove(&name);
        }
    }

    /// # Returns
    ///
    /// The callee of `expr`, after peeling conversions, if it calls a helper that builds errors: a
    /// fn path that isn't an error constructor, or a method of `self`.
    fn helper_callee(&self, expr: &syn::Expr) -> Option<Callee> {
        let built = peel_conversions(expr);
        if is_error_constructor(built) {
            return None;
        }
        match built {
            syn::Expr::Call(call) => match peel_parens(&call.func) {
                syn::Expr::Path(expr_path) => self.path_callee(&expr_path.path),
                _ => None,
            },
            syn::Expr::MethodCall(method_call)
                if is_ident_path(peel_parens(&method_call.receiver), "self") =>
            {
                self.callee_of(built)
            }
            _ => None,
        }
    }

    /// Finds the type of the error that the helper call `helper_call` builds: the fn's own error
    /// type if `converts` is set, or else the return type of the same-file helper fns, if they all
    /// have the same one.
    ///
    /// # Returns
    ///
    /// The error type, or [`None`] if it isn't known.
    fn helper_error_type(&self, helper_call: &syn::Expr, converts: bool) -> Option<SymbolPath> {
        if converts {
            return self.fn_item.own_error_type.clone();
        }
        let path = match peel_conversions(helper_call) {
            syn::Expr::Call(call) => match peel_parens(&call.func) {
                syn::Expr::Path(expr_path) => self.symbol(&expr_path.path)?,
                _ => return None,
            },
            syn::Expr::MethodCall(method_call) => {
                SymbolPath::parse(&format!("Self::{}", method_call.method))?
            }
            _ => return None,
        };
        let mut error_types = self.helper_fns(&path).into_iter().map(|helper_fn| {
            let syn::ReturnType::Type(_, return_type) = &helper_fn.sig.output else {
                return None;
            };
            match peel_type_parens(return_type) {
                syn::Type::Path(type_path) => SymbolPath::from_syn_path(&type_path.path),
                _ => None,
            }
        });
        let error_type = error_types.next()??;
        error_types
            .all(|other| other.as_ref() == Some(&error_type))
            .then_some(error_type)
    }

    /// Applies the match-forwarding rule to a `match` on a call:
    ///
    /// * An `Err(binding)` arm forwards the call as [`Self::forward_binding`] describes, or maps
    ///   its error as [`Self::wrap_binding`] describes.
    /// * A catch-all `result => result?` arm forwards the call.
    fn match_forwarding(&mut self, expr_match: &syn::ExprMatch) {
        if !is_call(&expr_match.expr) {
            return;
        }
        for arm in &expr_match.arms {
            if let syn::Pat::Ident(pat_ident) = &arm.pat
                && pat_ident.subpat.is_none()
                && let syn::Expr::Try(expr_try) = peel_parens(&arm.body)
                && is_ident_path(
                    peel_await_and_parens(&expr_try.expr),
                    &pat_ident.ident.to_string(),
                )
            {
                self.classify_fallible(&expr_match.expr, self.return_site());
            }
            if let Some(binding) = err_binding(&arm.pat) {
                self.forward_binding(&expr_match.expr, &binding, |binding| {
                    forwarded_binding_sites(&arm.body, binding)
                });
                if let Some((position, wrapping)) = wrapping_err_call(&arm.body, &binding) {
                    self.wrap_binding(&expr_match.expr, position, wrapping);
                }
            }
        }
    }

    /// Applies the match-forwarding rule to `if let Err(binding) = call`, including an `if let`
    /// inside a `&&` let-chain, as [`Self::forward_binding`] describes.
    fn if_let_forwarding(&mut self, expr_if: &syn::ExprIf) {
        for expr_let in let_chain(&expr_if.cond) {
            if !is_call(&expr_let.expr) {
                continue;
            }
            let Some(binding) = err_binding(&expr_let.pat) else {
                continue;
            };
            self.forward_binding(&expr_let.expr, &binding, |binding| {
                block_forwarded_binding_sites(&expr_if.then_branch, binding)
            });
            let wrapping = expr_if
                .then_branch
                .stmts
                .iter()
                .find_map(|stmt| match stmt {
                    syn::Stmt::Expr(expr, _) => wrapping_err_call(expr, &binding),
                    _ => None,
                });
            if let Some((position, wrapping)) = wrapping {
                self.wrap_binding(&expr_let.expr, position, wrapping);
            }
        }
    }

    /// Forwards `scrutinee` like a tail value when `forwarded_sites` finds the places that pass
    /// `binding` on unchanged, which then record nothing themselves.
    fn forward_binding(
        &mut self,
        scrutinee: &syn::Expr,
        binding: &str,
        forwarded_sites: impl FnOnce(&str) -> Option<Vec<Position>>,
    ) {
        if let Some(sites) = forwarded_sites(binding) {
            self.forwarded_sites.extend(sites);
            self.classify_fallible(scrutinee, self.return_site());
        }
    }

    /// Records that the `Err(wrapping)` at `position` wraps the error of `scrutinee`'s callee, such
    /// as `Err(error) => Err(Error::Cache(error))`. The callee's error is mapped into the errors of
    /// `wrapping`, so either may be documented, and the `Err(..)` records nothing itself.
    fn wrap_binding(&mut self, scrutinee: &syn::Expr, position: Position, wrapping: &syn::Expr) {
        let Some(callee) = self.callee_of(scrutinee) else {
            return;
        };
        let Some(errors) = self.classify_error(wrapping, true).into_resolved_errors() else {
            return;
        };
        self.forwarded_sites.insert(position);
        self.record(
            ObligationKind::Either {
                callee: callee.spellings,
                errors: self.convert(
                    errors,
                    is_conversion(wrapping) || self.return_site().is_try(),
                ),
            },
            position,
        );
    }

    /// # Returns
    ///
    /// The site where a `return` value leaves the fn: the site of the in-place closure being
    /// visited, if any, or else [`Site::Tail`].
    fn return_site(&self) -> Site {
        self.in_place_return_site.unwrap_or(Site::Tail)
    }

    /// Runs `visit` inside a nested scope, where `?` and `return` don't leave the fn.
    fn with_nested_scope(&mut self, visit: impl FnOnce(&mut Self)) {
        let outer_return_site = self.in_place_return_site.take();
        self.nested_scope_depth += 1;
        visit(self);
        self.nested_scope_depth -= 1;
        self.in_place_return_site = outer_return_site;
    }
}

impl<'node> Visit<'node> for BodyScanner<'_, '_> {
    /// Visits a closure as a nested scope, whose value records nested callees only.
    fn visit_expr_closure(&mut self, closure: &'node syn::ExprClosure) {
        self.with_nested_scope(|body_scanner| {
            syn::visit::visit_expr_closure(body_scanner, closure);
            body_scanner.classify_fallible(closure_result(closure), Site::Tail);
        });
    }

    /// Visits an async block as a nested scope, whose value records nested callees only.
    fn visit_expr_async(&mut self, expr_async: &'node syn::ExprAsync) {
        self.with_nested_scope(|body_scanner| {
            syn::visit::visit_expr_async(body_scanner, expr_async);
            body_scanner.classify_block_value(&expr_async.block, Site::Tail);
        });
    }

    fn visit_expr_try_block(&mut self, expr_try_block: &'node syn::ExprTryBlock) {
        self.with_nested_scope(|body_scanner| {
            syn::visit::visit_expr_try_block(body_scanner, expr_try_block);
        });
    }

    /// Nested items are checked by the item walk on their own.
    fn visit_item(&mut self, _item: &'node syn::Item) {}

    /// Patterns are skipped, except for match guards: an `Err(Error::X) => ..` arm neither returns
    /// nor mentions `Error::X`.
    fn visit_pat(&mut self, pat: &'node syn::Pat) {
        if let syn::Pat::Guard(pat_guard) = pat {
            self.visit_expr(&pat_guard.guard);
        }
    }

    /// Records the binding of a closure, of a query builder, of an error constructor or of a call.
    /// A new binding of a name replaces any earlier one.
    fn visit_local(&mut self, local: &'node syn::Local) {
        if let syn::Pat::Ident(pat_ident) = &local.pat
            && let Some(local_init) = &local.init
        {
            let name = pat_ident.ident.to_string();
            self.closure_bindings.remove(&name);
            self.builder_bindings.remove(&name);
            self.error_bindings.remove(&name);
            self.call_bindings.remove(&name);
            self.spawned_bindings.remove(&name);
            let init = peel_parens(&local_init.expr);
            if is_spawn_call(init) {
                self.spawned_bindings.insert(name);
            } else if let syn::Expr::Closure(closure) = init {
                let closure_binding = ClosureBinding {
                    error_set: self.classify_error(&closure.body, false),
                    tail_callee: self.callee_of(closure_result(closure)),
                };
                self.closure_bindings.insert(name, closure_binding);
            } else if let Some(builder_type) = self.builder_type(init) {
                self.builder_bindings.insert(name, builder_type);
            } else if is_error_constructor(init)
                && let Some(errors) = self.classify_error(init, false).into_resolved_errors()
                && !errors.is_empty()
            {
                let error_set = ErrorSet {
                    errors,
                    ..ErrorSet::default()
                };
                self.error_bindings.insert(name, error_set);
            } else if local_init.diverge.is_none()
                && is_call(init)
                && let Some(callee) = self.callee_of(init)
            {
                self.call_bindings.insert(name, callee);
            }
        }
        syn::visit::visit_local(self, local);
    }

    /// Keeps a query builder binding across `query = query.bind(..)`.
    fn visit_expr_assign(&mut self, expr_assign: &'node syn::ExprAssign) {
        if let syn::Expr::Path(expr_path) = peel_parens(&expr_assign.left)
            && let Some(ident) = expr_path.path.get_ident()
            && let Some(builder_type) = self.builder_type(&expr_assign.right)
        {
            self.builder_bindings
                .insert(ident.to_string(), builder_type);
        }
        syn::visit::visit_expr_assign(self, expr_assign);
    }

    /// A `for` loop calls `IntoIterator::into_iter` and `Iterator::next`, so both are mentions.
    fn visit_expr_for_loop(&mut self, expr_for_loop: &'node syn::ExprForLoop) {
        for implicit_call in ["into_iter", "next"] {
            self.facts
                .mentions
                .insert(SymbolPath::single(implicit_call));
        }
        syn::visit::visit_expr_for_loop(self, expr_for_loop);
    }

    fn visit_expr_try(&mut self, expr_try: &'node syn::ExprTry) {
        let question_position = span::start_position(expr_try.question_token.span);
        self.classify_fallible(&expr_try.expr, Site::Try { question_position });
        syn::visit::visit_expr_try(self, expr_try);
    }

    fn visit_expr_return(&mut self, expr_return: &'node syn::ExprReturn) {
        if let Some(returned) = &expr_return.expr {
            self.classify_fallible(returned, self.return_site());
        }
        syn::visit::visit_expr_return(self, expr_return);
    }

    /// Visits each arm with the callee that its `Err(binding)` pattern binds, if any.
    fn visit_expr_match(&mut self, expr_match: &'node syn::ExprMatch) {
        self.match_forwarding(expr_match);
        for attr in &expr_match.attrs {
            self.visit_attribute(attr);
        }
        self.visit_expr(&expr_match.expr);
        for arm in &expr_match.arms {
            let err_bindings = self
                .err_binding_callee(&arm.pat, &expr_match.expr)
                .into_iter()
                .collect();
            self.with_err_bindings(err_bindings, |body_scanner| body_scanner.visit_arm(arm));
        }
    }

    /// Visits the `then` branch with the callees that the `Err(binding)` patterns of the
    /// condition's `let` expressions bind.
    fn visit_expr_if(&mut self, expr_if: &'node syn::ExprIf) {
        self.if_let_forwarding(expr_if);
        for attr in &expr_if.attrs {
            self.visit_attribute(attr);
        }
        self.visit_expr(&expr_if.cond);
        let err_bindings = self.if_let_err_bindings(expr_if);
        self.with_err_bindings(err_bindings, |body_scanner| {
            body_scanner.visit_block(&expr_if.then_branch);
        });
        if let Some((_, else_branch)) = &expr_if.else_branch {
            self.visit_expr(else_branch);
        }
    }

    /// Records the path as a mention, and the type `X` of an `X::from` path as a conversion target.
    fn visit_expr_path(&mut self, expr_path: &'node syn::ExprPath) {
        if let Some(path) = self.symbol(&expr_path.path) {
            if path.last() == "from"
                && let Some(qualifier) = path.qualifier()
                && is_upper_camel_case(qualifier)
                && !self.fn_item.scope.is_self_name(qualifier)
            {
                self.facts.conversion_targets.insert(qualifier.to_owned());
            }
            self.facts.mentions.insert(path);
        }
        syn::visit::visit_expr_path(self, expr_path);
    }

    fn visit_expr_method_call(&mut self, method_call: &'node syn::ExprMethodCall) {
        self.facts.mentions.insert(self.method_path(method_call));
        syn::visit::visit_expr_method_call(self, method_call);
    }

    fn visit_expr_struct(&mut self, expr_struct: &'node syn::ExprStruct) {
        if let Some(path) = self.symbol(&expr_struct.path) {
            self.facts.mentions.insert(path);
        }
        syn::visit::visit_expr_struct(self, expr_struct);
    }

    fn visit_macro(&mut self, mac: &'node syn::Macro) {
        let macro_kind = macros::kind(mac);
        if macro_kind == MacroKind::Anyhow {
            self.facts.mentions.insert(SymbolPath::anyhow_error());
            let position = macro_position(mac);
            if !is_anyhow_constructor(mac) && !self.forwarded_sites.contains(&position) {
                self.record(
                    ObligationKind::Direct {
                        error: anyhow_error_spellings(),
                    },
                    position,
                );
            }
        }
        let Some(macro_body) = macros::parse_body(mac, macro_kind) else {
            for ident in macros::identifiers(&mac.tokens) {
                self.facts
                    .mentions
                    .insert(SymbolPath::single(&ident.to_string()));
            }
            if macros::has_try_operator(&mac.tokens) {
                self.record_opaque_source();
            }
            return;
        };
        let visit_body = |body_scanner: &mut Self| {
            for expr in macro_body.exprs.iter().chain(&macro_body.arm_bodies) {
                body_scanner.visit_expr(expr);
            }
            for stmt in &macro_body.stmts {
                body_scanner.visit_stmt(stmt);
            }
        };
        if macro_kind == MacroKind::Generator {
            self.with_nested_scope(visit_body);
        } else {
            visit_body(self);
        }
    }
}

/// Looks for a plain path with a given name.
///
/// # Type Parameters
///
/// * `'name` - The lifetime of the name looked for.
struct IdentFinder<'name> {
    name: &'name str,
    is_found: bool,
}

impl<'node> Visit<'node> for IdentFinder<'_> {
    fn visit_expr_path(&mut self, expr_path: &'node syn::ExprPath) {
        self.is_found |= expr_path.qself.is_none() && expr_path.path.is_ident(self.name);
    }
}

/// # Returns
///
/// The tail expression of `block`: its last statement if that is an expression without `;`.
fn block_tail(block: &syn::Block) -> Option<&syn::Expr> {
    match block.stmts.last() {
        Some(syn::Stmt::Expr(expr, None)) => Some(expr),
        _ => None,
    }
}

/// # Returns
///
/// `expr` without parentheses and invisible groups.
fn peel_parens(mut expr: &syn::Expr) -> &syn::Expr {
    loop {
        expr = match expr {
            syn::Expr::Paren(expr_paren) => &expr_paren.expr,
            syn::Expr::Group(expr_group) => &expr_group.expr,
            _ => return expr,
        };
    }
}

/// # Returns
///
/// `expr` without parentheses, invisible groups and `.await`.
fn peel_await_and_parens(mut expr: &syn::Expr) -> &syn::Expr {
    loop {
        expr = match peel_parens(expr) {
            syn::Expr::Await(expr_await) => &expr_await.base,
            peeled => return peeled,
        };
    }
}

/// # Returns
///
/// `expr` without `.await`, parentheses and the error adaptors that [`is_error_adaptor`] matches,
/// such as `.map(..)` and `.context(..)`.
fn peel_error_adaptors(mut expr: &syn::Expr) -> &syn::Expr {
    loop {
        expr = match peel_await_and_parens(expr) {
            syn::Expr::MethodCall(method_call)
                if is_error_adaptor(&method_call.method.to_string()) =>
            {
                &method_call.receiver
            }
            peeled => return peeled,
        };
    }
}

/// # Returns
///
/// The expression at the root of a method chain: `expr` without `.await`, parentheses and method
/// calls, so `timeout(d, f).await.map_err(g)` gives `timeout(d, f)`.
fn chain_root(mut expr: &syn::Expr) -> &syn::Expr {
    loop {
        expr = match peel_await_and_parens(expr) {
            syn::Expr::MethodCall(method_call) => &method_call.receiver,
            peeled => return peeled,
        };
    }
}

/// # Returns
///
/// `expr` without parentheses, `.await`, `.into()`, `Into::into(x)`, `X::from(x)` and
/// `Box::new(x)`.
fn peel_conversions(mut expr: &syn::Expr) -> &syn::Expr {
    loop {
        let peeled = peel_await_and_parens(expr);
        match conversion_operand(peeled) {
            Some(operand) => expr = operand,
            None => return peeled,
        }
    }
}

/// # Returns
///
/// `expr` without parentheses and without the blocks around a tail expression.
fn peel_block_tail(mut expr: &syn::Expr) -> &syn::Expr {
    loop {
        expr = match peel_parens(expr) {
            syn::Expr::Block(expr_block) => match block_tail(&expr_block.block) {
                Some(tail) => tail,
                None => return expr,
            },
            peeled => return peeled,
        };
    }
}

/// # Returns
///
/// The body of `expr` if it is a closure, or `expr` itself otherwise.
fn closure_value(expr: &syn::Expr) -> &syn::Expr {
    match peel_parens(expr) {
        syn::Expr::Closure(closure) => &closure.body,
        _ => expr,
    }
}

/// # Returns
///
/// The expression whose value a closure returns: its body's tail, or the tail of an `async` block
/// that its body is.
fn closure_result(closure: &syn::ExprClosure) -> &syn::Expr {
    let value = peel_block_tail(&closure.body);
    match value {
        syn::Expr::Async(expr_async) => {
            block_tail(&expr_async.block).map_or(value, peel_block_tail)
        }
        _ => value,
    }
}

/// # Returns
///
/// The method names of the receiver chain `expr`, innermost last, without the call at its root.
fn chain_methods(mut expr: &syn::Expr) -> Vec<SymbolPath> {
    let mut chain_methods = Vec::new();
    while let syn::Expr::MethodCall(method_call) = peel_await_and_parens(expr) {
        chain_methods.push(SymbolPath::single(&method_call.method.to_string()));
        expr = &method_call.receiver;
    }
    chain_methods
}

/// # Returns
///
/// `expr` without parentheses, invisible groups and `&` or `&mut` references.
fn peel_references(mut expr: &syn::Expr) -> &syn::Expr {
    loop {
        expr = match peel_parens(expr) {
            syn::Expr::Reference(expr_reference) => &expr_reference.expr,
            peeled => return peeled,
        };
    }
}

/// # Returns
///
/// The only argument of `args`, or [`None`] if there are none or several.
fn only_argument(
    args: &syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>,
) -> Option<&syn::Expr> {
    let mut args = args.iter();
    match (args.next(), args.next()) {
        (Some(arg), None) => Some(arg),
        _ => None,
    }
}

/// # Returns
///
/// Whether `method` returns a value taken from its receiver or its arguments, such as `.expect(..)`
/// or `.map_or(..)`, instead of a callee's result.
fn is_value_method(method: &str) -> bool {
    known_apis::RECEIVER_VALUE_METHODS.contains(&method)
        || known_apis::ARGUMENT_VALUE_METHODS.contains(&method)
}

/// # Returns
///
/// Whether `method` passes its receiver's error on, as `.map(..)` does, or maps it, as
/// `.map_err(..)`, `.context(..)` and `.with_context(..)` do.
fn is_error_adaptor(method: &str) -> bool {
    matches!(method, "map_err" | "context" | "with_context")
        || known_apis::PASS_THROUGH_METHODS.contains(&method)
}

/// # Returns
///
/// Whether `expr` is an awaited `spawn(..)` or `spawn_blocking(..)` call or method call, directly
/// or as the value of a closure that runs in place, such as `(|| tokio::spawn(..))().await`.
fn is_awaited_spawn(expr: &syn::Expr) -> bool {
    let syn::Expr::Await(expr_await) = peel_parens(expr) else {
        return false;
    };
    let mut awaited = &*expr_await.base;
    while let syn::Expr::Call(call) = peel_parens(awaited)
        && let Some(closure) = in_place_closure(call)
    {
        awaited = peel_block_tail(&closure.body);
    }
    is_spawn_call(awaited)
}

/// # Returns
///
/// Whether `expr` is a `spawn(..)` or `spawn_blocking(..)` call or method call.
fn is_spawn_call(expr: &syn::Expr) -> bool {
    match peel_parens(expr) {
        syn::Expr::Call(call) => {
            call_path_last(call).is_some_and(|last| known_apis::SPAWN_FNS.contains(&last.as_str()))
        }
        syn::Expr::MethodCall(method_call) => {
            known_apis::SPAWN_FNS.contains(&method_call.method.to_string().as_str())
        }
        _ => false,
    }
}

/// # Returns
///
/// The closure that `call` runs in place and whose value is the call's value, or [`None`] if there
/// is none, or if it is async or its body is an async block. A closure runs in place when it is
/// called on the spot, such as `(|| body)()`, or is the only argument of a fn that
/// [`known_apis::IN_PLACE_FNS`] lists.
fn in_place_closure(call: &syn::ExprCall) -> Option<&syn::ExprClosure> {
    let closure = match peel_parens(&call.func) {
        syn::Expr::Closure(closure) => closure,
        _ if call_path_last(call)
            .is_some_and(|last| known_apis::IN_PLACE_FNS.contains(&last.as_str())) =>
        {
            match peel_parens(only_argument(&call.args)?) {
                syn::Expr::Closure(closure) => closure,
                _ => return None,
            }
        }
        _ => return None,
    };
    let is_async = closure.asyncness.is_some()
        || matches!(peel_block_tail(&closure.body), syn::Expr::Async(_));
    (!is_async).then_some(closure)
}

/// # Returns
///
/// The converted operand `x` if `expr` is `x.into()`, `Into::into(x)`, `X::from(x)` or
/// `Box::new(x)`, or [`None`] otherwise.
fn conversion_operand(expr: &syn::Expr) -> Option<&syn::Expr> {
    match expr {
        syn::Expr::MethodCall(method_call)
            if method_call.method == "into" && method_call.args.is_empty() =>
        {
            Some(&method_call.receiver)
        }
        syn::Expr::Call(call) if is_conversion_fn(&call.func) => only_argument(&call.args),
        _ => None,
    }
}

/// # Returns
///
/// Whether `expr` is `x.into()`, `Into::into(x)`, `X::from(x)` or `Box::new(x)`.
fn is_conversion(expr: &syn::Expr) -> bool {
    conversion_operand(peel_parens(expr)).is_some()
}

/// # Returns
///
/// Whether `func` is `Into::into`, `X::from` or `Box::new`.
fn is_conversion_fn(func: &syn::Expr) -> bool {
    let syn::Expr::Path(expr_path) = peel_parens(func) else {
        return false;
    };
    let segments: Vec<String> = expr_path
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    match segments.as_slice() {
        [.., qualifier, last] => {
            (qualifier == "Into" && last == "into")
                || last == "from"
                || (qualifier == "Box" && last == "new")
        }
        _ => false,
    }
}

/// # Returns
///
/// Whether `expr` is `name`, `name.into()`, `X::from(name)` or `Into::into(name)`.
fn is_conversion_of(expr: &syn::Expr, name: &str) -> bool {
    is_ident_path(peel_conversions(expr), name)
}

/// # Returns
///
/// Whether `expr` is the plain path `name`.
fn is_ident_path(expr: &syn::Expr, name: &str) -> bool {
    matches!(
        expr,
        syn::Expr::Path(expr_path) if expr_path.qself.is_none() && expr_path.path.is_ident(name)
    )
}

/// # Returns
///
/// Whether `mac` is `anyhow!` or `format_err!`, which build an `anyhow::Error` value.
fn is_anyhow_constructor(mac: &syn::Macro) -> bool {
    macros::kind(mac) == MacroKind::Anyhow
        && mac
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "anyhow" || segment.ident == "format_err")
}

/// # Returns
///
/// The name of a closure's only parameter, if it is a plain identifier pattern.
fn closure_parameter(closure: &syn::ExprClosure) -> Option<String> {
    if closure.inputs.len() != 1 {
        return None;
    }
    let mut pat = closure.inputs.first()?;
    if let syn::Pat::Type(pat_type) = pat {
        pat = &pat_type.pat;
    }
    match pat {
        syn::Pat::Ident(pat_ident) if pat_ident.subpat.is_none() => {
            Some(pat_ident.ident.to_string())
        }
        _ => None,
    }
}

/// # Returns
///
/// The names that `closure`'s identifier parameter patterns bind.
fn closure_parameters(closure: &syn::ExprClosure) -> Vec<String> {
    closure
        .inputs
        .iter()
        .filter_map(|pat| {
            let pat = match pat {
                syn::Pat::Type(pat_type) => &*pat_type.pat,
                pat => pat,
            };
            match pat {
                syn::Pat::Ident(pat_ident) => Some(pat_ident.ident.to_string()),
                _ => None,
            }
        })
        .collect()
}

/// Sets the binding of `name` in `bindings` to `binding`, or removes it if `binding` is [`None`].
///
/// # Type Parameters
///
/// * `BindingType` - The type of what a binding records.
fn restore_binding<BindingType>(
    bindings: &mut HashMap<String, BindingType>,
    name: &str,
    binding: Option<BindingType>,
) {
    match binding {
        Some(binding) => {
            bindings.insert(name.to_owned(), binding);
        }
        None => {
            bindings.remove(name);
        }
    }
}

/// # Returns
///
/// The last segment of `call`'s callee path, or [`None`] if the callee isn't a path.
fn call_path_last(call: &syn::ExprCall) -> Option<String> {
    match peel_parens(&call.func) {
        syn::Expr::Path(expr_path) => expr_path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        _ => None,
    }
}

/// # Returns
///
/// Whether `call` is `Err(..)` or `Result::Err(..)`.
fn is_err_call(call: &syn::ExprCall) -> bool {
    let syn::Expr::Path(expr_path) = peel_parens(&call.func) else {
        return false;
    };
    let segments = &expr_path.path.segments;
    segments
        .last()
        .is_some_and(|segment| segment.ident == "Err")
        && (segments.len() == 1
            || segments
                .iter()
                .rev()
                .nth(1)
                .is_some_and(|segment| segment.ident == "Result"))
}

/// # Returns
///
/// Whether `call` is an `Ok(..)` or `Some(..)` call, which returns no error.
fn is_ok_or_some_call(call: &syn::ExprCall) -> bool {
    call_path_last(call).is_some_and(|last| last == "Ok" || last == "Some")
}

/// # Returns
///
/// The position of the `Err` identifier of an `Err(..)` call.
fn err_position(call: &syn::ExprCall) -> Option<Position> {
    match peel_parens(&call.func) {
        syn::Expr::Path(expr_path) => expr_path
            .path
            .segments
            .last()
            .map(|segment| span::start_position(segment.ident.span())),
        _ => None,
    }
}

/// # Returns
///
/// The position of the first segment of a macro's path.
fn macro_position(mac: &syn::Macro) -> Position {
    mac.path.segments.first().map_or_else(
        || span::start_position(mac.path.span()),
        |segment| span::start_position(segment.ident.span()),
    )
}

/// # Returns
///
/// The spellings of an `anyhow::Error` value.
fn anyhow_error_spellings() -> Spellings {
    Spellings::of_error(SymbolPath::anyhow_error(), Some(SymbolPath::anyhow_error()))
}

/// # Returns
///
/// `snake_case` converted to `SnakeCase`.
fn upper_camel_case(snake_case: &str) -> String {
    snake_case
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect()
}

/// # Returns
///
/// Whether `expr` is a call or a method call, after peeling `.await` and parentheses.
fn is_call(expr: &syn::Expr) -> bool {
    matches!(
        peel_await_and_parens(expr),
        syn::Expr::Call(_) | syn::Expr::MethodCall(_)
    )
}

/// # Returns
///
/// The argument of the helper call `helper_call`, after peeling conversions, that the helper's
/// input at `input_index` binds. A method call's receiver is input 0.
fn helper_argument(helper_call: &syn::Expr, input_index: usize) -> Option<&syn::Expr> {
    match peel_conversions(helper_call) {
        syn::Expr::Call(call) => call.args.iter().nth(input_index),
        syn::Expr::MethodCall(method_call) => {
            method_call.args.iter().nth(input_index.checked_sub(1)?)
        }
        _ => None,
    }
}

/// # Returns
///
/// Whether `expr` builds an error in place, after peeling conversions: a path, call or struct
/// literal whose last path segment is in upper camel case, a call `T::ctor(..)` of an upper camel
/// case `T`, or `anyhow!(..)`.
fn is_error_constructor(expr: &syn::Expr) -> bool {
    let path = match peel_conversions(expr) {
        syn::Expr::Path(expr_path) => &expr_path.path,
        syn::Expr::Struct(expr_struct) => &expr_struct.path,
        syn::Expr::Call(call) => match peel_parens(&call.func) {
            syn::Expr::Path(expr_path) => &expr_path.path,
            _ => return false,
        },
        syn::Expr::Macro(expr_macro) => return is_anyhow_constructor(&expr_macro.mac),
        _ => return false,
    };
    let mut segments = path
        .segments
        .iter()
        .rev()
        .map(|segment| segment.ident.to_string());
    let (Some(last), qualifier) = (segments.next(), segments.next()) else {
        return false;
    };
    !matches!(last.as_str(), "Ok" | "Some" | "Err" | "None")
        && (is_upper_camel_case(&last)
            || qualifier.is_some_and(|qualifier| is_upper_camel_case(&qualifier)))
}

/// Finds an `Err(wrapping)` call in `expr` that builds an error around `binding`, such as
/// `Err(Error::Cache(error))` or `return Err(Error::Db { source: error })`, looking into `return`
/// and block statements.
///
/// # Type Parameters
///
/// * `'expr` - The lifetime of `expr`.
///
/// # Returns
///
/// The position of the `Err` identifier and the argument `wrapping`, or [`None`] if there is no
/// such call.
fn wrapping_err_call<'expr>(
    expr: &'expr syn::Expr,
    binding: &str,
) -> Option<(Position, &'expr syn::Expr)> {
    match peel_parens(expr) {
        syn::Expr::Call(call) if is_err_call(call) => {
            let wrapping = call.args.first()?;
            let wraps = match peel_conversions(wrapping) {
                syn::Expr::Call(built) => {
                    call_path_last(built).is_some_and(|last| is_upper_camel_case(&last))
                        && built.args.iter().any(|arg| mentions_ident(arg, binding))
                }
                syn::Expr::Struct(built) => built
                    .fields
                    .iter()
                    .any(|field| mentions_ident(&field.expr, binding)),
                _ => false,
            };
            if wraps {
                Some((err_position(call)?, wrapping))
            } else {
                None
            }
        }
        syn::Expr::Return(expr_return) => wrapping_err_call(expr_return.expr.as_ref()?, binding),
        syn::Expr::Block(expr_block) => expr_block.block.stmts.iter().find_map(|stmt| match stmt {
            syn::Stmt::Expr(expr, _) => wrapping_err_call(expr, binding),
            _ => None,
        }),
        _ => None,
    }
}

/// # Returns
///
/// Whether `expr` contains the plain path `name`.
fn mentions_ident(expr: &syn::Expr, name: &str) -> bool {
    let mut ident_finder = IdentFinder {
        name,
        is_found: false,
    };
    ident_finder.visit_expr(expr);
    ident_finder.is_found
}

/// # Returns
///
/// The name bound by an `Err(binding)` pattern, or [`None`] for any other pattern.
fn err_binding(pat: &syn::Pat) -> Option<String> {
    let syn::Pat::TupleStruct(pat_tuple_struct) = pat else {
        return None;
    };
    let is_err_path = pat_tuple_struct
        .path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Err");
    if !is_err_path || pat_tuple_struct.elems.len() != 1 {
        return None;
    }
    match pat_tuple_struct.elems.first()? {
        syn::Pat::Ident(pat_ident) if pat_ident.subpat.is_none() => {
            Some(pat_ident.ident.to_string())
        }
        _ => None,
    }
}

/// # Returns
///
/// The `let` expressions of an `if` condition, looking through `&&` let-chains.
fn let_chain(cond: &syn::Expr) -> Vec<&syn::ExprLet> {
    match peel_parens(cond) {
        syn::Expr::Let(expr_let) => vec![expr_let],
        syn::Expr::Binary(expr_binary) if matches!(expr_binary.op, syn::BinOp::And(_)) => {
            let mut found = let_chain(&expr_binary.left);
            found.extend(let_chain(&expr_binary.right));
            found
        }
        _ => Vec::new(),
    }
}

/// Decides whether `expr` passes `binding` on unchanged.
///
/// * `Err(binding)`, `Err(binding.into())`, `Err(X::from(binding))` and `Err(anyhow!(binding))`
///   pass it on.
/// * So do `bail!(binding)`, `return` of any of these, and a block with such a statement.
///
/// # Returns
///
/// The positions of the `Err` calls and anyhow macros that pass the binding on, or [`None`] if
/// `expr` doesn't pass it on.
fn forwarded_binding_sites(expr: &syn::Expr, binding: &str) -> Option<Vec<Position>> {
    match peel_parens(expr) {
        syn::Expr::Call(call) if is_err_call(call) => {
            let arg = peel_conversions(call.args.first()?);
            let mut sites = vec![err_position(call)?];
            if is_ident_path(arg, binding) {
                return Some(sites);
            }
            if let syn::Expr::Macro(expr_macro) = arg
                && is_anyhow_macro_of(&expr_macro.mac, "anyhow", binding)
            {
                sites.push(macro_position(&expr_macro.mac));
                return Some(sites);
            }
            None
        }
        syn::Expr::Return(expr_return) => {
            forwarded_binding_sites(expr_return.expr.as_ref()?, binding)
        }
        syn::Expr::Macro(expr_macro) => macro_forwarded_binding_site(&expr_macro.mac, binding),
        syn::Expr::Block(expr_block) => block_forwarded_binding_sites(&expr_block.block, binding),
        _ => None,
    }
}

/// # Returns
///
/// The sites of the first statement of `block` that passes `binding` on, as in
/// [`forwarded_binding_sites`].
fn block_forwarded_binding_sites(block: &syn::Block, binding: &str) -> Option<Vec<Position>> {
    block.stmts.iter().find_map(|stmt| match stmt {
        syn::Stmt::Expr(expr, _) => forwarded_binding_sites(expr, binding),
        syn::Stmt::Macro(stmt_macro) => macro_forwarded_binding_site(&stmt_macro.mac, binding),
        syn::Stmt::Local(_) | syn::Stmt::Item(_) => None,
    })
}

/// # Returns
///
/// The position of `mac` if it is `bail!(binding)`.
fn macro_forwarded_binding_site(mac: &syn::Macro, binding: &str) -> Option<Vec<Position>> {
    is_anyhow_macro_of(mac, "bail", binding).then(|| vec![macro_position(mac)])
}

/// # Returns
///
/// Whether `mac` is the anyhow macro `name` invoked with `binding` as its only token.
fn is_anyhow_macro_of(mac: &syn::Macro, name: &str, binding: &str) -> bool {
    let token_trees: Vec<TokenTree> = mac.tokens.clone().into_iter().collect();
    macros::kind(mac) == MacroKind::Anyhow
        && mac
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == name)
        && matches!(token_trees.as_slice(), [TokenTree::Ident(ident)] if ident == binding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::errors_doc::items;

    /// Scans the `Result` fn named `subject` in `source_text`.
    ///
    /// # Returns
    ///
    /// The fn's body facts.
    ///
    /// # Panics
    ///
    /// Panics if `source_text` doesn't parse or has no `Result` fn named `subject`.
    fn facts_of(source_text: &str) -> BodyFacts {
        let syntax_tree = syn::parse_file(source_text).expect("valid file");
        let file_index = FileIndex::build(&syntax_tree);
        let fn_item = items::collect(&syntax_tree, false)
            .into_iter()
            .find(|fn_item| fn_item.name == "subject")
            .expect("a `Result` fn named `subject`");
        scan(&fn_item, &file_index)
    }

    fn spelled(spellings: &Spellings) -> String {
        spellings
            .paths()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("|")
    }

    fn obligations_of(source_text: &str) -> Vec<String> {
        facts_of(source_text)
            .obligations
            .iter()
            .map(|obligation| match &obligation.kind {
                ObligationKind::Direct { error } => format!("direct {}", spelled(error)),
                ObligationKind::Forward { callee } => format!("forward {}", spelled(callee)),
                ObligationKind::Either { callee, errors } => format!(
                    "either {} {}",
                    spelled(callee),
                    errors.iter().map(spelled).collect::<Vec<_>>().join(",")
                ),
                ObligationKind::EitherUnknown { callee, .. } => {
                    format!("either-unknown {}", spelled(callee))
                }
                ObligationKind::HelperError { helper, error_type } => format!(
                    "helper-error {} {}",
                    spelled(helper),
                    error_type
                        .as_ref()
                        .map_or_else(|| "?".to_owned(), ToString::to_string)
                ),
                ObligationKind::HigherOrder {
                    callee,
                    closure_errors,
                } => format!(
                    "higher-order {} {}",
                    spelled(callee),
                    closure_errors
                        .iter()
                        .map(spelled)
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            })
            .collect()
    }

    fn subject(body: &str) -> String {
        format!("fn subject() -> Result<(), Error> {{\n{body}\n}}")
    }

    #[test]
    fn constructed_errors_are_direct() {
        let source_text = subject(
            "if a { return Err(Error::A); }\nif b { return Err(Error::B(1)); }\nif c { \
             Err(Error::C { code: 1 })?; }\nErr(Error::D.into())",
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "direct Error::A",
                "direct Error::B",
                "direct Error::C",
                "direct Error::D"
            ]
        );
    }

    #[test]
    fn ok_or_returns_its_argument_and_not_its_receiver() {
        let source_text =
            subject("self.find().ok_or(Error::A)?;\nlookup().ok_or_else(|| Error::B(1))?;\nOk(())");
        assert_eq!(
            obligations_of(&source_text),
            ["direct Error::A", "direct Error::B"]
        );
    }

    #[test]
    fn question_marks_on_calls_forward_their_callee() {
        let source_text = subject(
            "foo()?;\nbar().await?;\nself.baz()?;\nType::qux()?;\nquux().map(|x| x)?;\nOk(())",
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "forward foo",
                "forward bar",
                "forward baz",
                "forward Type::qux",
                "forward quux"
            ]
        );
    }

    #[test]
    fn map_err_conversions_forward_and_constructors_map() {
        let source_text = subject(
            &[
                "a().map_err(Into::into)?;",
                "b().map_err(Error::from)?;",
                "c().map_err(Error::Wrap)?;",
                "d().map_err(|e| Error::Wrap(e.to_string()))?;",
                "e().map_err(|e| e.into())?;",
                "Ok(())",
            ]
            .join("\n"),
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "forward a",
                "forward b",
                "either c Error::Wrap",
                "either d Error::Wrap",
                "forward e"
            ]
        );
    }

    #[test]
    fn helpers_resolve_one_level_deep() {
        let source_text = format!(
            "fn to_transport_error(e: E) -> SchedulerError {{ SchedulerError::Transport(e) }}\nfn \
             unsupported_type_error() -> Error {{ if x {{ return Error::Unsupported; }} \
             Error::Other }}\n{}",
            subject(
                "a().map_err(to_transport_error)?;\nlet to_status = |e| \
                 Status::internal(e);\nb().map_err(to_status)?;\nif x { return \
                 Err(unsupported_type_error()); }\nc().map_err(unknown_helper)?;\nOk(())"
            )
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "either a SchedulerError::Transport",
                "either b Status|Status::Internal|Status::internal",
                "direct Error::Other",
                "direct Error::Unsupported",
                "either-unknown c",
            ]
        );
        assert!(facts_of(&source_text).has_opaque_error);
    }

    #[test]
    fn anyhow_context_and_macros() {
        let source_text = subject(
            "a().context(\"a\")?;\nvalue.context(\"v\")?;\nensure!(ok, \"not ok\");\nlet error = \
             anyhow!(\"built\");\nbail!(\"failed\")",
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "either a anyhow::Error",
                "direct anyhow::Error",
                "direct anyhow::Error"
            ]
        );
        assert!(facts_of(&source_text).has_opaque_source);
    }

    #[test]
    fn anyhow_macros_in_a_return_position_are_direct() {
        let source_text = subject(
            "if a { return Err(anyhow!(\"a\")); }\nlookup().ok_or_else(|| \
             anyhow::anyhow!(\"b\"))?;\nOk(())",
        );
        assert_eq!(
            obligations_of(&source_text),
            ["direct anyhow::Error", "direct anyhow::Error"]
        );
    }

    #[test]
    fn block_on_and_timeout_see_through_to_the_inner_call() {
        let source_text = subject(
            "runtime.block_on(provider.provide()).context(\"c\")?;\ntimeout(d, \
             self.f()).await.map_err(|_| Error::Timeout)??;\nOk(())",
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "either provide anyhow::Error",
                "forward f",
                "either timeout Error::Timeout"
            ]
        );
        assert!(!facts_of(&source_text).has_opaque_source);
    }

    #[test]
    fn variables_fields_handles_and_sinks_are_opaque_sources() {
        for body in [
            "value?;\nOk(())",
            "self.field?;\nOk(())",
            "handle.await??;\nOk(())",
            "(self.callback)(x)?;\nOk(())",
            "some_macro!(x)?;\nOk(())",
            "r.into()",
            "items.collect::<Result<Vec<_>, _>>()?;\nOk(())",
            "futures::future::try_join_all(futures).await?;\nOk(())",
            "Err(error)",
            "let r = value;\nr",
        ] {
            let facts = facts_of(&subject(body));
            assert_eq!(facts.obligations, [], "obligations of `{body}`");
            assert!(facts.has_opaque_source, "opaque source of `{body}`");
        }
    }

    #[test]
    fn spawned_handles_and_converted_variables_are_opaque_errors() {
        for body in [
            "let x = tokio::task::spawn_blocking(f).await??;\nOk(())",
            "(|| tokio::spawn(load()))().await??;\nOk(())",
            "Err(error.into())",
            "Err(Error::from(error))",
        ] {
            let facts = facts_of(&subject(body));
            assert_eq!(facts.obligations, [], "obligations of `{body}`");
            assert!(facts.has_opaque_error, "opaque error of `{body}`");
        }
    }

    #[test]
    fn recursive_calls_forward_nothing() {
        let source_text = subject(
            "subject()?;\nSelf::subject()?;\nself.subject()?;\nself.inner.subject()?;\nOk(())",
        );
        assert_eq!(obligations_of(&source_text), ["forward subject"]);
    }

    #[test]
    fn tail_calls_and_tail_matches_forward() {
        assert_eq!(
            obligations_of(&subject("self.inner.do_it().await")),
            ["forward do_it"]
        );
        assert_eq!(
            obligations_of(&subject(
                "match kind { A => first(), B => Ok(()), _ => second() }"
            )),
            ["forward first", "forward second"]
        );
        assert_eq!(
            obligations_of(&subject("table.get(&id).map_or(Err(Error::A), |_| Ok(()))")),
            ["direct Error::A"]
        );
        assert_eq!(
            obligations_of(&subject("handle.await.expect(\"joined\")")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn match_forwarding_passes_the_binding_on() {
        let source_text = subject(
            "match self.start(id).await {\nOk(_) | Err(Error::Invalid(_)) => {}\nErr(error) => \
             return Err(error.into()),\n}\nif let Err(e) = process(x).await && !stale(&e) \
             {\nbail!(e);\n}\nmatch other() { Err(e) => Err(Error::Wrap(e)), Ok(v) => Ok(v) }",
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "forward start",
                "forward process",
                "either other Error::Wrap"
            ]
        );
        let facts = facts_of(&source_text);
        assert!(!facts.has_opaque_source && !facts.has_opaque_error);
    }

    #[test]
    fn nested_scopes_do_not_return_from_the_fn() {
        let source_text = subject(
            "let f = || foo()?;\nlet g = async { bar()?; Err(Error::A) };\nlet s = stream! { \
             baz()?; };\nfn inner() -> Result<(), E> { qux()?; Ok(()) }\nOk(())",
        );
        let facts = facts_of(&source_text);
        assert_eq!(facts.obligations, []);
        for mention in ["foo", "bar", "baz", "Error::A"] {
            let path = SymbolPath::parse(mention).expect("valid path");
            assert!(facts.mentions.contains(&path), "mention `{mention}`");
        }
        let nested_fn_call = SymbolPath::parse("qux").expect("valid path");
        assert!(!facts.mentions.contains(&nested_fn_call));
    }

    #[test]
    fn patterns_are_not_mentions() {
        let facts = facts_of(&subject("match r { Err(Error::X) => {} _ => {} }\nOk(())"));
        let pattern_path = SymbolPath::parse("Error::X").expect("valid path");
        assert!(!facts.mentions.contains(&pattern_path));
    }

    #[test]
    fn macro_bodies_are_scanned_by_kind() {
        let source_text = subject(
            "tokio::select! {\nbiased;\nSome(m) = rx.recv(), if open => { self.handle(m).await?; \
             }\nelse => {}\n}\nif matches!(r, Err(Error::Y)) {}\nassert_eq!(r, \
             Err(Error::Z));\nassert!(check()?);\nlet v = vec![foo()?];\njson!({ \"k\": hidden \
             });\nOk(())",
        );
        assert_eq!(
            obligations_of(&source_text),
            ["forward handle", "forward check", "forward foo"]
        );
        let hidden = SymbolPath::parse("hidden").expect("valid path");
        assert!(facts_of(&source_text).mentions.contains(&hidden));
    }

    #[test]
    fn wrapped_variants_constructors_and_builder_chains_have_alternative_spellings() {
        let source_text = subject(
            "if a { return Err(Outer::Wrap(Inner::Leaf)); }\nif b { return \
             Err(Error::not_found(id)); \
             }\ncollection.find(filter).with_options(options).await?;\nOk(())",
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "direct Inner::Leaf|Outer::Wrap",
                "direct Error|Error::NotFound|Error::not_found",
                "forward with_options|find"
            ]
        );
    }

    #[test]
    fn err_values_that_do_not_leave_the_fn_are_not_direct() {
        let source_text = subject(
            "tx.send(Err(Error::Cancelled));\nresults.push(Err(Error::Timeout));\nif value == \
             Err(Error::NotFound) {}\nlet r: Result<(), Error> = Err(Error::Bound);\nOk(())",
        );
        assert_eq!(obligations_of(&source_text), Vec::<String>::new());
    }

    #[test]
    fn calls_of_closure_bindings_forward_the_closure_tail() {
        let source_text = subject("let fetch = |key| load(key);\nfetch(1)?;\nOk(())");
        assert_eq!(obligations_of(&source_text), ["forward load"]);
        let source_text = subject("let fetch = |key| key + 1;\nfetch(1)?;\nOk(())");
        assert_eq!(obligations_of(&source_text), Vec::<String>::new());
        assert!(facts_of(&source_text).has_opaque_source);
    }

    #[test]
    fn unqualified_calls_in_associated_fns_are_not_recursive() {
        let source_text = concat!(
            "impl S {\n",
            "fn subject(&self) -> Result<(), Error> {\n",
            "subject(self.raw)?;\n",
            "Self::subject(&self.inner)?;\n",
            "self.subject()?;\n",
            "Ok(())\n",
            "}\n",
            "}\n",
        );
        assert_eq!(obligations_of(source_text), ["forward subject"]);
    }

    #[test]
    fn and_then_forwards_both_sides_and_or_else_only_its_fn() {
        let source_text = subject(
            "first().and_then(|v| second(v))?;\nthird().or_else(|_| \
             fallback())?;\nfourth().and_then(Self::fifth)?;\nOk(())",
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "forward first",
                "forward second",
                "forward fallback",
                "forward fourth",
                "forward Self::fifth"
            ]
        );
    }

    #[test]
    fn try_join_forwards_its_futures() {
        let source_text = subject("tokio::try_join!(a(), self.b())?;\nOk(())");
        assert_eq!(obligations_of(&source_text), ["forward a", "forward b"]);
        assert!(!facts_of(&source_text).has_opaque_source);
    }

    #[test]
    fn select_arm_bodies_in_a_return_position_are_returned() {
        let source_text = subject(
            "tokio::select! {\n_ = token.cancelled() => Err(Error::Cancelled),\nr = self.run() => \
             r,\n}",
        );
        assert_eq!(obligations_of(&source_text), ["direct Error::Cancelled"]);
    }

    #[test]
    fn helpers_resolve_to_the_fns_of_the_callers_impl_first() {
        let source_text = concat!(
            "impl A {\n",
            "fn wrap(e: E) -> ErrorA { ErrorA::Wrapped(e) }\n",
            "fn subject() -> Result<(), ErrorA> {\n",
            "x().map_err(Self::wrap)?;\n",
            "Ok(())\n",
            "}\n",
            "}\n",
            "impl B {\n",
            "fn wrap(e: E) -> ErrorB { ErrorB::Wrapped(e) }\n",
            "}\n",
        );
        assert_eq!(obligations_of(source_text), ["either x ErrorA::Wrapped"]);
    }

    #[test]
    fn from_conversions_of_the_file_respell_converted_errors() {
        let source_text = concat!(
            "enum DecodeError { Incomplete, Invalid(ProtocolError) }\n",
            "impl From<ProtocolError> for DecodeError {\n",
            "    fn from(error: ProtocolError) -> Self { Self::Invalid(error) }\n",
            "}\n",
            "fn subject() -> Result<(), DecodeError> {\n",
            "    usize::try_from(n).map_err(|_| ProtocolError::Malformed(1))?;\n",
            "    if a { return Err(ProtocolError::Malformed(2).into()); }\n",
            "    if b { return Err(OtherError::Unrelated.into()); }\n",
            "    Err(DecodeError::Incomplete)\n",
            "}\n",
        );
        assert_eq!(
            obligations_of(source_text),
            [
                "either usize::try_from DecodeError::Invalid|ProtocolError::Malformed",
                "direct DecodeError::Invalid|ProtocolError::Malformed",
                "direct OtherError::Unrelated",
                "direct DecodeError::Incomplete",
            ]
        );
        let source_text = concat!(
            "enum InternalError { Creation(#[from] std::io::Error) }\n",
            "fn subject() -> Result<(), InternalError> {\n",
            "    let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other(\"x\"))?;\n",
            "    Ok(())\n",
            "}\n",
        );
        assert_eq!(
            obligations_of(source_text),
            ["direct InternalError::Creation|std::io::Error|Error::Other|std::io::Error::other"]
        );
    }

    #[test]
    fn callables_passed_to_a_call_complete_its_callee() {
        let source_text = subject(
            &[
                "if a { return INSTANCE.get_or_try_init(Self::init).await; }",
                "if b { return call_with_retry(config, move || async move { c.start(r).await }); }",
                "decode_frame(buffer, |reader| {",
                "    if x { return Err(Error::A.into()); }",
                "    read(reader)?;",
                "    Ok(1)",
                "})",
            ]
            .join("\n"),
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "forward Self::init|get_or_try_init",
                "forward start|call_with_retry",
                "higher-order decode_frame Error::A",
            ]
        );
        let source_text =
            subject("self.spawn_body(async move { let x = get().ok_or(Error::B)?; Ok(x) }).await");
        assert_eq!(
            obligations_of(&source_text),
            ["higher-order spawn_body Error::B"]
        );
    }

    #[test]
    fn callable_parameters_passed_on_are_callees() {
        let source_text = concat!(
            "fn subject<CallType: FnMut() -> Fut>(config: Config, grpc_call: CallType) -> \
             Result<(), Status> {\n",
            "    execute(config, grpc_call).await\n",
            "}\n",
        );
        assert_eq!(obligations_of(source_text), ["forward grpc_call|execute"]);
    }

    #[test]
    fn iterator_sinks_classify_the_items_their_callables_produce() {
        let source_text = subject(
            &[
                "v.iter().map(|x| foo(x)).collect::<Result<Vec<_>, _>>()?;",
                "o.map(|x| bar(x)).transpose()?;",
                "v.iter().filter(|x| keep(x)).map(|x| qux(x).map_err(|_| \
                 Error::Q)).collect::<Result<Vec<_>, _>>()?;",
                "v.iter().try_for_each(|x| quux(x))?;",
                "v.into_iter().map(corge).collect::<Result<Vec<_>, _>>()?;",
                "futures::future::try_join_all(v.iter().map(|x| self.load(x))).await?;",
                "Ok(())",
            ]
            .join("\n"),
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "forward foo",
                "forward bar",
                "either qux Error::Q",
                "forward quux",
                "forward corge",
                "forward load",
            ]
        );
        assert!(!facts_of(&source_text).has_opaque_source);
    }

    #[test]
    fn let_bound_errors_calls_and_spawned_handles_are_resolved() {
        let source_text = subject(
            "let e = Error::B;\nif y { return Err(e); }\nlet r = foo();\nr?;\nlet h = \
             tokio::spawn(work());\nh.await?;\nlet result = run().await;\nresult",
        );
        assert_eq!(
            obligations_of(&source_text),
            ["direct Error::B", "forward foo", "forward run"]
        );
        let facts = facts_of(&source_text);
        assert!(facts.has_opaque_error && !facts.has_opaque_source);
    }

    #[test]
    fn self_method_helpers_resolve_like_self_paths() {
        let source_text = concat!(
            "impl S {\n",
            "    fn to_error(&self, e: E) -> Error { Error::Wrapped(e) }\n",
            "    fn subject(&self) -> Result<(), Error> {\n",
            "        Err(self.to_error(e))\n",
            "    }\n",
            "}\n",
        );
        assert_eq!(obligations_of(source_text), ["direct Error::Wrapped"]);
    }

    #[test]
    fn helpers_passing_on_a_bound_error_forward_the_callee_that_returned_it() {
        let classifier = concat!(
            "    async fn classify(&self, chunk: &[u8], error: DbError) -> DbError {\n",
            "        let DbError::Sql(error) = error else {\n",
            "            return error;\n",
            "        };\n",
            "        if rejected(&error) {\n",
            "            return DbError::PayloadRejected(error);\n",
            "        }\n",
            "        DbError::Sql(error)\n",
            "    }\n",
        );
        let in_match = format!(
            "impl S {{\n{classifier}    async fn subject(&self) -> Result<(), DbError> {{\n        \
             match insert(chunk).await {{\n            Ok(()) => Ok(()),\n            Err(e) => \
             return Err(self.classify(chunk, e).await),\n        }}\n    }}\n}}\n"
        );
        let facts = facts_of(&in_match);
        assert_eq!(
            obligations_of(&in_match),
            ["forward insert", "direct DbError::PayloadRejected"]
        );
        assert!(!facts.has_opaque_source);

        let in_if_let = format!(
            "impl S {{\n{classifier}    async fn subject(&self) -> Result<(), DbError> {{\n        \
             if let Err(e) = insert(chunk).await {{\n            return \
             Err(self.classify(chunk, e).await);\n        }}\n        Ok(())\n    }}\n}}\n"
        );
        assert_eq!(
            obligations_of(&in_if_let),
            ["forward insert", "direct DbError::PayloadRejected"]
        );

        let unbound = format!(
            "impl S {{\n{classifier}    async fn subject(&self, e: DbError) -> Result<(), \
             DbError> {{\n        Err(self.classify(chunk, e).await)\n    }}\n}}\n"
        );
        let facts = facts_of(&unbound);
        assert_eq!(
            obligations_of(&unbound),
            ["direct DbError::PayloadRejected"]
        );
        assert!(facts.has_opaque_source);

        let converting = concat!(
            "impl S {\n",
            "    fn resolve(&self, error: sqlx::Error) -> Error {\n",
            "        if !missing(&error) {\n",
            "            return error.into();\n",
            "        }\n",
            "        Error::NotFound\n",
            "    }\n",
            "    fn subject(&self) -> Result<(), Error> {\n",
            "        match fetch() {\n",
            "            Ok(()) => Ok(()),\n",
            "            Err(e) => Err(self.resolve(e)),\n",
            "        }\n",
            "    }\n",
            "}\n",
        );
        assert_eq!(
            obligations_of(converting),
            ["forward fetch", "direct Error::NotFound"]
        );
    }

    #[test]
    fn query_builders_qualify_their_methods() {
        let source_text = subject(
            &[
                "sqlx::query_as::<_, Row>(&sql).bind(x).fetch_all(&pool).await?;",
                "let mut query = sqlx::query_scalar(&sql);",
                "query = query.bind(a);",
                "query.fetch_one(&pool).await?;",
                "client.query(&sql).fetch_one()?;",
                "Ok(())",
            ]
            .join("\n"),
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "forward QueryAs::fetch_all|bind",
                "forward QueryScalar::fetch_one",
                "forward fetch_one|query",
            ]
        );
    }

    #[test]
    fn chain_root_calls_are_not_alternative_spellings() {
        let source_text = subject("SpiderClient::builder(endpoint).connect().await?;\nOk(())");
        assert_eq!(obligations_of(&source_text), ["forward connect"]);
    }

    #[test]
    fn catch_all_arms_that_apply_question_marks_forward_the_scrutinee() {
        let source_text = subject(
            "let state = match self.succeed().await {\nErr(rejection @ Error::Rejected { .. }) => \
             return Ok(()),\nresult => result?,\n};\nOk(())",
        );
        assert_eq!(obligations_of(&source_text), ["forward succeed"]);
    }

    #[test]
    fn calls_whose_errors_leave_nested_scopes_are_nested_callees() {
        let source_text = subject(
            "let f = async { this.register().await?; Ok(()) };\nlet g = || \
             Self::insert(x);\nOk(())",
        );
        let facts = facts_of(&source_text);
        assert_eq!(facts.obligations, []);
        for nested_callee in ["register", "Self::insert"] {
            let path = SymbolPath::parse(nested_callee).expect("valid path");
            assert!(facts.nested_callees.contains(&path), "`{nested_callee}`");
        }
    }

    #[test]
    fn for_loops_mention_the_iterator_methods_they_call() {
        let facts = facts_of(&subject("for line in reader.lines() {}\nOk(())"));
        let next = SymbolPath::parse("next").expect("valid path");
        assert!(facts.mentions.contains(&next));
    }

    #[test]
    fn from_paths_are_conversion_targets() {
        let facts = facts_of(&subject(
            "s.parse::<Token>().map_err(ProtocolError::from)?;\nOk(())",
        ));
        assert!(facts.conversion_targets.contains("ProtocolError"));
    }

    #[test]
    fn impl_future_fns_are_scanned_through_their_async_tail() {
        let source_text = concat!(
            "fn subject() -> impl Future<Output = Result<(), Error>> {\n",
            "    async move { foo()?; bar()?; Ok(()) }\n",
            "}\n",
        );
        assert_eq!(obligations_of(source_text), ["forward foo", "forward bar"]);
    }

    #[test]
    fn unparsable_macros_with_a_postfix_question_mark_are_opaque_sources() {
        let facts = facts_of(&subject(
            "sqlx::query!(SELECT x FROM t WHERE id = load()?);\nOk(())",
        ));
        assert!(facts.has_opaque_source);
        let facts = facts_of(&subject("tracing::info!(id = ?id, %name, \"x\");\nOk(())"));
        assert!(!facts.has_opaque_source);
    }

    #[test]
    fn same_name_self_calls_in_trait_impls_depend_on_the_inherent_fns_of_the_file() {
        let trait_impl_text = concat!(
            "impl Trait for S {\n",
            "    fn subject(&self) -> Result<(), Error> {\n",
            "        Self::subject(self)?;\n",
            "        S::subject(self)?;\n",
            "        self.subject()?;\n",
            "        Trait::subject(self)?;\n",
            "        <Self as Trait>::subject(self)?;\n",
            "        Ok(())\n",
            "    }\n",
            "}\n",
        );
        let trait_impl = |inherent: &str| format!("{inherent}{trait_impl_text}");
        let nested_callees_of = |source_text: &str| {
            let mut nested_callees: Vec<String> = facts_of(source_text)
                .nested_callees
                .iter()
                .map(ToString::to_string)
                .collect();
            nested_callees.sort();
            nested_callees
        };
        for inherent in ["", "impl Other { fn subject(&self) {} }\n"] {
            assert_eq!(obligations_of(&trait_impl(inherent)), Vec::<String>::new());
            assert_eq!(
                nested_callees_of(&trait_impl(inherent)),
                ["S::subject", "Self::subject"]
            );
        }
        assert_eq!(
            obligations_of(&trait_impl("impl S { fn subject(&self) {} }\n")),
            [
                "forward Self::subject",
                "forward S::subject",
                "forward subject"
            ]
        );
        assert_eq!(
            obligations_of(&trait_impl("impl Other for S { fn subject(&self) {} }\n")),
            Vec::<String>::new()
        );

        let blanket_impl = concat!(
            "impl<T: Inner> Trait for T {\n",
            "    fn subject(&self) -> Result<(), Error> {\n",
            "        Self::subject(self)?;\n",
            "        self.subject()?;\n",
            "        Ok(())\n",
            "    }\n",
            "}\n",
        );
        assert_eq!(obligations_of(blanket_impl), Vec::<String>::new());
        assert_eq!(nested_callees_of(blanket_impl), Vec::<String>::new());
    }

    #[test]
    fn closure_bindings_name_the_callee_under_error_adaptors() {
        let source_text = subject(
            &[
                "let a = |k| load(k).context(\"c\");",
                "a(1)?;",
                "let b = |k| fetch(k).with_context(|| \"c\");",
                "b(1)?;",
                "let c = |k| read(k).map_err(Error::Wrap);",
                "c(1)?;",
                "let d = |k| open(k).inspect_err(|_| ()).map_err(Into::into);",
                "d(1)?;",
                "Ok(())",
            ]
            .join("\n"),
        );
        assert_eq!(
            obligations_of(&source_text),
            [
                "forward load",
                "forward fetch",
                "forward read",
                "forward open"
            ]
        );
    }

    #[test]
    fn callable_args_name_the_callee_under_error_adaptors() {
        let source_text = subject("call_with_retry(cfg, || start(r).context(\"c\"))?;\nOk(())");
        assert_eq!(
            obligations_of(&source_text),
            ["forward start|call_with_retry"]
        );
    }

    #[test]
    fn call_bindings_name_the_callee_under_error_adaptors() {
        let source_text = subject("let r = foo().context(\"c\");\nr?;\nOk(())");
        assert_eq!(obligations_of(&source_text), ["forward foo"]);
        let facts = facts_of(&subject("let r = x.map_err(Error::Wrap);\nr?;\nOk(())"));
        assert_eq!(facts.obligations, []);
        assert!(facts.has_opaque_source);
    }

    #[test]
    fn in_place_closures_leave_the_fn_with_the_call() {
        let source_text = subject(
            &[
                "std::thread::scope(|scope| {",
                "    let child = spawn_child().context(\"c\")?;",
                "    drop(child);",
                "    let result: anyhow::Result<()> = Ok(());",
                "    result.context(\"r\")",
                "})",
            ]
            .join("\n"),
        );
        assert_eq!(
            obligations_of(&source_text),
            ["either spawn_child anyhow::Error"]
        );
        assert!(facts_of(&source_text).has_opaque_source);

        let source_text = subject("tokio::task::block_in_place(|| { load()?; store() })");
        assert_eq!(
            obligations_of(&source_text),
            ["forward load", "forward store"]
        );
        assert!(!facts_of(&source_text).has_opaque_source);

        let source_text = subject("std::thread::scope(|s| { a()?; b() })?;\nOk(())");
        assert_eq!(obligations_of(&source_text), ["forward a", "forward b"]);

        let source_text = subject("let r = std::thread::scope(|_| load());\nr?;\nOk(())");
        assert_eq!(obligations_of(&source_text), ["forward load"]);

        let source_text = subject(
            "std::thread::scope(|_| {\nif x { return Err(Error::A); }\nanyhow::ensure!(y, \
             \"z\");\nload()\n})",
        );
        assert_eq!(
            obligations_of(&source_text),
            ["direct Error::A", "direct anyhow::Error", "forward load"]
        );
    }

    #[test]
    fn in_place_closure_returns_convert_at_the_call_site() {
        let source_text = concat!(
            "enum Outer { Inner(Inner) }\n",
            "impl From<Inner> for Outer {\n",
            "    fn from(inner: Inner) -> Self { Self::Inner(inner) }\n",
            "}\n",
            "fn subject() -> Result<(), Outer> {\n",
            "    std::thread::scope(|_| -> Result<(), Inner> {\n",
            "        if x { return Err(Inner::A); }\n",
            "        Ok(())\n",
            "    })?;\n",
            "    Ok(())\n",
            "}\n",
        );
        assert_eq!(
            obligations_of(source_text),
            ["direct Outer::Inner|Inner::A"]
        );
    }

    #[test]
    fn in_place_closure_match_arms_convert_at_the_call_site() {
        let conversion = concat!(
            "enum Outer { Inner(Inner) }\n",
            "impl From<Inner> for Outer {\n",
            "    fn from(inner: Inner) -> Self { Self::Inner(inner) }\n",
            "}\n",
            "fn classify(error: Inner) -> Inner { log(&error); error }\n",
        );
        for (body, expected) in [
            (
                concat!(
                    "    std::thread::scope(|_| -> Result<(), Inner> {\n",
                    "        match load() {\n",
                    "            Ok(()) => Ok(()),\n",
                    "            Err(error) => Err(Inner::Wrap(error)),\n",
                    "        }\n",
                    "    })?;\n",
                ),
                "either load Outer::Inner|Inner::Wrap",
            ),
            (
                concat!(
                    "    (|| -> Result<(), Inner> {\n",
                    "        if let Err(error) = load() { return Err(Inner::Wrap(error)); }\n",
                    "        Ok(())\n",
                    "    })()?;\n",
                ),
                "either load Outer::Inner|Inner::Wrap",
            ),
            (
                concat!(
                    "    std::thread::scope(|_| -> Result<(), Inner> {\n",
                    "        match retry(|| Err(Inner::A)) {\n",
                    "            Ok(()) => Ok(()),\n",
                    "            Err(error) => Err(error),\n",
                    "        }\n",
                    "    })?;\n",
                ),
                "higher-order retry Outer::Inner|Inner::A",
            ),
            (
                concat!(
                    "    std::thread::scope(|_| -> Result<(), Inner> {\n",
                    "        match retry(|| Err(Inner::A)) {\n",
                    "            Ok(()) => Ok(()),\n",
                    "            Err(error) => Err(classify(error)),\n",
                    "        }\n",
                    "    })?;\n",
                ),
                "higher-order retry Outer::Inner|Inner::A",
            ),
        ] {
            let source_text =
                format!("{conversion}fn subject() -> Result<(), Outer> {{\n{body}    Ok(())\n}}\n");
            assert_eq!(
                obligations_of(&source_text),
                [expected],
                "obligations of `{body}`"
            );
        }
    }

    #[test]
    fn closures_called_on_the_spot_leave_the_fn_only_from_a_return_position() {
        let source_text = subject("(|| { load()?; store() })()");
        assert_eq!(
            obligations_of(&source_text),
            ["forward load", "forward store"]
        );

        let source_text = subject(
            "let r = (|| { store()?; Ok::<(), Error>(()) })();\nif let Err(e) = r { warn(e); \
             }\nload()?;\nOk(())",
        );
        let facts = facts_of(&source_text);
        assert_eq!(obligations_of(&source_text), ["forward load"]);
        let store = SymbolPath::parse("store").expect("valid path");
        assert!(facts.nested_callees.contains(&store));
        assert!(!facts.has_opaque_source);

        let source_text = subject("let r = (|| load())();\nr?;\nOk(())");
        assert_eq!(obligations_of(&source_text), ["forward load"]);
    }

    #[test]
    fn in_place_closure_parameters_shadow_outer_bindings() {
        for body in [
            "let r = load();\ndrop(r);\n(|r: Result<(), Error>| { r?; Ok(()) })(store())",
            "let s = load();\ndrop(s);\nstd::thread::scope(|s| { s?; store() })",
        ] {
            let obligations = obligations_of(&subject(body));
            assert!(
                !obligations.contains(&"forward load".to_owned()),
                "obligations of `{body}`: {obligations:?}"
            );
        }

        let source_text =
            subject("let r = load();\n(|r: Result<(), Error>| { r?; Ok(()) })(store())?;\nr");
        assert_eq!(obligations_of(&source_text), ["forward load"]);
    }

    #[test]
    fn async_closures_called_on_the_spot_stay_nested() {
        for body in [
            "(async || { load().await?; Ok(()) })().await?;\nOk(())",
            "(|| async { load().await?; Ok(()) })().await?;\nOk(())",
        ] {
            let facts = facts_of(&subject(body));
            assert_eq!(facts.obligations, [], "obligations of `{body}`");
            assert!(facts.has_opaque_source, "opaque source of `{body}`");
            let load = SymbolPath::parse("load").expect("valid path");
            assert!(
                facts.nested_callees.contains(&load),
                "nested `load` of `{body}`"
            );
        }
    }

    #[test]
    fn closures_run_elsewhere_stay_nested() {
        let source_text = subject(
            &[
                "let h = std::thread::spawn(move || { store()?; Ok::<(), Error>(()) });",
                "tokio::spawn(async move { flush().await?; Ok::<(), Error>(()) });",
                "std::thread::scope(|scope| { scope.spawn(|| send()); load() })",
            ]
            .join("\n"),
        );
        let facts = facts_of(&source_text);
        assert_eq!(obligations_of(&source_text), ["forward load"]);
        for nested_callee in ["store", "flush", "send"] {
            let path = SymbolPath::parse(nested_callee).expect("valid path");
            assert!(facts.nested_callees.contains(&path), "`{nested_callee}`");
        }
    }

    #[test]
    fn method_forms_and_other_arguments_do_not_run_closures_in_place() {
        let source_text = subject(
            &[
                "pool.scope(|s| { a()?; b() })?;",
                "span.in_scope(|| { c()?; d() })?;",
                "metrics::scope(\"name\", || { e()?; f() })?;",
                "let body = |_| g();",
                "std::thread::scope(body)?;",
                "Ok(())",
            ]
            .join("\n"),
        );
        let facts = facts_of(&source_text);
        assert_eq!(
            obligations_of(&source_text),
            [
                "forward b|scope",
                "forward d|in_scope",
                "forward f|metrics::scope",
                "forward g|std::thread::scope",
            ]
        );
        for nested_callee in ["a", "c", "e"] {
            let path = SymbolPath::parse(nested_callee).expect("valid path");
            assert!(facts.nested_callees.contains(&path), "`{nested_callee}`");
        }
    }

    #[test]
    fn in_place_calls_under_error_adaptors_keep_their_question_marks_nested() {
        let source_text = subject(
            "std::thread::scope(|_| { a()?; b() }).context(\"c\")?;\n(|| { c()?; Ok(()) \
             })().context(\"d\")?;\nOk(())",
        );
        let facts = facts_of(&source_text);
        assert_eq!(obligations_of(&source_text), ["either b anyhow::Error"]);
        assert!(facts.has_opaque_source);
        for nested_callee in ["a", "c"] {
            let path = SymbolPath::parse(nested_callee).expect("valid path");
            assert!(facts.nested_callees.contains(&path), "`{nested_callee}`");
        }
    }

    #[test]
    fn possibly_recursive_self_calls_compose_with_error_adaptors_and_in_place_closures() {
        let trait_impl = |body: &str| {
            format!(
                "impl Trait for S {{\n    fn subject(&self) -> Result<(), Error> {{\n{body}\n    \
                 }}\n}}\n"
            )
        };
        let self_subject = SymbolPath::parse("Self::subject").expect("valid path");
        for body in [
            "self.subject().map_err(Into::into)?;\nOk(())",
            "Self::subject(self).inspect_err(report)",
            "let r = self.subject().context(\"c\");\nr?;\nOk(())",
            "let f = || Self::subject(self).with_context(|| \"c\");\nf()",
            "std::thread::scope(|_| self.subject())",
            "tokio::task::block_in_place(|| { Self::subject(self)?; Ok(()) })",
            "let r = std::thread::scope(|_| Self::subject(self));\nr",
            "(|| self.subject())()?;\nOk(())",
        ] {
            let source_text = trait_impl(body);
            assert_eq!(
                obligations_of(&source_text),
                Vec::<String>::new(),
                "`{body}`"
            );
            assert_eq!(
                facts_of(&source_text).nested_callees,
                HashSet::from([self_subject.clone()]),
                "`{body}`"
            );
        }
        for (body, obligation) in [
            (
                "Self::subject(self).context(\"c\")",
                "either Self::subject anyhow::Error",
            ),
            (
                "self.subject().map_err(Error::Wrap)",
                "either Self::subject Error::Wrap",
            ),
            (
                "std::thread::scope(|_| self.subject().with_context(|| \"c\"))",
                "either Self::subject anyhow::Error",
            ),
        ] {
            assert_eq!(obligations_of(&trait_impl(body)), [obligation], "`{body}`");
        }
    }

    #[test]
    fn let_closures_contribute_only_from_calls_in_a_return_position() {
        let handled_closure = [
            "let inner = |x: u32| -> anyhow::Result<()> { load(x)?; store(x) };",
            "if inner(1).is_err() { tracing::warn!(\"first attempt failed\"); }",
        ];
        let source_text =
            |tail: &[&str]| subject(&[&handled_closure[..], tail].concat().join("\n"));
        assert_eq!(
            obligations_of(&source_text(&["inner(2)?;", "other()"])),
            ["forward store", "forward other"]
        );
        assert_eq!(
            obligations_of(&source_text(&["other()"])),
            ["forward other"]
        );
    }
}
