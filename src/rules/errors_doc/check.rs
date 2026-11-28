//! The entry point of the `errors-doc` rule, and the comparison of each fn's `# Errors` section
//! with the facts of its body.

use std::collections::HashSet;

use crate::diagnostic::Diagnostic;
use crate::diagnostic::Position;
use crate::diagnostic::counted;
use crate::doc::doc_lines;
use crate::file_context::FileContext;
use crate::rules::RuleId;
use crate::rules::errors_doc::body;
use crate::rules::errors_doc::body::BodyFacts;
use crate::rules::errors_doc::body::Obligation;
use crate::rules::errors_doc::body::ObligationKind;
use crate::rules::errors_doc::body::Spellings;
use crate::rules::errors_doc::errors_section;
use crate::rules::errors_doc::errors_section::CANONICAL_INTRO;
use crate::rules::errors_doc::errors_section::ErrorsSection;
use crate::rules::errors_doc::errors_section::FormatFindingKind;
use crate::rules::errors_doc::errors_section::ForwardBullet;
use crate::rules::errors_doc::errors_section::ForwardKind;
use crate::rules::errors_doc::errors_section::VariantBullet;
use crate::rules::errors_doc::file_index::FileIndex;
use crate::rules::errors_doc::items;
use crate::rules::errors_doc::items::FnItem;
use crate::rules::errors_doc::symbol::KeyedPath;
use crate::rules::errors_doc::symbol::Pairing;
use crate::rules::errors_doc::symbol::QualifierLinks;
use crate::syntax::symbol_path::SymbolPath;
use crate::syntax::symbol_path::is_upper_camel_case;

/// Checks the `# Errors` sections of the fns in each file of a run.
#[derive(Debug)]
pub struct Checker {
    skip_tests: bool,
}

impl Checker {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A checker that skips `#[test]` fns and `#[cfg(test)]` items if `skip_tests` is set.
    pub const fn new(skip_tests: bool) -> Self {
        Self { skip_tests }
    }

    /// Checks every checkable fn of one file. A trait fn without a default body only gets the
    /// format findings of its section's bullets, since its section documents a contract rather than
    /// a body, and its intro isn't checked.
    ///
    /// The file is skipped in these cases:
    ///
    /// * The file failed to lex.
    /// * `syn` fails to parse the file. The [`FileContext`] reports a `parse-error` diagnostic.
    /// * The file is an out-of-line `#[cfg(test)]` module and tests are skipped. If a declaring
    ///   file that would decide it can't be read or parsed, the file is checked, and the
    ///   [`FileContext`] reports a `parse-error` diagnostic at its first line that says why.
    ///
    /// # Returns
    ///
    /// An `errors-doc` diagnostic for every mismatch between a fn's `# Errors` section and its
    /// body. Each one's anchor span is the whole fn, and its fn header is the lines from the fn's
    /// first attribute or doc line through its `fn` keyword.
    pub fn check(&self, context: &FileContext<'_>) -> Vec<Diagnostic> {
        let Some(syntax_tree) = context.syntax_tree(RuleId::ErrorsDoc) else {
            return Vec::new();
        };
        if self.skip_tests && context.is_test_module(RuleId::ErrorsDoc) {
            return Vec::new();
        }
        let mut diagnostics = Vec::new();
        let file_index = FileIndex::build(syntax_tree);
        let qualifier_links = file_index.qualifier_links();
        for fn_item in items::collect(syntax_tree, self.skip_tests) {
            let Some(section) = errors_section::parse(&doc_lines::collect(fn_item.attrs)) else {
                continue;
            };
            if fn_item.block.is_none() {
                let contract_findings = section.format_findings.iter().filter(|format_finding| {
                    format_finding.kind != FormatFindingKind::NonCanonicalIntro
                });
                diagnostics.extend(contract_findings.map(|format_finding| {
                    Diagnostic::new(
                        RuleId::ErrorsDoc.into(),
                        format_finding.position,
                        format_message(&format_finding.kind, &fn_item.param_names),
                    )
                    .with_anchor(fn_item.anchor)
                    .with_fn_header(fn_item.header)
                }));
                continue;
            }
            let facts = body::scan(&fn_item, &file_index);
            let fn_check = FnCheck {
                fn_item: &fn_item,
                section: &section,
                facts: &facts,
                file_index: &file_index,
                qualifier_links: &qualifier_links,
            };
            diagnostics.extend(fn_check.findings().into_iter().map(|finding| {
                Diagnostic::new(RuleId::ErrorsDoc.into(), finding.position, finding.message)
                    .with_anchor(fn_item.anchor)
                    .with_fn_header(fn_item.header)
            }));
        }
        diagnostics
    }
}

/// One mismatch, before it becomes a [`Diagnostic`].
struct Finding {
    position: Position,
    message: String,
}

/// The comparison of one fn's `# Errors` section with its body.
///
/// # Type Parameters
///
/// * `'check` - The lifetime of the fn, its section and the facts compared.
/// * `'ast` - The lifetime of the syntax tree the fn is in.
struct FnCheck<'check, 'ast> {
    fn_item: &'check FnItem<'ast>,
    section: &'check ErrorsSection,
    facts: &'check BodyFacts,
    file_index: &'check FileIndex<'ast>,
    qualifier_links: &'check QualifierLinks,
}

impl FnCheck<'_, '_> {
    /// Compares the section with the body.
    ///
    /// Forward checks, each reported once per obligation:
    ///
    /// * A `Direct` error must be matched by a variant head path or a prose link.
    /// * A `Forward` callee must be matched by a forward bullet's path.
    /// * A `Forward` callee may also be matched by a link of a variant bullet beyond its head, such
    ///   as a condition that names the function the error comes from.
    /// * An `Either` or a `HigherOrder` is satisfied by either of the two.
    /// * An `EitherUnknown` is satisfied by a matching forward bullet, or by any variant bullet or
    ///   prose link at all.
    /// * A `HelperError` is satisfied by a variant head path or a prose link of its error type, or
    ///   by any of them if the type is unknown.
    ///
    /// Reverse checks, for bullets only:
    ///
    /// * A variant bullet is stale when none of its head paths is returned or mentioned and none of
    ///   its links names a callee, unless [`Self::is_kept_variant`] keeps it.
    /// * An unresolved mapper in the body keeps every variant bullet.
    /// * A forward path is stale when no callee matches it and [`Self::is_stale_forward`] holds.
    ///
    /// Missing forwards together with stale variants become one delegation finding.
    ///
    /// # Returns
    ///
    /// The findings, including the section's format findings.
    fn findings(&self) -> Vec<Finding> {
        let obligations = unique_obligations(&self.facts.obligations);
        let coverage = self.coverage(&obligations);
        let has_documented_errors =
            !self.section.variant_bullets.is_empty() || !self.section.prose_links.is_empty();

        let mut findings = Vec::new();
        let mut missing_forwards: Vec<&Obligation> = Vec::new();
        for (obligation_index, obligation) in obligations.iter().enumerate() {
            let is_callee_documented = coverage.callees.paired_facts.contains(&obligation_index);
            let documented_errors: Vec<bool> = (0..obligation.kind.errors().len())
                .map(|error_index| {
                    coverage.errors.paired_facts.contains(&ErrorKey {
                        obligation_index,
                        error_index,
                    })
                })
                .collect();
            let are_errors_documented =
                documented_errors.iter().all(|is_documented| *is_documented);
            let is_documented = match &obligation.kind {
                ObligationKind::Direct { .. } => are_errors_documented,
                ObligationKind::Forward { .. } => is_callee_documented,
                ObligationKind::Either { .. } | ObligationKind::HigherOrder { .. } => {
                    is_callee_documented || are_errors_documented
                }
                ObligationKind::EitherUnknown { .. } => {
                    is_callee_documented || has_documented_errors
                }
                ObligationKind::HelperError { error_type, .. } => {
                    self.documents_error_of_type(error_type.as_ref())
                }
            };
            if is_documented {
                continue;
            }
            if matches!(obligation.kind, ObligationKind::Forward { .. }) {
                missing_forwards.push(obligation);
            } else {
                findings.push(self.missing_finding(obligation, &documented_errors));
            }
        }

        let stale_variants: Vec<&VariantBullet> = if self.facts.has_opaque_error {
            Vec::new()
        } else {
            self.section
                .variant_bullets
                .iter()
                .enumerate()
                .filter(|(variant_bullet_index, bullet)| {
                    let source = ErrorDocSource::VariantBullet {
                        variant_bullet_index: *variant_bullet_index,
                    };
                    let link = CalleeDocKey::VariantLink {
                        variant_bullet_index: *variant_bullet_index,
                    };
                    !coverage.errors.paired_docs.contains(&source)
                        && !coverage.callees.paired_docs.contains(&link)
                        && !bullet.head_paths.iter().any(|path| self.is_mentioned(path))
                        && !self.is_kept_variant(bullet)
                })
                .map(|(_, bullet)| bullet)
                .collect()
        };

        for (forward_bullet_index, bullet) in self.section.forward_bullets.iter().enumerate() {
            if bullet.kind == ForwardKind::Unnamed {
                continue;
            }
            for (path_index, path) in bullet.paths.iter().enumerate() {
                let key = CalleeDocKey::ForwardBullet {
                    forward_bullet_index,
                    path_index,
                };
                if !coverage.callees.paired_docs.contains(&key)
                    && !is_upper_camel_case(path.last())
                    && self.is_stale_forward(path)
                {
                    findings.push(self.stale_forward_finding(bullet, path));
                }
            }
        }

        if !missing_forwards.is_empty() && !stale_variants.is_empty() {
            findings.push(self.delegation_finding(&missing_forwards, stale_variants.len()));
        } else {
            findings.extend(
                missing_forwards
                    .into_iter()
                    .map(|obligation| self.missing_finding(obligation, &[])),
            );
            findings.extend(
                stale_variants
                    .into_iter()
                    .map(|bullet| self.stale_variant_finding(bullet)),
            );
        }

        findings.extend(self.format_findings());
        findings
    }

    /// Turns the section's format findings into findings. A section without bullets in a fn whose
    /// body has no error source states that the fn never fails, such as a section kept for
    /// `clippy::missing_errors_doc`, so its intro finding is dropped.
    ///
    /// # Returns
    ///
    /// The format findings.
    fn format_findings(&self) -> impl Iterator<Item = Finding> {
        let states_infallibility = !self.section.has_bullets && self.facts.is_infallible();
        self.section
            .format_findings
            .iter()
            .filter(move |format_finding| {
                !(states_infallibility
                    && format_finding.kind == FormatFindingKind::NonCanonicalIntro)
            })
            .map(|format_finding| Finding {
                position: format_finding.position,
                message: format_message(&format_finding.kind, &self.fn_item.param_names),
            })
    }

    /// Pairs the returned errors with the documented errors, and the callees with the documented
    /// forwards and the variant bullets' links, with the two-pass rule of
    /// [`crate::rules::errors_doc::symbol::SymbolScope::pair`].
    ///
    /// # Returns
    ///
    /// Both pairings.
    fn coverage(&self, obligations: &[&Obligation]) -> Coverage {
        let scope = &self.fn_item.scope;
        let error_facts: Vec<KeyedPath<'_, ErrorKey>> =
            obligations
                .iter()
                .enumerate()
                .flat_map(|(obligation_index, obligation)| {
                    obligation.kind.errors().iter().enumerate().flat_map(
                        move |(error_index, error)| {
                            error.paths().iter().map(move |path| KeyedPath {
                                key: ErrorKey {
                                    obligation_index,
                                    error_index,
                                },
                                path,
                            })
                        },
                    )
                })
                .collect();
        let error_docs: Vec<KeyedPath<'_, ErrorDocSource>> = self
            .section
            .variant_bullets
            .iter()
            .enumerate()
            .flat_map(|(variant_bullet_index, bullet)| {
                bullet.head_paths.iter().map(move |path| KeyedPath {
                    key: ErrorDocSource::VariantBullet {
                        variant_bullet_index,
                    },
                    path,
                })
            })
            .chain(self.section.prose_links.iter().map(|path| KeyedPath {
                key: ErrorDocSource::ProseLink,
                path,
            }))
            .collect();
        let callee_facts: Vec<KeyedPath<'_, usize>> = obligations
            .iter()
            .enumerate()
            .flat_map(|(obligation_index, obligation)| {
                obligation
                    .kind
                    .callee()
                    .into_iter()
                    .flat_map(Spellings::paths)
                    .map(move |path| KeyedPath {
                        key: obligation_index,
                        path,
                    })
            })
            .collect();
        let forward_docs: Vec<KeyedPath<'_, CalleeDocKey>> = self
            .section
            .forward_bullets
            .iter()
            .enumerate()
            .flat_map(|(forward_bullet_index, bullet)| {
                bullet
                    .paths
                    .iter()
                    .enumerate()
                    .map(move |(path_index, path)| KeyedPath {
                        key: CalleeDocKey::ForwardBullet {
                            forward_bullet_index,
                            path_index,
                        },
                        path,
                    })
            })
            .chain(self.section.variant_bullets.iter().enumerate().flat_map(
                |(variant_bullet_index, bullet)| {
                    bullet.linked_paths.iter().map(move |path| KeyedPath {
                        key: CalleeDocKey::VariantLink {
                            variant_bullet_index,
                        },
                        path,
                    })
                },
            ))
            .collect();
        Coverage {
            errors: scope.pair(&error_facts, &error_docs, self.qualifier_links),
            callees: scope.pair(&callee_facts, &forward_docs, self.qualifier_links),
        }
    }

    /// # Returns
    ///
    /// Whether a variant head path or a prose link names `error_type` or one of its variants, or
    /// whether any of them exists if `error_type` is [`None`].
    fn documents_error_of_type(&self, error_type: Option<&SymbolPath>) -> bool {
        let mut documented = self
            .section
            .variant_bullets
            .iter()
            .flat_map(|bullet| &bullet.head_paths)
            .chain(&self.section.prose_links);
        match error_type {
            Some(error_type) => documented.any(|path| {
                path.matches_type(error_type)
                    || path
                        .parent()
                        .is_some_and(|parent| parent.matches_type(error_type))
            }),
            None => documented.next().is_some(),
        }
    }

    /// # Returns
    ///
    /// Whether some path the body mentions matches `documented` as a mention.
    fn is_mentioned(&self, documented: &SymbolPath) -> bool {
        self.facts
            .mentions
            .iter()
            .any(|mention| self.fn_item.scope.matches_mention(documented, mention))
    }

    /// Decides whether a variant bullet that the body doesn't return is kept anyway.
    ///
    /// The bullet is kept in these cases:
    ///
    /// * A head path names the fn's own error type, which the signature guarantees.
    /// * The body has an opaque source, and every head path names an error type rather than a
    ///   variant, since the opaque source may produce that type.
    /// * A head path names a variant of a type `X` that the body converts into with `X::from`, and
    ///   some `From` conversion into `X` may build it: any variant if the file doesn't declare `X`,
    ///   or one that the file's conversions build.
    ///
    /// # Returns
    ///
    /// Whether the bullet is kept.
    fn is_kept_variant(&self, bullet: &VariantBullet) -> bool {
        let names_conversion_target = bullet.head_paths.iter().any(|path| {
            path.qualifier()
                .filter(|qualifier| self.facts.conversion_targets.contains(*qualifier))
                .is_some_and(|qualifier| {
                    self.file_index
                        .conversion_variants_into(qualifier)
                        .is_none_or(|variants| {
                            variants
                                .iter()
                                .any(|variant| self.fn_item.scope.matches_exactly(variant, path))
                        })
                })
        });
        if names_conversion_target {
            return true;
        }
        let names_own_error_type =
            self.fn_item
                .own_error_type
                .as_ref()
                .is_some_and(|own_error_type| {
                    bullet
                        .head_paths
                        .iter()
                        .any(|path| self.fn_item.scope.matches_exactly(path, own_error_type))
                });
        let names_only_error_types = bullet
            .head_paths
            .iter()
            .all(|path| !path.qualifier().is_some_and(is_upper_camel_case));
        names_own_error_type || (self.facts.has_opaque_source && names_only_error_types)
    }

    /// Decides whether a forward path that no callee matches is stale.
    ///
    /// The path is kept in these cases:
    ///
    /// * It matches a nested callee: a callee whose error leaves a closure or an async block of the
    ///   body, such as a `?` inside an async block that the body hands to a helper, or the callee
    ///   of a possibly recursive call of a trait impl method.
    /// * The body mentions it and has an opaque source or error, since the mentioned call's error
    ///   may leave the fn through it.
    /// * The body has an opaque source and the file calls it anywhere, since the `Result` that the
    ///   opaque source forwards may come from that call, such as a task spawned by another fn whose
    ///   join handle the body awaits.
    /// * It names a callable parameter of the fn that the body mentions.
    ///
    /// A path that the body only mentions, such as a call whose `Option` it checks, is stale.
    ///
    /// # Returns
    ///
    /// Whether the forward path is stale.
    fn is_stale_forward(&self, documented: &SymbolPath) -> bool {
        if self.facts.nested_callees.iter().any(|nested_callee| {
            self.fn_item
                .scope
                .matches_exactly(nested_callee, documented)
                || self.is_loosely_paired(nested_callee, documented)
        }) {
            return false;
        }
        let is_callable_param = documented.len() == 1
            && self
                .fn_item
                .callable_params
                .iter()
                .any(|param| param == documented.last());
        if self.facts.has_opaque_source && self.file_index.is_called(documented.last()) {
            return false;
        }
        let has_opaque_site = self.facts.has_opaque_source || self.facts.has_opaque_error;
        !((has_opaque_site || is_callable_param) && self.is_mentioned(documented))
    }

    /// # Returns
    ///
    /// Whether `fact` and `documented` match loosely: their last segments are equal, both are
    /// qualified, and the file's qualifier links link their qualifiers.
    fn is_loosely_paired(&self, fact: &SymbolPath, documented: &SymbolPath) -> bool {
        fact.last() == documented.last()
            && fact.qualifier().zip(documented.qualifier()).is_some_and(
                |(fact_qualifier, doc_qualifier)| {
                    self.qualifier_links
                        .are_linked(fact_qualifier, doc_qualifier)
                },
            )
    }

    /// # Returns
    ///
    /// The finding of an obligation that the section doesn't document, at the obligation's
    /// position. `documented_errors` tells, for each error of the obligation, whether the section
    /// documents it, so that an `Either` or a `HigherOrder` asks only for the undocumented ones.
    fn missing_finding(&self, obligation: &Obligation, documented_errors: &[bool]) -> Finding {
        let fn_name = &self.fn_item.name;
        let message = match &obligation.kind {
            ObligationKind::Direct { error } => {
                let error = error.primary();
                format!(
                    "`{fn_name}` returns [`{error}`] directly, but `# Errors` has no bullet for \
                     it; add \"* [`{error}`] if ...\""
                )
            }
            ObligationKind::Forward { callee } => {
                let callee = callee.primary();
                format!(
                    "`{fn_name}` forwards errors from `{callee}`, but `# Errors` does not list \
                     it; add \"* Forwards [`{callee}`]'s return values on failure.\""
                )
            }
            ObligationKind::Either { callee, errors } => {
                let callee = callee.primary();
                let error_paths: Vec<SymbolPath> =
                    errors.iter().map(|error| error.primary().clone()).collect();
                let error_links = join_links(&error_paths);
                let (documented, undocumented) = split_documented(&error_paths, documented_errors);
                if documented.is_empty() {
                    let variant_suggestion = match error_paths.as_slice() {
                        [error] => format!("\"* [`{error}`] if ...\""),
                        _ => "a variant bullet for each of them".to_owned(),
                    };
                    format!(
                        "`{fn_name}` maps errors from `{callee}` into {error_links}, but `# \
                         Errors` documents neither; add \"* Forwards [`{callee}`]'s return values \
                         on failure.\" or {variant_suggestion}"
                    )
                } else {
                    format!(
                        "`{fn_name}` maps errors from `{callee}` into {error_links}, but `# \
                         Errors` documents only {}; add \"* Forwards [`{callee}`]'s return values \
                         on failure.\" or a variant bullet for {}",
                        join_links(&documented),
                        join_links(&undocumented)
                    )
                }
            }
            ObligationKind::HelperError { helper, error_type } => {
                let helper = helper.primary();
                let type_suggestion = error_type
                    .as_ref()
                    .map_or_default(|error_type| format!(", or \"* [`{error_type}`] if ...\""));
                let built = error_type.as_ref().map_or_else(
                    || "an error".to_owned(),
                    |error_type| format!("the [`{error_type}`]"),
                );
                format!(
                    "`{fn_name}` returns {built} that `{helper}` builds, but `# Errors` has no \
                     bullet for it; add a bullet for the variant `{helper}` \
                     builds{type_suggestion}"
                )
            }
            ObligationKind::EitherUnknown { callee, mapper } => {
                let callee = callee.primary();
                format!(
                    "`{fn_name}` maps errors from `{callee}` with {mapper}, but `# Errors` \
                     documents neither; add \"* Forwards [`{callee}`]'s return values on \
                     failure.\" or a variant bullet"
                )
            }
            ObligationKind::HigherOrder {
                callee,
                closure_errors,
            } => {
                let callee = callee.primary();
                let error_paths: Vec<SymbolPath> = closure_errors
                    .iter()
                    .map(|error| error.primary().clone())
                    .collect();
                let error_links = join_links(&error_paths);
                let (documented, undocumented) = split_documented(&error_paths, documented_errors);
                if documented.is_empty() {
                    format!(
                        "`{fn_name}` forwards errors from `{callee}`, whose closures return \
                         {error_links}, but `# Errors` documents neither; add \"* Forwards \
                         [`{callee}`]'s return values on failure.\" or a variant bullet for each \
                         closure error"
                    )
                } else {
                    format!(
                        "`{fn_name}` forwards errors from `{callee}`, whose closures return \
                         {error_links}, but `# Errors` documents only {}; add \"* Forwards \
                         [`{callee}`]'s return values on failure.\" or a variant bullet for {}",
                        join_links(&documented),
                        join_links(&undocumented)
                    )
                }
            }
        };
        Finding {
            position: obligation.position,
            message,
        }
    }

    /// # Returns
    ///
    /// The finding of a variant bullet that the body never returns, at the bullet.
    fn stale_variant_finding(&self, bullet: &VariantBullet) -> Finding {
        let fn_name = &self.fn_item.name;
        let head_links = join_links(&bullet.head_paths);
        let (verb, pronoun) = if bullet.head_paths.len() == 1 {
            ("is", "it")
        } else {
            ("are", "them")
        };
        Finding {
            position: bullet.position,
            message: format!(
                "{head_links} {verb} documented, but `{fn_name}` never returns {pronoun} \
                 directly; if a callee returns {pronoun}, write \"* Forwards [`callee`]'s return \
                 values on failure.\" instead, or remove the bullet"
            ),
        }
    }

    /// # Returns
    ///
    /// The finding of a forward bullet's path whose errors the body never forwards, at the bullet.
    fn stale_forward_finding(&self, bullet: &ForwardBullet, documented: &SymbolPath) -> Finding {
        let fn_name = &self.fn_item.name;
        Finding {
            position: bullet.position,
            message: format!(
                "\"Forwards [`{documented}`]\" names a function whose errors `{fn_name}` never \
                 forwards"
            ),
        }
    }

    /// # Returns
    ///
    /// The one finding that replaces the missing forwards and the stale variants of the fn, at the
    /// section's header.
    fn delegation_finding(
        &self,
        missing_forwards: &[&Obligation],
        num_stale_variants: usize,
    ) -> Finding {
        let fn_name = &self.fn_item.name;
        let callees: Vec<&SymbolPath> = missing_forwards
            .iter()
            .filter_map(|obligation| obligation.kind.callee().map(Spellings::primary))
            .collect();
        let callee_names = callees
            .iter()
            .map(|callee| format!("`{callee}`"))
            .collect::<Vec<_>>()
            .join(" and ");
        let suggestions = callees
            .iter()
            .map(|callee| format!("\"* Forwards [`{callee}`]'s return values on failure.\""))
            .collect::<Vec<_>>()
            .join(" and ");
        let variants = counted(num_stale_variants, "variant");
        let pronoun = if callees.len() == 1 { "it" } else { "them" };
        Finding {
            position: self.section.header,
            message: format!(
                "`{fn_name}` lists {variants} it never returns directly, and forwards \
                 {callee_names} without listing {pronoun}; replace them with {suggestions}"
            ),
        }
    }
}

/// Identifies one error of one obligation, by the obligation's index among the unique obligations
/// and the error's index among the obligation's errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ErrorKey {
    obligation_index: usize,
    error_index: usize,
}

/// Where a documented error path comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ErrorDocSource {
    /// The head of the variant bullet at `variant_bullet_index`.
    VariantBullet { variant_bullet_index: usize },

    /// A link in the section's prose or in a bullet of another kind.
    ProseLink,
}

/// Where a documented callee path comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CalleeDocKey {
    /// The path at `path_index` of the forward bullet at `forward_bullet_index`.
    ForwardBullet {
        forward_bullet_index: usize,
        path_index: usize,
    },

    /// A link beyond the head of the variant bullet at `variant_bullet_index`.
    VariantLink { variant_bullet_index: usize },
}

/// The pairings of a fn's obligations with its `# Errors` section.
struct Coverage {
    /// The returned errors paired with variant head paths and prose links.
    errors: Pairing<ErrorKey, ErrorDocSource>,

    /// The callees, keyed by their obligation's index, paired with the forward bullets' paths and
    /// the variant bullets' links.
    callees: Pairing<usize, CalleeDocKey>,
}

/// What makes two obligations the same one: an `EitherUnknown` is identified by its callee only,
/// since its mapper description is only for messages.
///
/// # Type Parameters
///
/// * `'check` - The lifetime of the obligation the key identifies.
#[derive(PartialEq, Eq, Hash)]
enum ObligationKey<'check> {
    Direct {
        error: &'check Spellings,
    },
    Forward {
        callee: &'check Spellings,
    },
    Either {
        callee: &'check Spellings,
        errors: &'check Vec<Spellings>,
    },
    EitherUnknown {
        callee: &'check Spellings,
    },
    HelperError {
        helper: &'check Spellings,
        error_type: &'check Option<SymbolPath>,
    },
    HigherOrder {
        callee: &'check Spellings,
        closure_errors: &'check Vec<Spellings>,
    },
}

impl<'check> ObligationKey<'check> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The key of `kind`.
    const fn new(kind: &'check ObligationKind) -> Self {
        match kind {
            ObligationKind::Direct { error } => Self::Direct { error },
            ObligationKind::Forward { callee } => Self::Forward { callee },
            ObligationKind::Either { callee, errors } => Self::Either { callee, errors },
            ObligationKind::EitherUnknown { callee, .. } => Self::EitherUnknown { callee },
            ObligationKind::HelperError { helper, error_type } => {
                Self::HelperError { helper, error_type }
            }
            ObligationKind::HigherOrder {
                callee,
                closure_errors,
            } => Self::HigherOrder {
                callee,
                closure_errors,
            },
        }
    }
}

/// # Returns
///
/// The obligations without duplicates by [`ObligationKey`], keeping the first site of each.
fn unique_obligations(obligations: &[Obligation]) -> Vec<&Obligation> {
    let mut seen = HashSet::new();
    obligations
        .iter()
        .filter(|obligation| seen.insert(ObligationKey::new(&obligation.kind)))
        .collect()
}

/// # Returns
///
/// A tuple containing:
///
/// * The paths whose entry in `documented_errors` is set.
/// * The other paths, including those that `documented_errors` has no entry for.
fn split_documented(
    paths: &[SymbolPath],
    documented_errors: &[bool],
) -> (Vec<SymbolPath>, Vec<SymbolPath>) {
    let (documented, undocumented): (Vec<_>, Vec<_>) =
        paths.iter().enumerate().partition(|(error_index, _)| {
            documented_errors
                .get(*error_index)
                .copied()
                .unwrap_or(false)
        });
    let paths_of = |indexed: Vec<(usize, &SymbolPath)>| {
        indexed.into_iter().map(|(_, path)| path.clone()).collect()
    };
    (paths_of(documented), paths_of(undocumented))
}

/// # Returns
///
/// `paths` as intra-doc links joined with "and", such as ``[`A`] and [`B`]``.
fn join_links(paths: &[SymbolPath]) -> String {
    paths
        .iter()
        .map(|path| format!("[`{path}`]"))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// # Returns
///
/// The message of a format finding of `kind`, in a fn whose parameters are named `param_names`. An
/// unnamed forward that names a parameter in a bare code span is told to forward that parameter.
fn format_message(kind: &FormatFindingKind, param_names: &[String]) -> String {
    match kind {
        FormatFindingKind::UnnamedForward { code_names } => code_names
            .iter()
            .find(|name| param_names.contains(name))
            .map_or_else(
                || {
                    "this Forwards bullet names no function; write \"* Forwards \
                     [`path::to::func`]'s return values on failure.\""
                        .to_owned()
                },
                |param_name| {
                    format!(
                        "this Forwards bullet names no function; write \"* Forwards \
                         `{param_name}`'s return values on failure.\""
                    )
                },
            ),
        FormatFindingKind::NonCanonicalForward => {
            "write \"Forwards [`func`]'s return values on failure.\"".to_owned()
        }
        FormatFindingKind::ForwardNamesType { path } => format!(
            "\"Forwards [`{path}`]\" names a type, not a function; name the function whose errors \
             are forwarded, or write \"* [`{path}`] if ...\" for an error the fn returns"
        ),
        FormatFindingKind::NonCanonicalIntro => format!(
            "start the `# Errors` section with \"{CANONICAL_INTRO}\" followed by one bullet per \
             error"
        ),
        FormatFindingKind::DuplicateSection => {
            "the docstring has a second `# Errors` section at the same level; merge them".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::Width;
    use crate::line_ranges::LineSpan;
    use crate::syntax::test_code::TestModuleCache;

    const INTRO_MESSAGE: &str = "start the `# Errors` section with";

    /// Checks `source_text` as a file that no other file declares.
    ///
    /// # Returns
    ///
    /// Each finding as `<line>:<column>: <message>`, sorted by position, without the findings of a
    /// section that has no bullet. [`intro_findings`] reports those.
    ///
    /// # Panics
    ///
    /// Panics if `source_text` doesn't parse.
    fn checked(source_text: &str) -> Vec<String> {
        all_findings(source_text)
            .into_iter()
            .filter(|finding| !finding.contains(INTRO_MESSAGE))
            .collect()
    }

    /// # Returns
    ///
    /// The findings of `source_text` about a section's intro, as [`checked`] formats them.
    fn intro_findings(source_text: &str) -> Vec<String> {
        all_findings(source_text)
            .into_iter()
            .filter(|finding| finding.contains(INTRO_MESSAGE))
            .collect()
    }

    fn all_findings(source_text: &str) -> Vec<String> {
        syn::parse_file(source_text).expect("valid file");
        let test_module_cache = TestModuleCache::default();
        let context = FileContext::new(
            Path::new("nonexistent-fixture-directory/subject.rs"),
            source_text,
            Width::DEFAULT,
            &test_module_cache,
        );
        let mut diagnostics = Checker::new(false).check(&context);
        diagnostics.sort_by_key(Diagnostic::position);
        diagnostics
            .iter()
            .map(|diagnostic| {
                let position = diagnostic.position();
                format!(
                    "{}:{}: {}",
                    position.line,
                    position.column,
                    diagnostic.message()
                )
            })
            .collect()
    }

    /// # Returns
    ///
    /// A fn named `subject` whose `# Errors` section has `bullets` and whose body is `body`.
    fn subject(bullets: &[&str], body: &str) -> String {
        let mut doc = String::new();
        for bullet in bullets {
            doc.push_str("/// * ");
            doc.push_str(bullet);
            doc.push('\n');
        }
        format!(
            "/// Does things.\n///\n/// # Errors\n///\n/// Returns an error if:\n///\n{doc}fn \
             subject() -> Result<(), Error> {{\n{body}\n}}\n"
        )
    }

    #[test]
    fn missing_variant_and_its_satisfied_twin() {
        let body = "    load()?;\n    Err(Error::NotFound)";
        assert_eq!(
            checked(&subject(
                &["Forwards [`load`]'s return values on failure."],
                body
            )),
            [
                "10:5: `subject` returns [`Error::NotFound`] directly, but `# Errors` has no \
                 bullet for it; add \"* [`Error::NotFound`] if ...\""
            ]
        );
        let bullets = [
            "[`Error::NotFound`] if the thing is missing.",
            "Forwards [`load`]'s return values on failure.",
        ];
        assert_eq!(checked(&subject(&bullets, body)), Vec::<String>::new());
    }

    #[test]
    fn missing_forward_and_its_satisfied_twin() {
        let body = "    self.store.load().await?;\n    Ok(())";
        assert_eq!(
            checked(&subject(&[], body)),
            [
                "8:28: `subject` forwards errors from `load`, but `# Errors` does not list it; \
                 add \"* Forwards [`load`]'s return values on failure.\""
            ]
        );
        let bullets = ["Forwards [`Store::load`]'s return values on failure."];
        assert_eq!(checked(&subject(&bullets, body)), Vec::<String>::new());
    }

    #[test]
    fn missing_either_is_satisfied_by_either_form() {
        let body = "    serialize(x).map_err(InternalError::TaskOutputs)?;\n    Ok(())";
        assert_eq!(
            checked(&subject(&[], body)),
            [
                "8:53: `subject` maps errors from `serialize` into \
                 [`InternalError::TaskOutputs`], but `# Errors` documents neither; add \"* \
                 Forwards [`serialize`]'s return values on failure.\" or \"* \
                 [`InternalError::TaskOutputs`] if ...\""
            ]
        );
        for bullet in [
            "Forwards [`serialize`]'s return values on failure.",
            "[`InternalError::TaskOutputs`] if serialization fails.",
        ] {
            assert_eq!(
                checked(&subject(&[bullet], body)),
                Vec::<String>::new(),
                "`{bullet}`"
            );
        }
    }

    #[test]
    fn errors_a_helper_builds_are_direct_and_each_needs_a_bullet() {
        let source_text = |bullets: &[&str]| {
            format!(
                "{}fn classify(error: E) -> Error {{\n    if large(&error) {{\n        return \
                 Error::PayloadRejected;\n    }}\n    Error::Timeout\n}}\n",
                subject(
                    bullets,
                    "    match insert() {\n        Ok(()) => Ok(()),\n        Err(e) => \
                     Err(classify(e)),\n    }"
                )
            )
        };
        let payload_rejected = "[`Error::PayloadRejected`] if the payload is too large.";
        let timeout = "[`Error::Timeout`] if the insertion times out.";
        assert_eq!(
            checked(&source_text(&[payload_rejected])),
            [
                "11:19: `subject` returns [`Error::Timeout`] directly, but `# Errors` has no \
                 bullet for it; add \"* [`Error::Timeout`] if ...\""
            ]
        );
        assert_eq!(
            checked(&source_text(&[payload_rejected, timeout])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn helpers_passing_on_a_forwarded_error_are_covered_by_its_forward() {
        let source_text = |bullets: &[&str]| {
            format!(
                "{}fn classify(error: Error) -> Error {{\n    let Error::Sql(error) = error else \
                 {{\n        return error;\n    }};\n    if large(&error) {{\n        return \
                 Error::PayloadRejected(error);\n    }}\n    Error::Sql(error)\n}}\n",
                subject(
                    bullets,
                    "    match insert() {\n        Ok(()) => Ok(()),\n        Err(e) => \
                     Err(classify(e)),\n    }"
                )
            )
        };
        let payload_rejected = "[`Error::PayloadRejected`] if the payload is too large.";
        let forward = "Forwards [`insert`]'s return values on failure.";
        assert_eq!(
            checked(&source_text(&[payload_rejected, forward])),
            Vec::<String>::new()
        );
        assert_eq!(
            checked(&source_text(&[payload_rejected])),
            [
                "9:11: `subject` forwards errors from `insert`, but `# Errors` does not list it; \
                 add \"* Forwards [`insert`]'s return values on failure.\""
            ]
        );
        assert_eq!(
            checked(&source_text(&[forward])),
            [
                "11:19: `subject` returns [`Error::PayloadRejected`] directly, but `# Errors` has \
                 no bullet for it; add \"* [`Error::PayloadRejected`] if ...\""
            ]
        );
    }

    #[test]
    fn missing_either_unknown_is_satisfied_by_a_forward_or_any_variant() {
        let body = "    send(x).map_err(to_status)?;\n    Ok(())";
        assert_eq!(
            checked(&subject(&[], body)),
            [
                "8:31: `subject` maps errors from `send` with `to_status`, but `# Errors` \
                 documents neither; add \"* Forwards [`send`]'s return values on failure.\" or a \
                 variant bullet"
            ]
        );
        for bullet in [
            "Forwards [`send`]'s return values on failure.",
            "[`Status`] if sending fails.",
        ] {
            assert_eq!(
                checked(&subject(&[bullet], body)),
                Vec::<String>::new(),
                "`{bullet}`"
            );
        }
    }

    #[test]
    fn stale_variant_and_its_satisfied_twin() {
        let bullets = ["[`Error::Conflict`] if the key exists."];
        assert_eq!(
            checked(&subject(&bullets, "    Ok(())")),
            [
                "7:1: [`Error::Conflict`] is documented, but `subject` never returns it directly; \
                 if a callee returns it, write \"* Forwards [`callee`]'s return values on \
                 failure.\" instead, or remove the bullet"
            ]
        );
        let body = "    if exists { return Err(Error::Conflict); }\n    Ok(())";
        assert_eq!(checked(&subject(&bullets, body)), Vec::<String>::new());
    }

    #[test]
    fn stale_forward_and_its_satisfied_twin() {
        let bullets = ["Forwards [`plan_events`]'s return values on failure."];
        let stale = [
            "7:1: \"Forwards [`plan_events`]\" names a function whose errors `subject` never \
             forwards",
        ];
        assert_eq!(checked(&subject(&bullets, "    Ok(())")), stale);
        let body = "    if let Some(events) = self.plan_events() {}\n    Ok(())";
        assert_eq!(checked(&subject(&bullets, body)), stale);
        let body = "    let events = self.plan_events()?;\n    Ok(())";
        assert_eq!(checked(&subject(&bullets, body)), Vec::<String>::new());
    }

    #[test]
    fn missing_forwards_with_stale_variants_are_one_delegation_finding() {
        let bullets = [
            "[`ClientError::JobNotFound`] if no job exists.",
            "[`ClientError::Transport`] if the transport fails.",
        ];
        let body = "    self.orchestration.submit_job(id).await";
        assert_eq!(
            checked(&subject(&bullets, body)),
            [
                "3:1: `subject` lists 2 variants it never returns directly, and forwards \
                 `submit_job` without listing it; replace them with \"* Forwards [`submit_job`]'s \
                 return values on failure.\""
            ]
        );
    }

    #[test]
    fn unnamed_typo_and_duplicate_forwards_get_format_findings() {
        let bullets = [
            "Forwards [`sqlx::Error`] on DB failure.",
            "Forward [`a`]'s return values on failure.",
            "Forwards [`b`]' return values on failure.",
        ];
        let source_text = subject(&bullets, "    a()?;\n    b()?;\n    Ok(())").replace(
            "fn subject",
            "///\n/// # Errors\n///\n/// * [`Error::A`] if a.\nfn subject",
        );
        assert_eq!(
            checked(&source_text),
            [
                "7:1: this Forwards bullet names no function; write \"* Forwards \
                 [`path::to::func`]'s return values on failure.\"",
                "8:1: write \"Forwards [`func`]'s return values on failure.\"",
                "9:1: write \"Forwards [`func`]'s return values on failure.\"",
                "11:1: the docstring has a second `# Errors` section at the same level; merge them",
            ]
        );
    }

    #[test]
    fn own_type_multi_head_and_opaque_mapper_bullets_are_never_stale() {
        let own_type = subject(
            &["[`Error`] converted from [`DbError::Corrupted`] if the DB is corrupted."],
            "    Ok(())",
        );
        assert_eq!(checked(&own_type), Vec::<String>::new());

        let one_of_several = subject(
            &["[`Error::A`] or [`Error::B`] if it fails."],
            "    Err(Error::B)",
        );
        assert_eq!(checked(&one_of_several), Vec::<String>::new());

        let opaque_error = subject(
            &[
                "[`Error::A`] if a.",
                "Forwards [`send`]'s return values on failure.",
            ],
            "    send().map_err(to_status)?;\n    Ok(())",
        );
        assert_eq!(checked(&opaque_error), Vec::<String>::new());

        let opaque_source_type = subject(
            &["[`tokio::task::JoinError`] if the task panics."],
            "    handle.await?;\n    Ok(())",
        );
        assert_eq!(checked(&opaque_source_type), Vec::<String>::new());
    }

    #[test]
    fn converted_foreign_errors_need_their_own_bullet() {
        let body = "    return Err(DbError::Corrupted.into());";
        let bullets = [
            "[`Error::Unrelated`] if the moon is full.",
            "[`Error::AlsoUnrelated`] if it rains.",
        ];
        assert_eq!(
            checked(&subject(&bullets, body)),
            [
                "7:1: [`Error::Unrelated`] is documented, but `subject` never returns it \
                 directly; if a callee returns it, write \"* Forwards [`callee`]'s return values \
                 on failure.\" instead, or remove the bullet",
                "8:1: [`Error::AlsoUnrelated`] is documented, but `subject` never returns it \
                 directly; if a callee returns it, write \"* Forwards [`callee`]'s return values \
                 on failure.\" instead, or remove the bullet",
                "10:12: `subject` returns [`DbError::Corrupted`] directly, but `# Errors` has no \
                 bullet for it; add \"* [`DbError::Corrupted`] if ...\"",
            ]
        );
        let bullets = ["[`Error`] converted from [`DbError::Corrupted`] if the DB is corrupted."];
        assert_eq!(checked(&subject(&bullets, body)), Vec::<String>::new());
    }

    #[test]
    fn forwards_of_a_differently_qualified_fn_are_missing_and_stale() {
        let body = "    Storage::load()?;\n    Ok(())";
        let bullets = ["Forwards [`Cache::load`]'s return values on failure."];
        assert_eq!(
            checked(&subject(&bullets, body)),
            [
                "7:1: \"Forwards [`Cache::load`]\" names a function whose errors `subject` never \
                 forwards",
                "9:20: `subject` forwards errors from `Storage::load`, but `# Errors` does not \
                 list it; add \"* Forwards [`Storage::load`]'s return values on failure.\"",
            ]
        );
        let source_text = format!("use config::Cache;\n{}", subject(&bullets, body));
        assert_eq!(checked(&source_text), Vec::<String>::new());
    }

    #[test]
    fn findings_are_anchored_to_the_whole_fn_and_its_header() {
        let source_text = subject(&[], "    load()?;\n    Ok(())");
        let test_module_cache = TestModuleCache::default();
        let context = FileContext::new(
            Path::new("nonexistent-fixture-directory/subject.rs"),
            &source_text,
            Width::DEFAULT,
            &test_module_cache,
        );
        let diagnostics = Checker::new(false).check(&context);
        let spans: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| !diagnostic.message().contains(INTRO_MESSAGE))
            .map(|diagnostic| (diagnostic.anchor(), diagnostic.fn_header()))
            .collect();
        assert_eq!(
            spans,
            [(
                LineSpan::new(1, 10).expect("valid span"),
                LineSpan::new(1, 7)
            )]
        );
    }

    #[test]
    fn opaque_sources_keep_only_forwards_the_body_mentions_or_the_file_calls() {
        let bullets = [
            "Forwards [`mentioned`]'s return values on failure.",
            "Forwards [`Client::spawned_elsewhere`]'s return values on failure.",
            "Forwards [`never_called`]'s return values on failure.",
        ];
        let other_fn = "fn other(client: Client) {\n    spawn(async move { \
                        client.spawned_elsewhere().await });\n}\nfn never_called() {}\n";
        let opaque = format!(
            "{}{other_fn}",
            subject(
                &bullets,
                "    self.handle = spawn(mentioned());\n    self.handle.await??;\n    Ok(())"
            )
        );
        assert_eq!(
            checked(&opaque),
            [
                "9:1: \"Forwards [`never_called`]\" names a function whose errors `subject` never \
                 forwards"
            ]
        );

        let bullets = [
            "Forwards [`race`]'s return values on failure.",
            "Forwards [`Client::spawned_elsewhere`]'s return values on failure.",
        ];
        let tuple_binding = format!(
            "{}{other_fn}",
            subject(
                &bullets,
                "    let (commit, cancel) = race();\n    assert_eq!(commit?, 1);\n    Ok(())"
            )
        );
        assert_eq!(checked(&tuple_binding), Vec::<String>::new());

        let attributed = format!(
            "{}{other_fn}",
            subject(
                &["Forwards [`load`]'s return values on failure.", bullets[1]],
                "    load()?;\n    Ok(())"
            )
        );
        assert_eq!(
            checked(&attributed),
            [
                "8:1: \"Forwards [`Client::spawned_elsewhere`]\" names a function whose errors \
                 `subject` never forwards"
            ]
        );
    }

    #[test]
    fn opaque_sources_keep_variant_bullets_checked() {
        let bullets = ["[`Error::Conflict`] if the key exists."];
        assert_eq!(
            checked(&subject(&bullets, "    handle.await?;\n    Ok(())")),
            [
                "7:1: [`Error::Conflict`] is documented, but `subject` never returns it directly; \
                 if a callee returns it, write \"* Forwards [`callee`]'s return values on \
                 failure.\" instead, or remove the bullet"
            ]
        );
    }

    #[test]
    fn each_obligation_is_reported_once() {
        let body = "    load()?;\n    load()?;\n    Ok(())";
        assert_eq!(checked(&subject(&[], body)).len(), 1);
    }

    #[test]
    fn fns_without_an_errors_section_or_result_are_not_checked() {
        let source_text = "/// No section.\nfn a() -> Result<(), E> { load()?; Ok(()) }\n/// # \
                           Errors\n///\n/// * [`E::X`] if x.\nfn b() -> Option<()> { None }\n";
        assert_eq!(checked(source_text), Vec::<String>::new());
    }

    #[test]
    fn variant_bullet_links_satisfy_the_forward_of_a_converted_binding() {
        let source_text = concat!(
            "/// Waits.\n",
            "///\n",
            "/// # Errors\n",
            "///\n",
            "/// Returns an error if:\n",
            "///\n",
            "/// * [`CacheError::StaleState`] if:\n",
            "///   * The job is terminal (see [`StateRequirement::ensure_met`]).\n",
            "fn subject() -> Result<(), CacheError> {\n",
            "    match requirement.ensure_met(state) {\n",
            "        Err(error) => Err(error.into()),\n",
            "        Ok(()) => Ok(()),\n",
            "    }\n",
            "}\n",
        );
        assert_eq!(checked(source_text), Vec::<String>::new());
    }

    #[test]
    fn from_conversions_of_the_file_match_variant_bullets() {
        let source_text = concat!(
            "enum DecodeError { Invalid(ProtocolError) }\n",
            "impl From<ProtocolError> for DecodeError {\n",
            "    fn from(error: ProtocolError) -> Self { Self::Invalid(error) }\n",
            "}\n",
            "/// Checks.\n",
            "///\n",
            "/// # Errors\n",
            "///\n",
            "/// Returns an error if:\n",
            "///\n",
            "/// * [`DecodeError::Invalid`] if the count is wrong.\n",
            "fn subject() -> Result<(), DecodeError> {\n",
            "    Err(ProtocolError::Malformed(n).into())\n",
            "}\n",
        );
        assert_eq!(checked(source_text), Vec::<String>::new());
    }

    #[test]
    fn variants_of_a_from_conversion_target_are_kept() {
        let bullets = [
            "[`ProtocolError::InvalidToken`] if the token is invalid.",
            "Forwards [`str::parse`]'s return values on failure.",
        ];
        let body = "    s.parse::<Token>().map_err(ProtocolError::from)?;\n    Ok(())";
        assert_eq!(checked(&subject(&bullets, body)), Vec::<String>::new());
        let source_text = format!(
            "enum ProtocolError {{ InvalidId(#[from] IdError), InvalidToken(String) }}\n{}",
            subject(&bullets, body)
        );
        assert_eq!(
            checked(&source_text),
            [
                "8:1: [`ProtocolError::InvalidToken`] is documented, but `subject` never returns \
                 it directly; if a callee returns it, write \"* Forwards [`callee`]'s return \
                 values on failure.\" instead, or remove the bullet"
            ]
        );
    }

    #[test]
    fn higher_order_obligation_is_satisfied_by_the_helper_or_the_closure_errors() {
        let body = "    decode_frame(buffer, |reader| {\n        if x { return \
                    Err(Error::A.into()); }\n        Ok(1)\n    })";
        assert_eq!(
            checked(&subject(&[], body)),
            [
                "8:5: `subject` forwards errors from `decode_frame`, whose closures return \
                 [`Error::A`], but `# Errors` documents neither; add \"* Forwards \
                 [`decode_frame`]'s return values on failure.\" or a variant bullet for each \
                 closure error"
            ]
        );
        for bullet in [
            "Forwards [`decode_frame`]'s return values on failure.",
            "[`Error::A`] if the frame is bad.",
        ] {
            assert_eq!(
                checked(&subject(&[bullet], body)),
                Vec::<String>::new(),
                "`{bullet}`"
            );
        }
    }

    #[test]
    fn forwards_of_callees_in_nested_scopes_and_of_callable_parameters_are_kept() {
        let bullets = [
            "Forwards [`Store::register`]'s return values on failure.",
            "Forwards [`Self::spawn_body`]'s return values on failure.",
        ];
        let body = "    self.spawn_body(async move { store.register().await?; Ok(()) }).await";
        assert_eq!(checked(&subject(&bullets, body)), Vec::<String>::new());

        let source_text = concat!(
            "/// Runs.\n",
            "///\n",
            "/// # Errors\n",
            "///\n",
            "/// Returns an error if:\n",
            "///\n",
            "/// * Forwards `body`'s return values on failure.\n",
            "fn subject(body: impl Future<Output = Result<(), E>>) -> Result<(), E> {\n",
            "    let handle = tracker.spawn(body);\n",
            "    Ok(())\n",
            "}\n",
        );
        assert_eq!(checked(source_text), Vec::<String>::new());
    }

    #[test]
    fn unnamed_forward_naming_a_parameter_is_told_to_forward_it() {
        let source_text = concat!(
            "/// Decodes.\n",
            "///\n",
            "/// # Errors\n",
            "///\n",
            "/// Returns an error if:\n",
            "///\n",
            "/// * Forwards the error of an invalid frame from `decode`.\n",
            "fn subject(decode: impl FnOnce() -> Result<(), E>) -> Result<(), E> {\n",
            "    match decode() { Ok(()) => Ok(()), Err(E::Invalid(e)) => Err(e) }\n",
            "}\n",
        );
        assert_eq!(
            checked(source_text),
            [
                "7:1: this Forwards bullet names no function; write \"* Forwards `decode`'s \
                 return values on failure.\""
            ]
        );
    }

    #[test]
    fn wrong_query_builder_qualifiers_are_missing_and_stale() {
        let body = "    sqlx::query_scalar(SQL).bind(id).fetch_one(&pool).await?;\n    Ok(())";
        let bullets = ["Forwards [`sqlx::query::Query::fetch_one`]'s return values on failure."];
        assert_eq!(
            checked(&subject(&bullets, body)),
            [
                "7:1: \"Forwards [`sqlx::query::Query::fetch_one`]\" names a function whose \
                 errors `subject` never forwards",
                "9:60: `subject` forwards errors from `QueryScalar::fetch_one`, but `# Errors` \
                 does not list it; add \"* Forwards [`QueryScalar::fetch_one`]'s return values on \
                 failure.\"",
            ]
        );
        let bullets =
            ["Forwards [`sqlx::query::QueryScalar::fetch_one`]'s return values on failure."];
        assert_eq!(checked(&subject(&bullets, body)), Vec::<String>::new());
    }

    #[test]
    fn forwards_naming_a_type_get_a_format_finding() {
        let bullets = ["Forwards [`tokio::task::JoinHandle`]'s return values on failure."];
        assert_eq!(
            checked(&subject(&bullets, "    handle.await?;\n    Ok(())")),
            [
                "7:1: \"Forwards [`tokio::task::JoinHandle`]\" names a type, not a function; name \
                 the function whose errors are forwarded, or write \"* \
                 [`tokio::task::JoinHandle`] if ...\" for an error the fn returns"
            ]
        );
    }

    #[test]
    fn sections_without_the_canonical_intro_get_a_format_finding() {
        let bullets = ["Forwards [`load`]'s return values on failure."];
        let canonical = subject(&bullets, "    load()?;\n    Ok(())");
        assert_eq!(intro_findings(&canonical), Vec::<String>::new());
        let message = "start the `# Errors` section with \"Returns an error if:\" followed by one \
                       bullet per error";
        let without_intro = canonical.replace("/// Returns an error if:\n///\n", "");
        assert_eq!(intro_findings(&without_intro), [format!("5:1: {message}")]);
        let prose_only = "/// Encodes.\n///\n/// # Errors\n///\n/// Returns [`Error::Zstd`] if \
                          zstd fails.\nfn subject() -> Result<(), Error> {\n    \
                          Err(Error::Zstd)\n}\n";
        assert_eq!(intro_findings(prose_only), [format!("5:1: {message}")]);
        assert_eq!(checked(prose_only), Vec::<String>::new());
    }

    #[test]
    fn sections_stating_that_the_fn_never_fails_are_accepted_only_for_infallible_bodies() {
        let message = "start the `# Errors` section with \"Returns an error if:\" followed by one \
                       bullet per error";
        let infallible = "/// Beats.\n///\n/// # Errors\n///\n/// This method does not currently \
                          return an error.\nasync fn subject(&self) -> Result<(), Error> {\n    \
                          self.registry.update().await;\n    Ok(())\n}\n";
        assert_eq!(all_findings(infallible), Vec::<String>::new());
        for fallible_body in ["    load()?;\n    Ok(())", "    handle.await?;\n    Ok(())"] {
            let fallible = format!(
                "/// Beats.\n///\n/// # Errors\n///\n/// This method does not currently return an \
                 error.\nfn subject() -> Result<(), Error> {{\n{fallible_body}\n}}\n"
            );
            assert_eq!(intro_findings(&fallible), [format!("5:1: {message}")]);
        }
        let bulleted = subject(&[], "    Ok(())")
            .replace("/// Returns an error if:\n///\n", "/// * Never fails.\n");
        assert_eq!(intro_findings(&bulleted).len(), 1);
    }

    #[test]
    fn trait_fns_without_a_body_get_only_bullet_format_findings() {
        let source_text = concat!(
            "trait Store {\n",
            "    /// Loads.\n",
            "    ///\n",
            "    /// # Errors\n",
            "    ///\n",
            "    /// Implementations document their errors.\n",
            "    fn load(&self) -> Result<(), E>;\n",
            "\n",
            "    /// Saves.\n",
            "    ///\n",
            "    /// # Errors\n",
            "    ///\n",
            "    /// * Forwards [`sqlx::Error`] on DB failure.\n",
            "    fn save(&self) -> Result<(), E>;\n",
            "}\n",
        );
        assert_eq!(
            all_findings(source_text),
            [
                "13:5: this Forwards bullet names no function; write \"* Forwards \
                 [`path::to::func`]'s return values on failure.\""
            ]
        );
    }

    #[test]
    fn errors_a_helper_builds_are_never_documented_by_forwarding_the_helper() {
        let source_text = |bullets: &[&str]| {
            format!(
                "fn transport_error(message: &str) -> Error {{\n    \
                 Error::Transport(message.to_owned())\n}}\n{}",
                subject(
                    bullets,
                    "    if closed() {\n        return Err(transport_error(\"closed\"));\n    \
                     }\n    Ok(())"
                )
            )
        };
        assert_eq!(
            checked(&source_text(&[])),
            [
                "12:16: `subject` returns [`Error::Transport`] directly, but `# Errors` has no \
                 bullet for it; add \"* [`Error::Transport`] if ...\""
            ]
        );
        assert_eq!(
            checked(&source_text(&[
                "[`Error::Transport`] if the channel is closed."
            ])),
            Vec::<String>::new()
        );
        assert_eq!(
            checked(&source_text(&[
                "Forwards [`transport_error`]'s return values on failure."
            ])),
            [
                "10:1: \"Forwards [`transport_error`]\" names a function whose errors `subject` \
                 never forwards",
                "13:16: `subject` returns [`Error::Transport`] directly, but `# Errors` has no \
                 bullet for it; add \"* [`Error::Transport`] if ...\""
            ]
        );
    }

    #[test]
    fn unresolved_error_building_helpers_need_a_bullet_for_their_error_type() {
        let in_file = |bullets: &[&str]| {
            format!(
                "fn transport_error(message: &str) -> Error {{\n    build(message)\n}}\n{}",
                subject(
                    bullets,
                    "    if closed() {\n        return Err(transport_error(\"closed\"));\n    \
                     }\n    Ok(())"
                )
            )
        };
        assert_eq!(
            checked(&in_file(&[])),
            [
                "12:16: `subject` returns the [`Error`] that `transport_error` builds, but `# \
                 Errors` has no bullet for it; add a bullet for the variant `transport_error` \
                 builds, or \"* [`Error`] if ...\""
            ]
        );
        assert_eq!(
            checked(&in_file(
                &["[`Error::Transport`] if the channel is closed."]
            )),
            Vec::<String>::new()
        );
        assert_eq!(
            checked(&in_file(
                &["[`Status::Internal`] if the channel is closed."]
            )),
            [
                "13:16: `subject` returns the [`Error`] that `transport_error` builds, but `# \
                 Errors` has no bullet for it; add a bullet for the variant `transport_error` \
                 builds, or \"* [`Error`] if ...\""
            ]
        );

        let elsewhere = |bullets: &[&str]| {
            subject(
                bullets,
                "    if closed() {\n        return Err(errors::transport(\"closed\"));\n    \
                 }\n    Ok(())",
            )
        };
        assert_eq!(
            checked(&elsewhere(&[])),
            [
                "9:16: `subject` returns an error that `errors::transport` builds, but `# Errors` \
                 has no bullet for it; add a bullet for the variant `errors::transport` builds"
            ]
        );
        assert_eq!(
            checked(&elsewhere(&["[`io::Error`] if the channel is closed."])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn same_name_self_calls_in_trait_impls_forward_the_inherent_fn_or_nothing() {
        let trait_impl = |inherent: &str, bullet: &str, call: &str| {
            let head = concat!(
                "impl Submitter for Client {\n",
                "    /// # Errors\n",
                "    ///\n",
                "    /// Returns an error if:\n",
            );
            let signature = "    fn submit(&self) -> Result<(), Error> {\n";
            format!("{inherent}{head}{bullet}{signature}        Ok({call}?)\n    }}\n}}\n")
        };
        let inherent = concat!(
            "impl Client {\n",
            "    fn submit(&self) -> Result<(), Error> {\n",
            "        Err(Error::Failed)\n",
            "    }\n",
            "}\n",
        );
        let forwards_bullet = |path: &str| {
            format!("    ///\n    /// * Forwards [`{path}`]'s return values on failure.\n")
        };
        let bullet = forwards_bullet("Client::submit");
        for call in [
            "Self::submit(self)",
            "Client::submit(self)",
            "self.submit()",
        ] {
            for optional_bullet in [bullet.as_str(), &forwards_bullet("Self::submit"), ""] {
                assert_eq!(
                    all_findings(&trait_impl("", optional_bullet, call)),
                    Vec::<String>::new(),
                    "`{call}` with bullet `{optional_bullet}`"
                );
            }
            assert_eq!(
                checked(&trait_impl("", &forwards_bullet("Other::submit"), call)),
                [
                    "6:5: \"Forwards [`Other::submit`]\" names a function whose errors `submit` \
                     never forwards"
                ],
                "`{call}`"
            );
            assert_eq!(
                checked(&trait_impl(inherent, &bullet, call)),
                Vec::<String>::new(),
                "`{call}`"
            );
        }
        assert_eq!(
            checked(&trait_impl(inherent, "", "Self::submit(self)")),
            [
                "11:30: `submit` forwards errors from `Self::submit`, but `# Errors` does not \
                 list it; add \"* Forwards [`Self::submit`]'s return values on failure.\""
            ]
        );
        assert_eq!(
            checked(&trait_impl(inherent, "", "self.submit()")),
            [
                "11:25: `submit` forwards errors from `submit`, but `# Errors` does not list it; \
                 add \"* Forwards [`submit`]'s return values on failure.\""
            ]
        );
        assert_eq!(
            checked(&trait_impl("", &bullet, "Submitter::submit(self)")),
            [
                "6:5: \"Forwards [`Client::submit`]\" names a function whose errors `submit` \
                 never forwards"
            ]
        );
    }

    #[test]
    fn documented_forwards_through_adaptor_closures_are_clean() {
        let source_text = concat!(
            "use anyhow::Context;\n",
            "\n",
            "/// Creates the directories.\n",
            "///\n",
            "/// # Errors\n",
            "///\n",
            "/// Returns an error if:\n",
            "///\n",
            "/// * Forwards [`std::fs::create_dir_all`]'s return values on failure.\n",
            "fn create(a: &std::path::Path, b: &std::path::Path) -> anyhow::Result<()> {\n",
            "    let create_directory = |directory: &std::path::Path| {\n",
            "        std::fs::create_dir_all(directory)\n",
            "            .context(\"failed to create the directory\")\n",
            "    };\n",
            "    create_directory(a)?;\n",
            "    create_directory(b)\n",
            "}\n",
        );
        assert_eq!(checked(source_text), Vec::<String>::new());
    }

    #[test]
    fn documented_forwards_through_thread_scopes_are_clean() {
        let source_text = concat!(
            "use anyhow::Context;\n",
            "\n",
            "/// Runs the command in a thread scope.\n",
            "///\n",
            "/// # Errors\n",
            "///\n",
            "/// Returns an error if:\n",
            "///\n",
            "/// * Forwards [`std::process::Command::spawn`]'s return values on failure.\n",
            "fn run(command: &mut std::process::Command) -> anyhow::Result<()> {\n",
            "    std::thread::scope(|_scope| {\n",
            "        let child = command.spawn().context(\"failed to spawn\")?;\n",
            "        drop(child);\n",
            "        let result: anyhow::Result<()> = Ok(());\n",
            "        result.context(\"failed to run\")\n",
            "    })\n",
            "}\n",
        );
        assert_eq!(checked(source_text), Vec::<String>::new());
    }

    #[test]
    fn in_place_closure_question_marks_need_bullets() {
        let source_text = subject(
            &["Forwards [`store`]'s return values on failure."],
            "    std::thread::scope(|_scope| {\n        load()?;\n        store()\n    })",
        );
        assert_eq!(
            checked(&source_text),
            [
                "10:15: `subject` forwards errors from `load`, but `# Errors` does not list it; \
                 add \"* Forwards [`load`]'s return values on failure.\""
            ]
        );
    }

    #[test]
    fn returns_from_closures_called_on_the_spot_convert_at_the_call_site() {
        let source_text = concat!(
            "enum Outer { Inner(Inner) }\n",
            "impl From<Inner> for Outer {\n",
            "    fn from(inner: Inner) -> Self { Self::Inner(inner) }\n",
            "}\n",
            "/// Checks.\n",
            "///\n",
            "/// # Errors\n",
            "///\n",
            "/// Returns an error if:\n",
            "///\n",
            "/// * [`Outer::Inner`] if `x` is set.\n",
            "fn subject(x: bool) -> Result<(), Outer> {\n",
            "    (|| -> Result<(), Inner> {\n",
            "        if x { return Err(Inner::A); }\n",
            "        Ok(())\n",
            "    })()?;\n",
            "    Ok(())\n",
            "}\n",
        );
        assert_eq!(checked(source_text), Vec::<String>::new());
    }

    #[test]
    fn wraps_in_in_place_closures_convert_at_the_call_site() {
        let source_text = concat!(
            "enum Outer { Inner(Inner) }\n",
            "impl From<Inner> for Outer {\n",
            "    fn from(inner: Inner) -> Self { Self::Inner(inner) }\n",
            "}\n",
            "/// Checks.\n",
            "///\n",
            "/// # Errors\n",
            "///\n",
            "/// Returns an error if:\n",
            "///\n",
            "/// * [`Outer::Inner`] if loading fails.\n",
            "fn wrapped_match() -> Result<(), Outer> {\n",
            "    std::thread::scope(|_| -> Result<(), Inner> {\n",
            "        match load() {\n",
            "            Ok(()) => Ok(()),\n",
            "            Err(error) => Err(Inner::Wrap(error)),\n",
            "        }\n",
            "    })?;\n",
            "    Ok(())\n",
            "}\n",
            "/// Checks.\n",
            "///\n",
            "/// # Errors\n",
            "///\n",
            "/// Returns an error if:\n",
            "///\n",
            "/// * [`Outer::Inner`] if loading fails.\n",
            "fn wrapped_if_let() -> Result<(), Outer> {\n",
            "    (|| -> Result<(), Inner> {\n",
            "        if let Err(error) = load() { return Err(Inner::Wrap(error)); }\n",
            "        Ok(())\n",
            "    })()?;\n",
            "    Ok(())\n",
            "}\n",
        );
        assert_eq!(checked(source_text), Vec::<String>::new());
    }

    #[test]
    fn same_name_self_calls_in_trait_impls_compose_with_error_adaptors_and_in_place_closures() {
        let trait_impl = |bullet: &str, call: &str| {
            let head = concat!(
                "impl Submitter for Client {\n",
                "    /// # Errors\n",
                "    ///\n",
                "    /// Returns an error if:\n",
            );
            let signature = "    fn submit(&self) -> Result<(), Error> {\n";
            format!("{head}{bullet}{signature}        Ok({call}?)\n    }}\n}}\n")
        };
        let bullet_of = |text: &str| format!("    ///\n    /// * {text}\n");
        let forwards_bullet =
            |path: &str| bullet_of(&format!("Forwards [`{path}`]'s return values on failure."));
        for call in [
            "self.submit().map_err(Into::into)",
            "std::thread::scope(|_| Self::submit(self))",
            "(|| self.submit())()",
        ] {
            for optional_bullet in [
                forwards_bullet("Client::submit"),
                forwards_bullet("Self::submit"),
                String::new(),
            ] {
                assert_eq!(
                    all_findings(&trait_impl(&optional_bullet, call)),
                    Vec::<String>::new(),
                    "`{call}` with bullet `{optional_bullet}`"
                );
            }
            assert_eq!(
                checked(&trait_impl(&forwards_bullet("Other::submit"), call)),
                [
                    "6:5: \"Forwards [`Other::submit`]\" names a function whose errors `submit` \
                     never forwards"
                ],
                "`{call}`"
            );
        }
        for (call, error, column) in [
            ("Self::submit(self).context(\"c\")", "anyhow::Error", 43),
            (
                "std::thread::scope(|_| self.submit().map_err(Error::Wrap))",
                "Error::Wrap",
                70,
            ),
        ] {
            for bullet in [
                forwards_bullet("Client::submit"),
                forwards_bullet("Self::submit"),
                bullet_of(&format!("[`{error}`] if submitting fails.")),
            ] {
                assert_eq!(
                    checked(&trait_impl(&bullet, call)),
                    Vec::<String>::new(),
                    "`{call}` with bullet `{bullet}`"
                );
            }
            assert_eq!(
                checked(&trait_impl("", call)),
                [format!(
                    "6:{column}: `submit` maps errors from `Self::submit` into [`{error}`], but \
                     `# Errors` documents neither; add \"* Forwards [`Self::submit`]'s return \
                     values on failure.\" or \"* [`{error}`] if ...\""
                )],
                "`{call}`"
            );
        }
    }

    #[test]
    fn let_closures_contribute_only_from_calls_in_a_return_position() {
        let handled_closure = concat!(
            "    let inner = |x: u32| -> anyhow::Result<()> {\n",
            "        load(x)?;\n",
            "        store(x)\n",
            "    };\n",
            "    if inner(1).is_err() {\n",
            "        tracing::warn!(\"first attempt failed\");\n",
            "    }\n",
        );
        let store_bullet = "Forwards [`store`]'s return values on failure.";
        let other_bullet = "Forwards [`other`]'s return values on failure.";
        assert_eq!(
            checked(&subject(
                &[store_bullet],
                &format!("{handled_closure}    inner(2)?;\n    Ok(())")
            )),
            Vec::<String>::new()
        );
        assert_eq!(
            checked(&subject(
                &[other_bullet],
                &format!("{handled_closure}    inner(2)?;\n    other()")
            )),
            [
                "16:13: `subject` forwards errors from `store`, but `# Errors` does not list it; \
                 add \"* Forwards [`store`]'s return values on failure.\""
            ]
        );
        assert_eq!(
            checked(&subject(
                &[other_bullet],
                &format!("{handled_closure}    other()")
            )),
            Vec::<String>::new()
        );
    }
}
