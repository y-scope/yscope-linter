# yscope-linter design

This document is for maintainers. It describes how `yscope-linter` is built: its pipeline, its
modules, and the models behind its rules. What the tool checks and how to use it are documented for
users elsewhere:

* [`README.md`](../README.md): installation, the command line, output and exit codes, the cargo
  integration and its settings, and CI use.
* [`docs/rules/README.md`](rules/README.md): the rule index, with the codes, the naming standard and
  the deprecation policy.
* [`docs/rules/comment-greedy-wrap.md`](rules/comment-greedy-wrap.md): the comment rule, with its
  exemptions.
* [`docs/rules/errors-doc.md`](rules/errors-doc.md): the `# Errors` rule, with the accepted
  documentation format and its findings.
* [`docs/rules/symbol-order.md`](rules/symbol-order.md): the item ordering rule.
* [`docs/rules/type-params-doc.md`](rules/type-params-doc.md): the `# Type Parameters` rule.
* [`docs/rules/trait-params-doc.md`](rules/trait-params-doc.md): the `# Parameters` rule.
* [`docs/suppression.md`](suppression.md): the suppression comments.

This document doesn't repeat those. It links to them where the user-facing behavior is the
specification.

## Contents

* [Scope](#scope)
* [Architecture](#architecture)
  * [Module layout](#module-layout)
  * [Code conventions](#code-conventions)
  * [Dependencies](#dependencies)
* [Pipeline](#pipeline)
* [Cargo mode](#cargo-mode)
  * [Workspace discovery](#workspace-discovery)
  * [Package files](#package-files)
  * [Settings](#settings)
* [FileContext](#filecontext)
* [Rule registry](#rule-registry)
  * [Codes and selectors](#codes-and-selectors)
  * [Adding a rule](#adding-a-rule)
  * [Deprecating a rule](#deprecating-a-rule)
* [Comment lexer](#comment-lexer)
* [Markdown classifier](#markdown-classifier)
  * [Block starts](#block-starts)
* [comment-greedy-wrap](#comment-greedy-wrap)
  * [Fit test](#fit-test)
  * [rustfmt hint](#rustfmt-hint)
  * [Blocks](#blocks)
  * [Line kinds](#line-kinds)
  * [Paragraph machine](#paragraph-machine)
  * [Design notes](#design-notes)
* [errors-doc](#errors-doc)
  * [Stages](#stages)
  * [Item walk](#item-walk)
  * [Out-of-line test modules](#out-of-line-test-modules)
  * [Doc lines](#doc-lines)
  * [Parsing the `# Errors` section](#parsing-the--errors-section)
  * [Symbol paths and matching](#symbol-paths-and-matching)
  * [File index](#file-index)
  * [Body scan](#body-scan)
  * [Comparison](#comparison)
* [Parameter sections](#parameter-sections)
* [symbol-order](#symbol-order)
  * [Checked scopes](#checked-scopes)
  * [Sort keys](#sort-keys)
  * [Minimal moves](#minimal-moves)
  * [symbol-order diagnostics](#symbol-order-diagnostics)
* [type-params-doc](#type-params-doc)
* [trait-params-doc](#trait-params-doc)
* [Suppression internals](#suppression-internals)
* [Diagnostics and output](#diagnostics-and-output)
* [Line ranges and the diff filter](#line-ranges-and-the-diff-filter)
* [Testing](#testing)
* [Known limits](#known-limits)
* [Rejected alternatives](#rejected-alternatives)
* [Open questions](#open-questions)

## Scope

The tool enforces y-scope Rust style rules that neither rustfmt nor clippy can check. It was built
against two code bases, [y-scope/clp](https://github.com/y-scope/clp) and
[y-scope/spider](https://github.com/y-scope/spider), and the motivating examples below come from
them.

Goals:

* No false positives on those code bases beyond a short, reviewed list, each of which is either a
  real violation or suppressed with a reason.
* Catch the real misses: under-filled comment lines, missing `Forwards` bullets, missing variant
  bullets, and stale bullets left behind by a refactor.
* Be cheap enough to run in CI on the changed lines only, including the lines a change deleted.

Non-goals:

* Requiring a `# Errors` section. Only existing sections are checked; `clippy::missing_errors_doc`
  covers their presence.
* Requiring docstrings. Every docstring rule skips an item without one: `missing_docs` and review
  cover their presence. `type-params-doc` and `trait-params-doc` do require their section in a
  docstring that exists.
* Type resolution. The tool works on syntax only and never runs rustc or rust-analyzer.
* Cross-file resolution, except the parent-file lookup for [out-of-line test
  modules](#out-of-line-test-modules).
* Auto-fixing. Messages say what to write, but the tool never rewrites files.

## Architecture

The package is a library with two thin binaries, edition 2024. The library holds the whole tool, and
its only public items are the two entry points, `main` and `cargo_main`. The `yscope-linter` binary
calls `main`, and the `cargo-yscope-linter` binary, which cargo runs for `cargo yscope-linter`,
calls `cargo_main`. Integration tests drive the built binaries, so nothing else needs a public
library surface.

### Module layout

Entry and configuration:

* `src/lib.rs`: module declarations, the two entry points, printing, and the exit-code mapping.
* `src/main.rs` and `src/bin/cargo-yscope-linter.rs`: the binaries, each one call to an entry point.
* `src/cli.rs`: the `clap` derive types of both command lines, which share `LintOptions`, and their
  conversion into a validated `LintConfig` or `CargoConfig`.
* `src/config.rs`: `LintConfig`, with the `CheckConfig`, the targets, the diff input, the excludes
  and the `OutputFormat`; `CargoConfig`, with the manifest path, the `PackageSelection`, the
  command-line `Settings`, the diff input and the `OutputFormat`; and `CheckConfig`, the enabled
  `RuleSet`, the `Width` and the `RuleSet` of the rules that skip test code, which the rules check a
  file with.
* `src/error.rs`: the fatal `Error` enum (I/O, invalid arguments, invalid diff, cargo failures,
  invalid settings, unknown packages), with a hand-written `Display` and `Error`.

Input selection:

* `src/source.rs`: target parsing, the directory walk, excludes, the `@generated` check, and the
  per-file `LineFilter`.
* `src/line_ranges.rs`: `LineSpan` and `LineRanges`, with parsing, union and intersection.
* `src/diff.rs`: a unified diff to per-file `LineRanges`.
* `src/cargo/`: the [cargo mode](#cargo-mode). `metadata.rs` runs `cargo metadata` and selects the
  packages, `settings.rs` parses and merges the `Settings`, and `plan.rs` builds the `Plan`: the
  files of each selected package, with its settings.

Per-file core:

* `src/driver.rs`: the run, and the per-file pipeline in `FileChecker`.
* `src/file_context.rs`: [`FileContext`](#filecontext), the per-file input of the rules.
* `src/lexer.rs`: the [comment lexer](#comment-lexer).
* `src/markdown/`: the [line-level Markdown classifier](#markdown-classifier) the comment and
  docstring rules share. `line.rs` recognizes indents, ATX headers, fences, list items and link
  reference definitions, `block_start.rs` classifies the block that a line opens, and `inline.rs`
  splits a paragraph into [inline tokens](#inline-tokens).
* `src/suppression.rs`: [suppression](#suppression-internals) parsing, coverage, and
  unused-suppression reporting.
* `src/diagnostic.rs`: diagnostic codes, positions, anchor spans, sorting and rendering.

Rule-agnostic helpers:

* `src/syntax/`: helpers over `syn` trees. `span.rs` converts span locations to positions,
  `item_lines.rs` finds the lines of every item or of one item or statement, `statement_lines.rs`
  finds the lines of every statement, `symbol_path.rs` normalizes paths, `types.rs` looks through
  parenthesized types, `test_code.rs` recognizes test attributes and out-of-line test modules,
  `visibility.rs` classifies visibilities into the ordering groups, `crate_names.rs` tells
  crate-local paths from foreign ones, and `doc_items.rs` walks the items whose docstrings document
  their signatures.
* `src/doc/`: helpers over docstrings. `doc_lines.rs` turns `#[doc]` attributes into positioned
  lines, `links.rs` finds code spans, links and link reference definitions, `section.rs` selects a
  `# Title` section and groups its lines, and `param_section.rs` parses and compares the sections
  that document parameters one bullet each.

Rules:

* `src/rules/registry.rs`: `RuleId` with each rule's name, code and deprecation state, `Code` and
  `CodeSet`, the `RuleSelector` that `--rule` and suppressions parse, and the `Rule` enum that
  dispatches a file to each enabled rule.
* `src/rules/greedy_wrap/`: `line_kind.rs` classifies lines, `paragraph.rs` splits blocks and runs
  the paragraph machine, `rustfmt_lists.rs` finds the lines that rustfmt wraps one column early, and
  `check.rs` holds the `Checker` with the fit test and the split-token check.
* `src/rules/errors_doc/`: `items.rs` (item walk), `errors_section.rs` (section parsing),
  `symbol.rs` (matching), `file_index.rs` (per-file index), `body.rs` (body scan), `pass_through.rs`
  (helper pass-through), `macros.rs` (macro handling), `known_apis.rs` (library API tables) and
  `check.rs` (the `Checker` and the comparison).
* `src/rules/symbol_order/`: `scope.rs` (the counted items of a scope and their keys), `keys.rs`
  (sort keys and reasons), `moves.rs` (the kept subsequence and placements) and `check.rs` (the
  `Checker`, the scope walk and the diagnostics).
* `src/rules/type_params_doc/` and `src/rules/trait_params_doc/`: each a `check.rs` with the rule's
  `Checker`, plus whatever modules the rule needs.

Tests:

* `tests/cli.rs` and `tests/cargo.rs`: end-to-end tests of the two binaries, with the helpers they
  share in `tests/support/`.

### Code conventions

* `mod.rs` files are tables of contents: declarations and named re-exports only.
* Items inside private modules use plain `pub`. `clippy::nursery` is on, and its
  `redundant_pub_crate` lint rejects `pub(crate)` there. All modules stay private in `lib.rs`, so
  nothing but the entry points escapes the crate.
* Unit tests live next to the code, in `#[cfg(test)] mod tests`.
* There are no tuple structs, including newtypes. `Width` is `struct Width { columns: NonZeroUsize
  }`.
* The tool dogfoods itself: `yscope-linter src tests` must be clean.

### Dependencies

`Cargo.toml` names every dependency down to its patch version, and `Cargo.lock` is committed so that
`cargo install --locked` reproduces a release.

* `clap` with `derive`.
* `proc-macro2` with `span-locations`, so that `Span::start()` returns real lines and columns
  outside a proc macro.
* `quote`, for `ToTokens`, which turns an item back into the tokens whose spans give the lines of an
  `allow-item` item.
* `serde` with `derive`, and `serde_json`, to read the output of `cargo metadata` and the
  `metadata.yscope-linter` tables in it. `serde_derive` uses the same `syn` as the tool, so the two
  add only `serde_core`, `itoa`, `memchr` and `zmij` besides themselves.
* `syn` with `default-features = false` and `full`, `parsing`, `printing` and `visit`. The body
  scanner never clones syntax, so `clone-impls` isn't needed.

There is no `regex`, `walkdir`, `thiserror`, `anyhow`, `cargo_metadata` or `toml`. The error enum is
small enough to write by hand, and `std::fs::read_dir` is enough for the walk. `cargo_metadata`
would add `camino`, `cargo-platform`, `semver` and `thiserror` to model fields the tool never reads,
and the settings would still need `serde`. `cargo metadata` already turns the `metadata` tables of
every `Cargo.toml` into JSON, so no TOML parser is needed either.

Adding `serde_json` made `assert_eq!(vec, [])` ambiguous in tests, since it implements
`PartialEq<Value>` for the primitive types; such assertions name the element type, as in
`Vec::<usize>::new()`.

## Pipeline

`driver::run` performs one `yscope-linter` run:

1. With `--diff`, it reads the diff (a file, or stdin for `-`) and parses it into per-file
   `LineRanges`. An invalid UTF-8 sequence in the diff is replaced with U+FFFD rather than rejected,
   since only the `+++` paths and the hunk headers are parsed.
2. `source::discover` merges the positional targets, the diff's files and the excludes into a sorted
   list of `SourceFile`s, each with a `LineFilter`.
3. It passes the files to `driver::check` as one `FileGroup`, with the `CheckConfig` of the command
   line.

A `cargo yscope-linter` run builds its `FileGroup`s with `cargo::Plan::new`, one per selected
package, as [Cargo mode](#cargo-mode) describes, and passes them to `driver::check`.

`driver::check` checks the files of each group with a `FileChecker` built from the group's
`CheckConfig`:

1. For each file, it reads the source. A read failure is a fatal `Error`.
2. It skips a file that `source::is_generated` marks, and counts it separately.
3. It runs `FileChecker::check` on every other file.
4. It sorts and deduplicates the diagnostics of all files.

`FileChecker::check` handles one file:

1. `FileContext::new` lexes the file and splits the suppression comments from the other comments.
2. Each enabled rule checks the file through the context, in `RuleId::ALL` order.
3. `FileContext::finish` adds the lex failure as a `parse-error`, locates `allow-item` items,
   extends own-line `allow`s through the statements they precede, drops the diagnostics that
   suppressions cover, and adds the `unused-suppression` and `invalid-suppression` diagnostics.
4. If the file has a `LineFilter::Ranges`, the diagnostics whose anchor span misses it are dropped.

`main` prints the diagnostics to stdout, then a one-line summary to stderr with the numbers of
diagnostics, checked files and skipped generated files. A fatal `Error` stops the run before
anything is printed to stdout and exits with 2. Otherwise the exit code is 1 when there is at least
one diagnostic, including a meta diagnostic, and 0 when there is none.

## Cargo mode

`cargo yscope-linter` replaces the targets of a `yscope-linter` run with the files of a cargo
workspace's packages, and lets each package set its own `CheckConfig`. Everything from
`driver::check` on is shared with the path mode.

Cargo runs a subcommand `cargo-<name>` with `<name>` as its first argument, so `cli::CargoCli` is a
`clap` enum with the single subcommand `yscope-linter`, named `cargo` for the usage line. Running
the binary without that first argument is a usage error.

### Workspace discovery

`metadata::Workspace::load` runs `cargo metadata --no-deps --format-version 1`, with
`--manifest-path` when it is given. It runs the cargo that `$CARGO` names, which cargo sets for its
subcommands, so the same toolchain answers, and `cargo` otherwise. Cargo's stderr is inherited, so
its errors and warnings reach the user as cargo prints them. `--no-deps` keeps the call offline and
fast, and makes `packages` the workspace members.

The tool reads only `packages` (each `name`, `id`, `manifest_path` and `metadata`),
`workspace_default_members`, `workspace_root`, `target_directory` and the workspace `metadata`.
These types don't deny unknown fields, since cargo adds fields over time. A `manifest_path` is
parsed into a `ManifestPath`, which rejects a path without a parent directory as
`Error::InvalidCargoMetadata`, so a package directory is never made up.

`Workspace::select` maps the `PackageSelection` to members:

* `Default` takes `workspace_default_members`. Cargo computes it for the selected manifest with the
  same rules as its own commands, so the tool doesn't reimplement them: a member's manifest selects
  that member, and the root manifest selects `default-members`, the root package, or every member of
  a virtual workspace.
* `Packages` takes the members that `-p` names, by package name. Cargo's package ID specs and globs
  aren't supported.
* `Workspace` takes every member but the ones `--exclude-package` names. `--workspace` takes
  precedence over `-p`, as in cargo.
* A name that matches no member is `Error::PackageNotFound`.

### Package files

`Plan::new` turns each selected package into a `FileGroup`:

* Paths are made absolute against the current directory and resolved lexically (`resolve`), so that
  the excludes, the target directory, the package directories and the diff's paths compare component
  by component.
* The package directory is walked with `source::walk_directory`, which skips what a directory target
  skips, plus the directories that the plan rejects: the workspace's target directory, which can be
  inside a package under any name (`CARGO_TARGET_DIR=out`), and every directory with its own
  `Cargo.toml`. The latter is how cargo bounds a package's files too. It keeps a nested member from
  being linted twice, and keeps a nested non-member, such as a test fixture crate, out entirely.
* A file under any exclude of the package's merged settings is dropped.
* With `--diff`, a file is kept only if the diff changes it, with those lines as its
  `LineFilter::Ranges`. The diff's files outside the selected packages are ignored.
* Each path is made relative to the current directory for output (`relative_to`), with `..`
  components when the package is outside it.

The whole package directory is walked rather than the targets' `src_path` directories, so that the
result equals a path-mode run on the package directories. A file that no target compiles, such as a
stray `.rs` file in a package, is linted as well.

### Settings

`settings::Settings` holds the settings that one source gives, each `None` when the source leaves it
unset: `rule_selectors`, `width`, `skip_tests`, and the `excludes`, which are always a list.
`skip_tests` is a `SkipTests`: `All`, `Off`, or `Listed` with the `RuleSelector`s of a list.

* `Settings::from_table` parses a `metadata.yscope-linter` table into `RawSettings`, a serde struct
  with `deny_unknown_fields` and kebab-case keys. The table is first kept as a `serde_json::Value`
  in `MetadataTable`, so that its errors are wrapped into `Error::InvalidSettings` with the table
  and the manifest named. A JSON position in the whole `cargo metadata` output wouldn't help the
  user. The other keys of `metadata` belong to other tools and are ignored.
* `rules` parses through `RuleSelector`'s `try_from = "String"`, with the same messages as `--rule`.
* `skip-tests` parses through a hand-written `Deserialize` for `SkipTests`, whose visitor takes a
  boolean or a sequence. Each element parses as a `RuleSelector`, so a list has the messages of
  `rules`, and any other type is an error that expects `a boolean or an array of rule IDs`. An
  untagged enum would replace both messages with one that names no variant.
* `width` is a `NonZeroUsize`, so 0 is rejected while parsing.
* `from_table` joins each exclude of a table to the directory of the `Cargo.toml` that declares it.
  The command line's excludes are kept as written, and `Plan::new` resolves every exclude against
  the current directory, so a relative one on the command line is relative to it. `SettingsTable`
  names the table in errors and warnings.
* `overridden_by` keeps each setting of the overriding source when it is set, and appends the
  excludes. `Plan::new` folds the workspace's settings, the package's and the command line's in that
  order, and `check_config` fills the unset ones with the defaults: every rule, `Width::DEFAULT` and
  each rule's default for test code in `RuleId::test_code_mode`.
* A `rules` or `skip-tests` list replaces the one it overrides, as every setting but `excludes`
  does. A package that adds one rule to the workspace's `skip-tests` list repeats the workspace's
  rules.
* `check_config` resolves `skip_tests` into `CheckConfig::test_skipping_rules`: every rule for
  `All`, none for `Off`, and for `Listed` the rules that the selectors name plus the
  `SkippedByDefault` rules. Without a setting, it holds the `SkippedByDefault` rules. `Rule::new`
  gives each checker its `CheckConfig::skips_tests`.
* `Settings::warnings` warns about each deprecated rule in a `rules` or a `skip-tests` list, with
  the key that names it, and about each rule without a test-code mode that a `skip-tests` list names
  by its name or code.

The command line sets `skip_tests` with `--skip-tests`, which takes an optional list, and
`--no-skip-tests`:

* `--skip-tests` is an `Option<Vec<RuleSelector>>` with `num_args = 0..=1`, `require_equals` and
  `value_delimiter = ','`. Bare, it gives an empty list, which becomes `All`. With a value, it gives
  the list, which becomes `Listed`. `--skip-tests=` and an empty entry, as in `ORG001,`, are the
  empty selector, an unknown rule, so an empty list always means the bare flag.
* `require_equals` keeps `--skip-tests ORG001` from taking `ORG001`. It stays a `TARGET`, as it
  would be before the list existed.
* `--no-skip-tests` gives `Off`, and neither flag gives `None`. So the command line overrides a
  `skip-tests` setting in both directions.
* `--skip-tests` overrides itself and `--no-skip-tests` with clap's `overrides_with_all`, so the
  last of them wins, and a later list replaces an earlier one instead of adding to it.
* `LintOptions::warnings` warns about each deprecated rule in `--rule` or `--skip-tests`, with the
  flag that names it, and about each rule without a test-code mode that `--skip-tests` names by its
  name or code.
* `--include-tests`, from when test code was skipped by default, is a hidden flag that has no
  effect. `LintOptions::warnings` warns about it too.
* The `include-tests` setting of the unreleased cargo integration isn't accepted. It is an unknown
  key, whose error lists `skip-tests`.

The path mode never reads `Cargo.toml`. Both command lines turn their shared `SettingsOptions` into
`Settings` with the same `From` impl, and `Cli::into_config` builds its `CheckConfig` through
`Settings::check_config` too, so the mapping and the defaults live in one place.

## FileContext

`FileContext` is what the rules see of one file. It is built once per file and shared by every
enabled rule:

* `comment_lines()`: the lexed comment lines without the suppression candidates and the lines that
  their reasons continue on.
* `syntax_tree(rule_id)`: the `syn::File`, parsed on the first call and kept in a `OnceCell`.
* `optional_syntax_tree()`: the same tree for a rule that can do without it. A parse failure records
  nothing, so it never adds a `parse-error`.
* `is_test_module(rule_id)`: whether the file is an [out-of-line test
  module](#out-of-line-test-modules), decided on the first call with the run's `TestModuleCache`.

`syntax_tree` returns `None` in two cases, and the rule then checks nothing:

* The file failed to lex. A lex failure is an unterminated literal or block comment, which `syn`
  can't parse either, and the lex failure is already a `parse-error`.
* `syn::parse_file` fails. The context records `rule_id` as skipped, so only the rules that need the
  tree are skipped.

`is_test_module` records `rule_id` when a candidate declaring file can't be read or parsed, since
the verdict may then be wrong.

Both records become one meta diagnostic each in `finish`, whatever the number of rules that asked:

* `syn failed to parse the file (...); errors-doc, symbol-order are skipped`, at the `syn` error.
* `` can't tell whether this file is a `#[cfg(test)]` module: ...; errors-doc, symbol-order check it
  as non-test code ``, at line 1.

The rules are listed in `RuleId::ALL` order, with `is`/`checks` for one rule. A run with only
`errors-doc` keeps the messages it had before the other rules existed.

A file is parsed at most once, and not at all when no enabled rule needs the tree, no `allow-item`
needs its items and no own-line `allow` precedes a line that isn't a comment. So a
`comment-greedy-wrap`-only run parses only the files that have an `allow-item`, such an `allow` or a
[move that fills a line to the width](#rustfmt-hint).

After the lex failure, the comment rule and the suppressions work on the comments lexed before it.

`finish(rule_diagnostics, rule_set)` consumes the context and completes the file's diagnostics, as
step 3 of the [pipeline](#pipeline) describes.

## Rule registry

Every rule is reached through `src/rules/registry.rs`:

* `RuleId` names a rule. `RuleId::ALL` lists the rules in run order, `RuleId::name` gives each its
  kebab-case name, `RuleId::code` its stable `Code`, `RuleId::is_deprecated` its deprecation state,
  and `RuleId::test_code_mode` its `TestCodeMode`: `Unsupported` for a rule without a test-code
  mode, and otherwise `CheckedByDefault` or `SkippedByDefault`, whether it checks test code without
  a `skip-tests` setting. These matches are the single source of truth for the rules' metadata.
  Every current rule but `comment-greedy-wrap` is `CheckedByDefault`. `comment-greedy-wrap` is
  `Unsupported`, so its checker never consults the setting.
  `RuleSelector::named_rule_ids_without_test_code_mode` picks out the selectors that name such a
  rule by its name or code, and naming one in a `skip-tests` list prints a warning.
* `Rule` holds an enabled rule's checker, with its settings and the state it keeps across the files
  of a run. `Rule::new` builds it from the `LintConfig`, and `Rule::check` dispatches a
  `FileContext` to it.

The checkers keep no state across files. The cache of [out-of-line test
module](#out-of-line-test-modules) verdicts is shared by every rule, so `FileChecker` owns it and
lends it to each file's `FileContext`. `TestModuleCache` keeps its interior mutability private. The
`comment-greedy-wrap` checker holds the width, and the other checkers hold `skip_tests`, which
`Rule::new` takes from `CheckConfig::skips_tests` for each rule.

### Codes and selectors

The naming standard and the deprecation policy are user-facing, in
[`docs/rules/README.md`](rules/README.md). In the code:

* `Code` is a `CodeSet` and a number, and displays as `DOC003`. `CodeSet` is `Doc`, `Org` or `Lnt`,
  with its three-letter prefix.
* `DiagnosticKind::code` gives the code of every diagnostic. A rule's code comes from
  `RuleId::code`, and the meta codes `LNT001` to `LNT003` from `DiagnosticKind` itself.
* `RuleSelector` is what `--rule` and a suppression name: `Rule(RuleId)`, matched by name or by
  code, or `Set(CodeSet)`, matched by prefix. `RuleSelector::rule_ids` expands a set to its rules
  that aren't deprecated. `FromStr` is case-sensitive, and rejects a meta code, by name, by code or
  as `LNT`, with `InvalidRuleSelector::MetaCode`. Any other miss is `InvalidRuleSelector::Unknown`,
  whose message suggests the uppercase spelling when that one parses.
* `clap` parses `--rule` through `RuleSelector::from_str`, so an invalid value is a usage error.

The deprecation state is enforced in three places:

* `RuleSet` drops deprecated rules when it is built, so `FileChecker` never builds or runs one.
* `Cli::warnings` returns one warning per deprecated rule that `--rule` or `--skip-tests` names,
  which `main` prints to stderr before the run. `Settings::warnings` does the same for the `rules`
  and `skip-tests` settings of each table.
* `Directive::parse` splits the rules a suppression names into the active and the deprecated ones.
  Only the active ones get a `Suppression`, and `suppression::parse` reports each deprecated one as
  an `unused-suppression`, as [Suppression internals](#suppression-internals) describes.

No rule is deprecated today, so these paths have no unit tests. They are tested through the first
rule that is deprecated.

### Adding a rule

1. Pick the set: `DOC` for a rule about comments and docstrings, `ORG` for one about code
   organization. A rule that fits neither needs a new `CodeSet`, which is a decision for the
   maintainers.
2. Take the next free number in the set: one more than the highest code in it, deprecated rules
   included. Never reuse a number.
3. Add a `RuleId` variant, add it to `RuleId::ALL`, and give it its name in `RuleId::name`, its code
   in `RuleId::code`, `false` in `RuleId::is_deprecated`, and its `TestCodeMode` in
   `RuleId::test_code_mode`. `--rule`, `--skip-tests`, suppression parsing, set selectors and the
   unknown-rule message pick it up from there.
4. Add the rule's module under `src/rules/`. Its `Checker` holds the rule's settings and cross-file
   state, and its `check` takes the `FileContext` and returns the rule's diagnostics.
5. Add a `Rule` variant that holds the checker, with its arms in `Rule::new` and `Rule::check`.
6. Give each diagnostic an anchor span that covers every line whose change can create the finding,
   so that the [diff filter](#line-ranges-and-the-diff-filter) keeps it. Set a fn header if a
   suppression above the item should cover it.
7. Add the code and the name to the frozen table of `codes_are_frozen` in `src/diagnostic.rs`.
8. Document the rule in a page under `docs/rules/`, named after the rule and showing its code. Add
   it to the index in `docs/rules/README.md` and to the README's rule table, and add a section to
   this document.

A rule reads the file only through its `FileContext`. When `syntax_tree` returns `None`, the rule
checks nothing; the context reports the `parse-error`. A rule that can skip test code asks
`is_test_module` only when it skips test code, and checks nothing for a test module. Rule-agnostic
helpers belong in `src/syntax/` and `src/doc/`, next to `src/lexer.rs` and `src/markdown/`.

### Deprecating a rule

A rule is never deleted from the registry:

1. Return `true` for it in `RuleId::is_deprecated`. Keep its variant, its name and its code, and
   keep its row in the frozen table.
2. Add tests of the deprecation paths through it: `RuleSet`, set selectors, `Cli::warnings`,
   `Directive::parse` and `FileChecker::check`.
3. Delete its checker module and its `Rule` variant. `Rule::new` then has no checker for it, and its
   arm is unreachable, since `RuleSet` never holds a deprecated rule.
4. Mark it deprecated in the index of `docs/rules/README.md` and in the README, and say on its page
   what replaced it. Keep the page, since its URL is built from the rule's name.

## Comment lexer

`src/lexer.rs` is a hand-written, single-pass state machine over the file's `char`s, with one
character of lookahead (two for the char-versus-lifetime decision). It tracks lines and columns, and
builds no token list. `lex` returns a `Lexed`: the comment lines, an optional `LexError`, and the
end of the file header.

States:

* `Code`: the start of the file, and after any literal or comment.
* Line comment: `//` in `Code`, until the end of the line. It records a `CommentLine`.
* Block comment: `/*` in `Code`. A nested `/*` increments the depth, and `*/` decrements it.
* String: `"`, `b"` or `c"` at a token start, until an unescaped `"`. A `\` skips the next char,
  including a newline.
* Raw string: `r`, `br` or `cr`, then `n` times `#`, then `"`, at a token start, until `"` followed
  by exactly `n` times `#`.
* Char: a `'` or `b'` that starts a char literal, until the closing `'`.

Decisions in `Code`:

* Token start: the `r`, `b`, `c`, `br` and `cr` prefixes count only when the preceding char isn't an
  identifier char, so `bar"` and `for"` never start a literal.
* Raw identifier: `r#` followed by an identifier char is a raw identifier (`r#type`) and stays in
  `Code`.
* Char versus lifetime: after `'`, a next char of `\` starts a char literal (`'\n'`), and so does a
  `'` two chars ahead (`'x'`, `'é'`). Anything else is a lifetime or a label (`'a`, `'outer:`).
* Shebang: a first line that starts with `#!` but not `#![` is skipped.

Nothing inside a string, raw string, char or block comment produces a `CommentLine`, so
`"http://x"`, `r#"// x"#` and `/* // */` are never comments. Block doc comments (`/** */`, `/*! */`)
are not extracted.

A `CommentLine` holds:

* `line` and `marker_column`: 1-based, with columns in Unicode scalar values.
* `marker`: `CommentMarker::{Plain, OuterDoc, InnerDoc}`. `////` and longer runs are `Plain`, as
  rustdoc treats them.
* `num_slashes`: the length of the slash run, so that two markers can be compared without storing
  their text.
* `text`: everything after the marker, without a trailing `\r`.
* `is_own_line`: whether only whitespace precedes the marker.
* `width`: the scalar count of the whole source line, with trailing whitespace removed.

`CommentLine::continues_block` decides whether a line continues the previous one's [block](#blocks).

The lexer also tracks the file header in a small `Header` state machine: leading whitespace, non-doc
comments, inner doc comments and inner attributes (with nested brackets). The first outer doc
comment, outer attribute or other token ends the header, and its position is `header_end`, which
[`allow-file`](#suppression-internals) placement is checked against without a parse.

An unterminated string, raw string, char or block comment stops the lexer with a `LexError` at the
token's start. The comment lines before it are kept.

## Markdown classifier

`src/markdown/` classifies one line's text after its comment marker and the one space that usually
follows it. Both rules use it, so a doc line plays the same role in `comment-greedy-wrap` as in the
`# Errors` parser.

`line.rs` holds the recognizers that the docstring rules read sections by:

* `indent`: the count of leading spaces and tabs.
* `atx_header`: an indent of at most 3, then 1 to 6 `#`, then whitespace or the end of the line. A
  closing `#` run isn't part of the title. `#544)` and `#[derive]` are not headers.
* `Fence::parse`: three or more backticks or tildes after any indent. A backtick fence's info string
  can't hold a backtick.
* `FenceTracker::is_code`: whether a line is a fence line or inside a fenced block. A fence closes
  only on a fence line of the same character, at least as long, with no info string.
* `bullet_marker`: `*`, `-`, `+`, or 1 to 9 digits and `.` or `)`, followed by at least one space or
  tab. Its content indent is the indent, plus the marker, plus that whitespace.
* `link_definition`: `[label]:` with an indent of at most 3 and a non-blank label without brackets,
  where the `:` ends the line or is followed by whitespace.

These accept any indent where a docstring section needs it, such as a nested bullet. They stay as
they are, so the docstring rules don't change with the block-start classifier.

### Block starts

`block_start.rs` is a formal classifier of where a block starts, after [CommonMark
0.31.2](https://spec.commonmark.org/0.31.2/) and the GFM extensions that rustdoc enables through
pulldown-cmark: tables, footnotes, strikethrough and task lists. `BlockStart::parse(line,
preceding)` takes the line's text relative to its container, the list item it is in, and returns
the block the line opens, if any. `Preceding` says what comes before the line, since that decides
which blocks can start:

* `NoParagraph`: the first line of a container, or a line after a blank line or another block.
  Every block can start.
* `Paragraph`: a paragraph line of the same container. Only a block that
  `BlockStart::can_interrupt_paragraph` allows ends the paragraph, and a setext heading underline
  turns it into a heading. Anything else is [paragraph continuation
  text](https://spec.commonmark.org/0.31.2/#paragraph-continuation-text).
* `LazyParagraph`: a paragraph line of a list item that the line isn't indented enough to belong
  to. The line would be a [lazy continuation
  line](https://spec.commonmark.org/0.31.2/#lazy-continuation-line), so it opens a block of its own
  container: any list item, but no setext underline, link reference definition, indented code or
  start condition 7.

The blocks, with the spec section each one follows:

| `BlockStart` | Recognized as | Interrupts a paragraph |
| ------------ | ------------- | ---------------------- |
| `AtxHeading` | 1 to 6 `#`, then a space, a tab or the end ([4.2](https://spec.commonmark.org/0.31.2/#atx-headings)) | yes |
| `FencedCode { fence }` | 3 or more backticks or tildes, no backtick in a backtick fence's info string ([4.5](https://spec.commonmark.org/0.31.2/#fenced-code-blocks)) | yes |
| `BlockQuote` | `>` ([5.1](https://spec.commonmark.org/0.31.2/#block-quotes)) | yes |
| `ThematicBreak` | 3 or more of one of `*`, `-`, `_`, with only spaces and tabs ([4.1](https://spec.commonmark.org/0.31.2/#thematic-breaks)) | yes |
| `SetextUnderline` | `=+` or `-+`, then only spaces and tabs, right after a paragraph line ([4.3](https://spec.commonmark.org/0.31.2/#setext-headings)) | yes, and the paragraph becomes the heading |
| `BulletItem { content_indent, is_empty }` | `-`, `+` or `*`, then a space, a tab or the end ([5.2](https://spec.commonmark.org/0.31.2/#list-items)) | only if not empty |
| `OrderedItem { start, content_indent, is_empty }` | 1 to 9 digits, `.` or `)`, then a space, a tab or the end | only if `start` is 1 and not empty |
| `HtmlBlock { condition }` | start conditions 1 to 7 ([4.6](https://spec.commonmark.org/0.31.2/#html-blocks)) | conditions 1 to 6 |
| `FootnoteDefinition` | `[^label]:`, as pulldown-cmark recognizes it | yes, in pulldown-cmark |
| `LinkReferenceDefinition` | `link_definition` ([4.7](https://spec.commonmark.org/0.31.2/#link-reference-definitions)) | no |
| `IndentedCode` | an indent of 4 columns or more ([4.4](https://spec.commonmark.org/0.31.2/#indented-code-blocks)) | no |

Details:

* Every block but `IndentedCode` allows an indent of up to 3 columns. A tab advances to the next
  multiple of 4 columns, so `\t# x` is indented code.
* A thematic break takes precedence over a list item, so `* * *` and `- - -` are breaks. After a
  paragraph line, a setext underline takes precedence over both, so `---` and `-` are underlines
  there.
* A list item's content indent is the number of bytes before its text: the indent, the marker and
  the whitespace after it. With 5 or more columns of whitespace, the content starts one byte after
  the marker, and the rest is indented code. An empty item's content starts one byte after the
  marker too.
* Columns, in which a tab advances to the next multiple of 4, are a `Column` of their own, and only
  decide which block a line opens. Content indents stay byte counts, since the paragraph machine
  compares them with the byte indents of later lines and slices lines with them. For `- \titem`, the
  content indent is 3, although the text starts at column 4.
* `HtmlBlockCondition` names the seven conditions: `RawText` (`<pre`, `<script`, `<style`,
  `<textarea`), `Comment`, `ProcessingInstruction`, `Declaration`, `Cdata`, `BlockTag` (the 62
  block-level tags of the spec, which pulldown-cmark shares) and `CompleteTag` (a complete open or
  closing tag, then only whitespace). `HtmlBlockCondition::is_ended_by` applies each one's end
  condition to a line.
* A link reference definition is `link_definition`, the same recognizer the docstring rules use. It
  requires whitespace or the end of the line after the `:`, which CommonMark doesn't. A definition
  can't interrupt a paragraph, so the difference never changes where a block starts.
* A table can't be told from one line: it needs its delimiter row. A `|` line opens no block in the
  classifier. GFM tables can't interrupt a paragraph. pulldown-cmark lets a table whose header row
  starts with `|` interrupt one, which the one-line classifier can't see.

### Inline tokens

`inline.rs` reads a whole paragraph, since a code span or a link can span lines.
`ParagraphText::new` takes the paragraph's lines, each without its prefix and surrounding
whitespace, and joins them with one space each, as Markdown renders a line break inside a code span
or link text. `ParagraphText::tokens` splits the text into `Token`s with a hand-written scanner:

* A token is a maximal run without whitespace, except that whitespace inside a code span or a link
  doesn't end it, so the characters glued to either belong to its token.
* `CodeSpan::parse` matches a backtick run with the next run of exactly the same length, as in
  CommonMark. A regex can't, since it needs a backreference, which the `regex` crate doesn't have.
* A link is a `[` and the `]` that `closing_bracket` balances it with, skipping nested brackets,
  backslash escapes and code spans, then a `(dest)` with balanced parentheses, if one follows. A
  `[ref]` after the text is a glued link of its own, so a reference link is one token too.
* A backslash before ASCII punctuation escapes it, so `` \` `` and `\[` open nothing. A backtick run
  without a closing run, or a `[` without a matching `]`, is plain text.

A `Token` has its `text`, its `width` in scalars, its `start` and `end` as `TextPosition`s (a line
index and a byte offset in that line), and its `split_kind`: whether the first line break inside it
falls in a `CodeSpan` or a `Link`, or `None` if the token is on one line. `Token::pieces` splits a
token at its whitespace into single-line tokens. The scanner knows nothing about widths, so a
formatter that refills a paragraph can reuse it: strip each line's prefix, place the tokens
greedily, replace a token wider than a line by its pieces, and add the prefix back.

`doc/links.rs` resolves references with the same `CodeSpan::parse` and `closing_bracket`.

## comment-greedy-wrap

The user-facing rule, with an example of every exemption, is in
[`docs/rules/comment-greedy-wrap.md`](rules/comment-greedy-wrap.md). The `Checker` runs these steps:
`paragraph::blocks` splits the comment lines into blocks, `paragraph::prose_paragraphs` finds the
prose paragraphs of two or more lines in each block, and `ParagraphText::tokens` splits each
paragraph into tokens. Then the fit test runs on each pair of consecutive lines of a paragraph, and
the split-token check on each token.

### Fit test

A pair `(L, N)` is a violation when, for some `k >= 1`,

```text
width(L) + 1 + width(first_tokens(N, k)) <= width
```

and the rest of `N` after its first `k` tokens is empty or opens no block, by `BlockStart::parse`
with the paragraph's `Preceding`. A rest that opens a block would leave the moved tokens' line
followed by a list item, a heading or another block, so the fill would change the Markdown.

`width(L)` is `CommentLine::width`, the rtrimmed width of the whole source line (indent, marker and
text). `first_tokens(N, k)` is `N`'s body from its first token to the end of its `k`-th token,
whitespace included. The diagnostic names the smallest valid `k`, in the `first word` form for 1
and the `first <k> words` form otherwise.

`N`'s tokens are the paragraph's tokens that are on `N`, taken in order:

* A token no wider than `N`'s capacity, the width minus `N`'s prefix (`CommentLine::width` minus
  the trimmed body's width), is one token of `N`.
* A wider token is replaced by its `pieces`. The pieces on lines before `N` are skipped.
* The list ends at the first token that doesn't start and end on `N`: the rest of a split token
  that fits on a line, which the split-token check reports, a token that runs on to the next line,
  or the next line's first token.

A piece wider than the room ends the search like any other token, so a long URL is never flagged.

The diagnostic is at `L`'s marker column. Its anchor span is `L` through `N`, since a change to
either line can create the violation.

### Split tokens

A token whose `split_kind` is set, and that is no wider than the capacity of the line after the one
it starts on, is split although it fits on a line. It gets the `code span ... is split across lines`
or `link ... is split across lines` diagnostic, at the marker of its first line, with an anchor span
over all of its lines. The pairs it spans get no fit-test diagnostic, since their next line starts
with its rest.

rustfmt fills the first paragraph of a list item at whitespace (its `ItemizedBlock`), with `*`, `-`
and `+` bullets and 1- or 2-digit ordered items, so it splits code spans and links there and splits
them again after a manual fix. `ProseParagraph::start` is `ProseStart::ListItem` for a paragraph
that starts on a `Bullet` line, and a split token in one gets a hint to reword the item instead,
like the rustfmt hint below.

### rustfmt hint

Nightly rustfmt wraps the comments in a vertical list of fields or parameters one column early
([rust-lang/rustfmt#6180](https://github.com/rust-lang/rustfmt/issues/6180)): its
`rewrite_aligned_items_inner` reserves a column for the separator. A line filled to exactly the
width there is wrapped again by rustfmt, so the rule and rustfmt can't both be satisfied, and the
paragraph has to be reworded. The rule stays strict at the width everywhere, but its message says
so.

`Checker::check` first finds every under-filled line. Only if some move fills a line to exactly the
width does it ask for `optional_syntax_tree`, and `rustfmt_lists::early_wrap_lines` collects the
lines strictly between the delimiters of every `FieldsNamed`, `FieldsUnnamed` and fn `Signature`
parameter list. These cover the fields of structs, unions and enum variants, named and tuple, and
the parameters of fns, methods and trait method declarations, including plain `//` comments and a
comment after the last field. A diagnostic whose move fills its line to the width, on a line in
those ranges, gets the hint. A file that fails to parse gets no hint and no `parse-error`. Comments
inside array literals, which rustfmt wraps early too, get no hint.

### Blocks

A block is a maximal run of own-line comments on consecutive lines with the same `marker`,
`num_slashes` and `marker_column` (`CommentLine::continues_block`). Two lines in different blocks
are never in the same paragraph.

A trailing comment is a block of its own, so it is never checked. Suppression candidates, and the
lines that the reasons of own-line ones continue on, are removed from the comment stream before the
rule runs, so a suppression comment leaves a gap in the line numbers and always ends a block.

### Line kinds

Each line's body is its `text` without one leading space. `Body::kind(container_indent, preceding)`
classifies it, trying these kinds in order:

1. `Blank`: empty after trimming.
2. `TableRow`: starts with `|` after trimming.
3. `LinkDef`: `link_definition` matches the body.
4. The block that `BlockStart::parse` finds in the body's text after `container_indent`, with
   `preceding`:
   * `AtxHeading` is `Header`, and `FencedCode` is `Fence { fence }`.
   * `BlockQuote`, `ThematicBreak` and `SetextUnderline` keep their names.
   * `BulletItem` and `OrderedItem` are `Bullet { content_indent }`, with the content indent in
     bytes, counted from the start of the body.
   * `HtmlBlock` is `Html { condition }`.
   * `FootnoteDefinition` and `LinkReferenceDefinition` are `LinkDef`.
   * `IndentedCode` falls through, since the paragraph machine treats indented text by its indent.
5. `Tagged`: starts with one of `NOTE`, `TODO`, `FIXME`, `SAFETY`, `XXX`, `HACK`, `WARNING` or
   `IMPORTANT`, followed by `:`.
6. `Formula`: a first token of identifier characters, `.`, `:`, `[`, `]`, `(` and `)` that starts
   with a letter or `_`, then one of `=`, `==`, `!=`, `<=`, `>=`, `+=`, `-=`, `*=`, `/=`, `:=`, `->`
   or `=>` as the second whitespace-separated word, then more text.
7. `Text`: anything else, including `<https://...>` autolinks and inline HTML.

`TableRow`, `LinkDef`, `Tagged` and `Formula` are the rule's own exemptions. They apply whatever
precedes the line, although in Markdown a table row, a definition, a tag or a formula continues a
paragraph.

### Paragraph machine

`ParagraphMachine` walks one block from top to bottom. It is created per block, so its state never
crosses a block boundary. It keeps:

* `paragraph`: the current paragraph, if any. It is `Prose { continuation_indent, start }`,
  `LinkDef { def_indent }`, `Preformatted`, `Html { condition }` or `FencedCode { fence }`.
* `list_indents`: the content indents of the list items still open, in increasing order. A list item
  stays open across blank lines, so a loose item's later paragraphs are still prose.

`step` moves the machine to the next line `N` and returns a `Step`: `N` continues the previous
line's paragraph, ends it, or underlines it as a setext heading. `prose_paragraphs` keeps the lines
of the current paragraph and drops them on an underline, so no line of a setext heading is checked.

A line at the continuation indent of the current `Prose` paragraph is classified with that indent as
its container and `Preceding::Paragraph`, or `Preceding::LazyParagraph` if the paragraph starts with
a `ProseStart::LazyContinuation` line. Any other line is classified with `Preceding::NoParagraph`,
inside the deepest open list item whose content indent is at most the line's indent. `N` is joinable
only when it is a `Text` line at the continuation indent of a `Prose` paragraph. Every step also
updates the state:

* A line inside a fence ends the paragraph, and a matching fence line closes the fence.
* A line inside an HTML block is raw. The line that meets the block's end condition closes it.
* `Fence` opens a fence and ends the paragraph.
* `Blank`, `TableRow` and `BlockQuote` end the paragraph.
* `ThematicBreak` ends the paragraph and closes the list items deeper than its indent.
* `Html` opens an HTML block, unless its first line already meets the end condition.
* `Header` and `SetextUnderline` end the paragraph and close every list item.
* `Formula` starts a one-line `Preformatted` paragraph, so it is joined with neither neighbor.
* `LinkDef` starts `LinkDef { def_indent }`. The following `Text` lines indented deeper than
  `def_indent` are its target and are never checked.
* `Bullet` closes the list items deeper than its own indent, pushes its content indent, and starts
  `Prose` at that content indent.
* `Tagged` starts `Prose` at its own indent.
* A `Text` line that doesn't continue the paragraph starts a new one. With indent 0, it starts
  `Prose { continuation_indent: 0 }` and closes every list item. With an indent in `list_indents`,
  it starts `Prose` at that indent and closes the deeper items. With any other indent, it starts
  `Preformatted`.

A `Text` or `Tagged` line starts a `Prose` paragraph as a `LazyContinuation` when the paragraph
before it is prose in a deeper list item than the line's container. Markdown makes such a line a
lazy continuation of the item, so a following `2.` starts a sibling item there, while after a
paragraph of the same container it continues the paragraph.

### Design notes

A pairwise check, which looks only at the two lines of a pair, gives false positives on the y-scope
code bases. The machine's state exists to avoid them:

* A header is never filled, and nothing is pulled up onto it, in either direction. A pairwise check
  flags `/// # Returns` followed by prose.
* Headers are ATX headers with an indent of at most 3 and whitespace after the `#` run. Any line
  starting with `#` would make `// #544).` after `// TODO: ... (see` a header.
* A fence closes only on a matching fence line, so `~~~` inside a backtick fence is code.
* Indented text outside a list item is `Preformatted`, which covers aligned case lists in plain
  comments such as `// index 0: alive EM, terminated -> removed`.
* `Formula` covers pseudo-code written one statement per line, such as `// deadline = now - 900ms`
  in `components/spider-storage/src/task_instance_pool.rs`.
* `Tagged` makes a `// NOTE:` line stacked under another comment start its own paragraph.
* `LinkDef` keeps the long target of an intra-doc link reference definition from being treated as
  prose.

A trailing `:` is deliberately not a boundary, in plain and doc comments alike. A list after a colon
is written as bullets or as indented, preformatted lines (see [Rejected
alternatives](#rejected-alternatives)).

`BlockQuote`, `ThematicBreak`, `SetextUnderline`, `Html`, `+` and `N)` bullets and `~~~` fences have
no use in the y-scope code bases. They stay because the block-start classifier follows CommonMark,
and none of them misfires.

The paragraph machine and the fit test share one definition of where a block starts. The machine
asks whether `N` opens a block after `L`, and the fit test asks whether the rest of `N` would, after
the same `L`. So a line that the machine joins as continuation text is never made a block by a
move, and a fill that the rule asks for never changes the rendered structure. Before the classifier,
the fit test moved only the first word, and Spider's
`components/spider-storage/src/state/test_utils.rs` had a false positive: moving `` `num_children ``
up left ``- 1`, in order.``, a list item that splits the code span.

Classification is relative to the open list item, as in CommonMark. A list item indented by 4 or
more columns from its container is indented text, not a nested item, and starts a `Preformatted`
paragraph like any other indented text outside a list item.

## errors-doc

The accepted documentation format, the findings and their messages are in
[`docs/rules/errors-doc.md`](rules/errors-doc.md). This section describes how the rule derives what
a body returns and compares it with what its docstring says.

The model has three parts:

* The [section parser](#parsing-the--errors-section) turns the `# Errors` section into documented
  variants, documented forwards and prose links.
* The [body scan](#body-scan) turns the fn body into obligations (what must be documented), opaque
  sites (where an error leaves the fn unattributed), and weaker facts used only to keep bullets from
  going stale.
* The [comparison](#comparison) pairs the two sides with the [matching
  rules](#symbol-paths-and-matching), and reports missing and stale documentation.

### Stages

`errors_doc::Checker::check` runs these stages on each file:

1. It gets the syntax tree from the `FileContext`, or checks nothing.
2. If it skips test code, it asks the `FileContext` whether the file is an [out-of-line test
   module](#out-of-line-test-modules). A test module is skipped. A declaring file that can't be read
   or parsed gives a `parse-error` at line 1, and the file is checked unless another candidate marks
   it as a test module.
3. It builds the [`FileIndex`](#file-index) and its qualifier links.
4. `items::collect` walks the file for checkable fns.
5. For each fn, `doc_lines::collect` and `errors_section::parse` find its section. A fn without one
   is skipped.
6. A trait fn without a default body gets only its section's bullet format findings, without the
   intro finding. Its section documents a contract, not a body, and a contract such as
   "Implementations must document their error conditions." has no bullets to introduce.
7. Any other fn's block is scanned by `body::scan`, and `FnCheck::findings` compares the section
   with the body's facts.

Every `errors-doc` diagnostic has the whole fn as its anchor span, from its first attribute or doc
line to its closing brace, and the fn's header as its fn header, from its first attribute or doc
line through its `fn` keyword.

### Item walk

`ItemWalker` implements `syn::visit::Visit` over the file with a stack of `ItemContext`s, each
holding the self names, the generic type parameters, whether its fns are associated, and the self
type of a trait impl:

* `visit_item_impl` pushes the self type's last segment and the trait's name as self names, with the
  impl's type parameters, and marks the context as associated. For a trait impl, it also records
  the self type's last segment as `trait_impl_self_type`, unless the self type isn't a path or is a
  type parameter of the impl, as in `impl<T: Inner> Outer for T`.
* `visit_item_trait` pushes the trait's name, with its type parameters, as an associated context.
* `visit_item_fn`, `visit_impl_item_fn` and `visit_trait_item_fn` record the fn, then recurse into
  its body in a fresh, non-associated context. So nested fns, impls and modules are checked on their
  own, while the body scanner never enters them.
* If the rule skips test code, every item with `#[cfg(test)]` (or a `cfg(all(..))` that requires
  `test`) is skipped without recursion, and so is a fn with an attribute whose last segment is
  `test` (`#[test]`, `#[tokio::test]`).

A fn is collected when its return type is a `Type::Path` whose last segment ends with `Result`
(`Result`, `io::Result`, `DbResult`), which covers `async fn`. A return type of `impl Future<Output
= R>`, with any further bounds, also counts when `R` is such a path. The scanned block is then the
`async` block that the fn's block ends with, and without such a tail the fn has no scanned block.

Each `FnItem` records:

* `block`: the scanned block, or none for a trait fn without a default body.
* `own_error_type`: the last segment of the `Result`'s second generic argument (`CacheError` in
  `Result<Vec<Publication>, CacheError>`), or `anyhow::Error` for `anyhow::Result`.
* `param_names`: the parameters that are plain identifiers.
* `callable_params`: those whose type is a fn pointer, an `impl` or `dyn` type bounded by a trait of
  `CALLABLE_TRAITS`, a type parameter with such a bound (inline or in the `where` clause), or a
  reference, `Box` or `Pin` of one of these.
* `scope`: the `SymbolScope` built from the context's self names and the generic type parameters of
  the context and the fn.
* `is_associated` and `trait_impl_self_type`, used by the [recursion rule](#callees).
* `anchor` and `header`: the line spans of the diagnostics.

### Out-of-line test modules

The `#[cfg(test)]` skip of the item walk only sees inline modules. A file declared in its parent as
`#[cfg(test)] mod tests;` is skipped too, by `syntax/test_code.rs`. A file is a test module when its
declaring file has a top-level `mod name;` item with `#[cfg(test)]`, or when the declaring file is
itself a test module.

The declaring file is the first existing candidate that declares the module:

* For `D/S.rs`: `D/mod.rs`, `D/lib.rs`, `D/main.rs` and `D.rs`, declaring `S`.
* For `D/mod.rs`: the same candidates one directory up, declaring `D`'s last component.
* `lib.rs`, `main.rs` and the files directly under `src/bin/` are crate roots and have no declaring
  file.

Each candidate is read and parsed at most once per run, and each verdict is cached. A missing
candidate is skipped silently. A candidate that exists but can't be read or parsed is skipped too,
and the verdict carries it as a problem. `#[path = "..."]` attributes are not followed.

### Doc lines

`doc_lines::collect` turns every `#[doc = "..."]` attribute with a string literal value into
`DocLine { text, line, column }`, with one leading space removed. A multi-line value is split on
`\n`:

* When the literal spans as many source lines as the value has lines (a `/** */` block, or a literal
  with real line breaks), each doc line gets its own source line.
* Otherwise, such as an escaped `\n` in `#[doc = "a\nb"]`, every doc line gets the literal's line.

Non-literal values (`include_str!`) are skipped.

### Parsing the `# Errors` section

`errors_section::parse` returns an `ErrorsSection`, or `None` when the docstring has no `# Errors`
section.

Section selection and line grouping are rule-agnostic, in `doc/section.rs`:
`section::find(doc_lines, title)` returns a `DocSection` (header position, body line range,
duplicate headers), and `DocSection::content` groups its body into a `SectionContent`. The rules
below hold for any title; `errors-doc` uses `Errors`.

Section selection:

* Headers are ATX headers outside fences. An Errors header's title is exactly `Errors`.
* The section starts at the first Errors header of the smallest level.
* When a header of a smaller level precedes it, the Errors header belongs to that other section, and
  the fn has no `# Errors` section. A `## Errors` under `# Returns` documents the items of a
  returned stream, not the fn.
* A second Errors header of the same level gets a duplicate-section format finding.
* The section ends at the next header of the same or a smaller level. Deeper headers stay in it, and
  fenced blocks inside it are skipped.

Link reference definitions are collected from the whole doc, not only the section, because they
usually sit at the very end. A definition's target may be on the following indented lines.
Definition lines are never bullets or prose.

Inside the section, lines are grouped into prose paragraphs, top-level bullets (at the section's
smallest bullet indent) and sub-bullets (deeper bullets, which belong to the preceding top-level
bullet). A bullet's continuation lines are joined with one space, so a bullet split across lines,
such as `Forwards` followed by a long link on the next line, is parsed as one string.

`doc/links.rs` finds the references in a joined string, in order:

* `` [`code`] ``, `` [`code`](target) `` and `` [`code`][ref] ``: link references whose path is the
  code text.
* `[text](target)` and `[text][ref]` with a non-code label: link references whose path is the
  target, or the definition's target.
* `` `code` `` outside a link: a `ReferenceKind::BareCode` reference. Bare code spans count only in
  forward bullets, where they name closure parameters such as `` `tx` ``.

Each top-level bullet is classified, in this order:

1. `Forward`: starts with the word `Forwards` or `Forward`, and some reference is followed by `'s`
   or `'`. Its paths are every reference before the first possessive, so `` [`a`] and [`b`]'s ``
   gives both.
2. `ForwardList`: starts with `Forwards` or `Forward`, has no possessive reference, ends with `:`,
   and has sub-bullets. Its paths are the first reference of each sub-bullet.
3. `ForwardUnnamed`: any other bullet that starts with `Forwards` or `Forward`. Its paths are every
   reference in it, leniently, and it gets an unnamed-forward format finding that keeps its
   one-segment bare code spans, so the message can suggest a parameter name.
4. `Variant`: starts with a link reference, optionally after `Returns a ` or `Returns an `. Its head
   paths are the link references in its head: the text up to the first ` if `, ` when `, ` for `, `
   unless `, or a `:` that ends the bullet. Its linked paths are the link references after the head
   and in its sub-bullets.
5. `Other`: anything else, including a bullet that starts with a bare code span. Its references join
   the prose links.

Further rules:

* A `Forward` bullet whose wording deviates from the canonical form gets a non-canonical-forward
  format finding, but keeps its paths. The wording check is `is_canonical_forward_wording`, with the
  canonical and mapped forms in `CANONICAL_FORWARD_WORDING` and `MAPPED_FORWARD_WORDING`.
* A forward path whose last segment is UpperCamelCase names a type, not a function. It gets a
  forward-names-type format finding and no stale-forward check, which would only repeat it.
* Sub-bullets of `Forward`, `Variant` and `Other` bullets are conditions, and their references
  document no error. Text after `except` in a `Forward` bullet is ignored too.
* Linked paths name the function a variant comes from, such as the `ensure_met` in ``*
  [`CacheError::StaleState`] if: ... (see [`StateRequirement::ensure_met`])``. The comparison lets
  them document a converted forward.
* The section's first line outside fences and link definitions must start a prose paragraph that is
  exactly `Returns an error if:`, and the section must have a top-level bullet. Otherwise the
  section gets one non-canonical-intro format finding at that line, or at the header when the
  section is empty.

The resulting `ErrorsSection` holds the header position, `variant_bullets` (head paths, linked
paths, position), `forward_bullets` (kind, paths, position), `prose_links`, `has_bullets` and the
format findings.

### Symbol paths and matching

Every name on either side becomes a `SymbolPath`, a list of identifier segments
(`src/syntax/symbol_path.rs`). Normalization applies to doc text and `syn` paths alike:

* Generic arguments are stripped (`Foo::<T>::bar` is `Foo::bar`), and so is a trailing `()` or `!`.
* A leading `::`, `crate`, `self` or `super` is stripped, repeatedly.
* `<T as Trait>::f` becomes `Trait::f`.
* `SymbolScope::normalize` drops a qualifier that is a generic type parameter of the fn or its impl
  or trait, so `T::new` becomes `new`.

The qualifier is the second-to-last segment. `SymbolScope` decides when two paths name the same
symbol:

* Two qualifiers are equal when they are the same identifier, or when both are in the `Self` set:
  `Self`, the impl's self type, the implemented trait in `impl Trait for X`, and the trait's name
  inside a `trait` block.
* Two paths match exactly when their last segments are equal and, if both are qualified, their
  qualifiers are equal. So a method call, which has no qualifier, matches a documented
  `Type::method` by its last segment.
* Two paths match loosely when their last segments are equal, both are qualified, and the file's
  `QualifierLinks` link the qualifiers.
* For mentions only, a documented path also matches any prefix of a body path, so
  `syn::Error::new_spanned` mentions `syn::Error`.

`QualifierLinks` links two different qualifiers when one of them is a trait name, or one is an alias
of the other:

* A trait name is a trait the file declares or implements, or a name that a `use` item imports and
  that the file never names again. A trait imported only to call its methods looks like that.
* An alias is a `use X as Y` rename or a `type Y = X` alias.

`SymbolScope::pair` pairs body facts with documented paths in two passes:

1. Pair every fact and documented path that match exactly.
2. Pair the facts left without an exact match with the documented paths left without one, by loose
   match.

So `ServerConfig::from_yaml_file(..)?` is paired with `` [`YamlConfig::from_yaml_file`] `` when the
file imports `YamlConfig` and never names it again. But `Storage::load()?` documented as ``
[`Cache::load`] `` stays unpaired, which is a missing forward plus a stale one. A fn that calls both
`A::new` and `B::new` and documents both still has each paired with its own bullet.

Each fact and documented path carries a key, and `pair` returns the sets of paired keys on each
side. A fact's key names its obligation and error. A documented path's key names its variant bullet,
its forward bullet and path, or the prose. So the callers never align parallel vectors by index.

### File index

`FileIndex::build` indexes a parsed file once, for every fn in it:

* `helper_fns(name)`: every fn item in the file (free, inherent, trait impl and trait default) with
  that name, with its signature, its body and its owner: the last segment of its impl's self type,
  its trait's name, or none for a free fn.
* `identifier_counts`: how often each identifier appears in the file, including inside macro bodies.
  The imported-only trait rule of `QualifierLinks` uses it.
* `has_inherent_fn(type, name)`: whether an inherent impl of the file whose self type's last
  segment is `type` has a fn named `name`. The [recursion rule](#callees) uses it.
* `is_called(name)`: whether the file calls `name`, as the last segment of a call path, a method
  name, or an identifier followed by a parenthesized group inside a macro body.
* The trait names, `use` imports and aliases of `qualifier_links()`.
* The `From` conversions between error types, and the enums the file declares.

A conversion comes from either of two declarations:

* `impl From<S> for T` whose `from` fn's tail and `return` expressions all build variants of `T`
  (`Self::Invalid(error)`, `T::Incomplete`, through blocks, `if` and `match`) converts `S` into
  those variants. If some branch isn't a variant of `T`, such as a helper call, the conversion's
  variants are unknown. An impl whose `S` is a type parameter is ignored.
* A variant `V` of an enum `T` with a `#[from]` field of type `S` converts `S` into `T::V`.

Two lookups use them:

* `convert(source_type, target)`: the variants that converting an error of `source_type` into
  `target` builds. Sources match by last segment, and by qualifier when both are qualified, so
  `std::io::Error` matches a `#[from] io::Error`. It is `None` when no conversion matches or the
  match has unknown variants.
* `conversion_variants_into(target)`: every variant that some conversion into `target` builds. It is
  `None` when the file doesn't declare `target` as an enum, or some conversion into it has unknown
  variants.

### Body scan

`BodyScanner` implements `syn::visit::Visit` and walks the scanned block once. `body::scan` returns
its `BodyFacts`. Paths are normalized with the fn's scope, and helpers are resolved through the
`FileIndex`.

#### Scopes and facts

The scanner keeps a `nested_scope_depth` counter. Closures, async blocks, `try` blocks and the
bodies of generator macros increment it, since a `?` inside them belongs to their value, not to the
fn. Nested items aren't entered at all (`visit_item` doesn't recurse).

A closure that a call runs in place (see [Values that leave the fn](#values-that-leave-the-fn)) is
the exception. When `classify_fallible` classifies the call in a return position,
`classify_in_place_closure` visits the closure's body a second time, at the call's depth, so its
`?`, `return` and anyhow-macro sites are the fn's. While it visits and classifies the body,
`in_place_return_site` holds the call's site, which `return_site` gives in place of `Tail`. So
`visit_expr_return` classifies the closure's `return` values there, as the closure's value is, and
so does match-forwarding for the `Err(binding)` arms it handles, including an arm that passes the
error through a helper (`Err(e) => Err(classify(e))`). A `?` after the call converts them, as it
converts the errors that `wrap_binding` records. `with_nested_scope` clears it, so a `return` in a
nested closure stays that closure's. The closure's identifier parameters hide the outer bindings of
their names meanwhile, so `r?` on a parameter `r` isn't taken as a forward of an earlier
`let r = load()`, and the bindings come back afterwards. The walk's own visit of the closure still
counts it as a nested scope, so the closure's callees are nested callees too, and a closure in any
other position stays nested. Visiting a body twice records nothing twice that matters: the
comparison dedups obligations by `ObligationKey`, the facts other than `obligations` are sets,
`forwarded_sites` is checked before an anyhow macro is recorded, and the bindings are recomputed.
They come out the same unless the closure shadows a binding it used earlier: a tail closure is
visited a second time after the walk has seen its `let`s, so the earlier use resolves to the
shadowing binding (see [Known limits](#known-limits)). An in-place closure at the tail of another
one in a return position is visited twice per visit of the outer one, so the visits grow
exponentially with the nesting depth of in-place closures in return positions. Real code nests one
or two.

`BodyFacts` holds:

* `obligations`: what the section must document, each with its position. They are recorded only at
  depth 0.
* `has_opaque_source` and `has_opaque_error`: whether some [opaque site](#opaque-sites) exists at
  depth 0.
* `nested_callees`: the callee of every obligation that would be recorded inside a nested scope,
  such as the `register` of `self.spawn_body(async move { this.register().await? })`, and the callee
  of every [possibly recursive](#callees) call. They only keep `Forwards` bullets from going stale.
* `mentions`: every expression path, call path, method path (qualified by its builder type, see
  [Query builders](#query-builders)) and struct-literal path, at any depth, plus `into_iter` and
  `next` for every `for` loop and every identifier of an unparsed macro. Patterns are never mentions
  (`visit_pat` doesn't recurse), so an `Err(Error::X) =>` arm doesn't count as returning `Error::X`.
* `conversion_targets`: the types `X` of every `X::from` path, such as `ProtocolError` in
  `.map_err(ProtocolError::from)`.

`BodyFacts::is_infallible` is true when the body has no obligation and no opaque site.

The hooks that drive classification:

* `visit_expr_try` classifies its operand as a `Try` site.
* `visit_expr_return` classifies its value as a `Tail` site, and so is the fn block's value: its
  tail expression, or a brace-delimited macro without `;` at its end (a tail `select! { .. }`).
* `visit_expr_closure` and `visit_expr_async` also classify the closure's value as a tail site,
  which only records nested callees.
* `visit_expr_match` and `visit_expr_if` apply [match-forwarding](#match-forwarding), looking
  through `&&` let-chains.
* `visit_local` records [`let` bindings](#bindings), and `visit_expr_assign` keeps a query builder
  binding across `query = query.bind(..)`.
* `visit_macro` dispatches to [macro handling](#macros).

These hooks run at every depth, so nested scopes record their nested callees, and their obligations
and opaque sites are dropped there.

#### Obligations and spellings

`ObligationKind` has six kinds:

* `Direct { error }`: the body constructs `error` and returns it.
* `Forward { callee }`: the body propagates `callee`'s error unchanged, up to `From`.
* `Either { callee, errors }`: the body maps `callee`'s error into `errors`. The y-scope code
  documents both ways (`.map_err(InternalError::TaskOutputs)?` as a forward, and `.map_err(|error|
  ClientError::Transport(..))?` as the variant), so either is accepted.
* `EitherUnknown { callee, mapper }`: the body maps `callee`'s error with a mapper the scan can't
  resolve. `mapper` describes it for the message.
* `HigherOrder { callee, closure_errors }`: the body passes closures to `callee`, whose result
  leaves the fn, and the closures build and return `closure_errors` (see [Higher-order
  calls](#higher-order-calls)).
* `HelperError { helper, error_type }`: the body returns an error that `helper` builds, and the scan
  can't resolve which. A helper returns an error, not a `Result`, so it can't fail and is never a
  callee: `callee()` is [`None`] for it, as for `Direct`. `error_type` is the fn's own error type
  when the call is converted, or else the return type of the helper fns of the file, if they agree.

`error` and `callee` are `Spellings`: the paths that may document them, the primary one (used in
messages) first, plus the error type of the value when the expression names it.

* A wrapped variant, `Err(CacheError::Internal(InternalError::X))`, is spelled `InternalError::X`
  first and `CacheError::Internal` second, with error type `CacheError`.
* A constructor call `T::ctor(..)`, such as `Error::not_found(id)`, is spelled `T`, then `T::Ctor`,
  then `T::ctor`, with error type `T`. So `` [`serde::de::Error::custom`] `` documents
  `D::Error::custom(..)`.
* A method at the end of a chain, `collection.find(filter).with_options(options).await?`, is spelled
  as the last method first, then the chain's earlier method calls. The call at the chain's root is
  not a spelling, so `SpiderClient::builder(e).connect()?` forwards `connect`, which ``
  [`SpiderClient::builder`] `` doesn't document.
* A call that passes callables is spelled with their callees first.
* anyhow errors have the error type `anyhow::Error`.

Positions:

* `?` obligations and opaque sites are at the `?` token. A multi-line chain then points at the line
  with `.await?`, which is where a trailing suppression goes. An own-line suppression above the
  statement covers it too.
* `Err(..)` is at the `Err` identifier, and `.ok_or(..)` and `.ok_or_else(..)` at the method name.
* Tail and `return` forwards are at the callee's last path segment or method name.
* Macros are at the macro path's first segment.

#### Bindings

`visit_local` records `let ident = <init>` bindings, a later one of a name replacing an earlier one:

* A closure (`let to_status = |e| ..;`) keeps two summaries: the errors its body classifies to, for
  use as a mapper, and the callee of its value, for a call of the closure. The callee comes from
  `callee_of`, which peels the error adaptors (see [Callees](#callees)), so
  `let create = |d| create_dir_all(d).context(..);` keeps `create_dir_all`.
* A spawn (`let handle = tokio::spawn(..);`) makes a `?` on the awaited binding an `OpaqueError`
  site, as an awaited spawn is.
* A query builder keeps the builder's type.
* An error constructor (`let e = Error::B;`) keeps its errors, so `return Err(e)` returns
  `Error::B`.
* Any other call or method call (`let result = run().await;`), unless the `let` has an `else`, keeps
  its callee, so `result` or `result?` in a return position forwards `run`. The callee comes from
  `callee_of` too, so `let result = run().context(..);` keeps `run`.

A binding stores only these summaries, computed when the `let` is visited, never syntax.

#### Return positions

An `Err(..)` is a `Direct` obligation only in a return position, where its value leaves the fn:

* The fn's value, a `return` value, and the operand of `?`.
* Inside any of them: a block's tail, an `if` or `else` branch's tail, a `match` arm, a `select!`
  arm body, and an argument of a value method that may return it (`.map_or(Err(..), ..)`,
  `.unwrap_or_else(|| Err(..))`).
* The value and the `return` values of a closure that a call in a return position runs in place,
  such as the closure of `std::thread::scope(..)` (see [Values that leave the
  fn](#values-that-leave-the-fn)).

So `tx.send(Err(Error::Cancelled))`, `results.push(Err(..))`, `value == Err(..)` and `assert_eq!(r,
Err(..))` record nothing. The same walk classifies the calls in return positions, so one rule,
`classify_fallible(expr, site)`, covers every value that leaves the fn.

#### Error expressions

`classify_error(expr)` gives the errors an expression builds. It first peels parentheses, `.await`,
`.into()`, `Into::into(x)`, `From::from(x)`, `X::from(x)` and `Box::new(x)`. Its result is a set of
errors, plus two flags: an unknown branch (a helper or mapper it can't resolve) and a value branch
(something it can't name as an error, such as a variable).

* A path whose last segment is UpperCamelCase (`Error::Stopped`): that path.
* A call whose callee's last segment is UpperCamelCase (`Error::Io(e)`): the callee, with a wrapped
  variant spelled as above.
* A struct literal (`Error::Invalid { .. }`): its path.
* A call `T::ctor(..)` with an UpperCamelCase `T` that isn't a name of `Self`, and a lower-case
  `ctor` (`Status::internal(..)`): the type `T`.
* `anyhow!(..)`, `anyhow::anyhow!(..)` or `format_err!(..)`: `anyhow::Error`.
* `if`, `match` and blocks: the union of the branches' tails.
* A call of a lower-case fn path (`unsupported_type_error()`, `Self::to_status(..)`), or a method
  call on `self` (resolved as `Self::to_status`): a [helper](#helper-and-pass-through-resolution).
  Unresolved, it is unknown, and so is any other method call or a call of something other than a
  path.
* A variable bound to an error constructor: that binding's errors.
* Anything else (a variable, a field, a literal, `format!`): a value.

An `Err(arg)` in a return position then records:

* `Direct` for each error of `classify_error(arg)`, [converted](#conversions) when `arg` is
  converted or the site is a `?`.
* The errors of a resolved helper (`Err(self.classify_failure(e).await)`) are `Direct` too. The
  helper can't fail, so it is never suggested or accepted as a Forwards target.
* An `OpaqueError` site when `arg` converts a value (`Err(error.into())`), since the conversion
  makes some variant the scan can't name.
* An `OpaqueSource` site for a value branch (`Err(error)`).
* Both opaque sites for an unknown helper, which may also pass on an error it is given. When none
  of the helper's errors resolve, a `HelperError { helper, error_type }` too.
* For each passed parameter of a resolved helper, a `Forward` of the callee whose error the argument
  binds, as described next.

#### Helper and pass-through resolution

Helper resolution is one level deep and same-file only. The helper is a `let` closure binding of
that name in the current body, or else the `helper_fns` with that last segment, narrowed by owner:

* An unqualified path calls a free fn, since an associated fn needs `Self::`.
* A path qualified by a name of `Self` calls a fn of the scanned fn's impl or trait.
* A path qualified by another name calls a fn whose owner is that name.
* When no fn fits, every fn with that name is a candidate.

The result is the union of `classify_error` over the helpers' tail and `return` expressions, where a
nested helper call is unknown instead of being resolved again. So `.map_err(to_transport_error)?`,
where `to_transport_error`'s body is `SchedulerError::Transport(..)`, maps into
`SchedulerError::Transport`.

Before that, `pass_through.rs` checks each outcome leaf of the helper (through blocks, `if` and
`match`, after peeling conversions) for passing on a parameter unchanged. Such a leaf is a passed
parameter of the result instead of a value:

* The parameter itself (`return error;`, `error.into()`), unless a destructuring that rebinds its
  name is in scope there.
* The variant that a top-level `let V(x) = param` (with or without `else`) matched, rebuilt around
  its binding: `DbError::Sql(error)` after `let DbError::Sql(error) = error else { .. };`. Another
  variant around the binding (`DbError::PayloadRejected(error)`) is an error as usual.

When the argument for a passed parameter is the binding of an enclosing `Err(binding)` match arm or
`if let` on a call, the call's callee is forwarded. So `Err(e) => return Err(self.classify(chunk,
e).await)` in `match insert(..).await` forwards `insert`, and `classify`'s other errors are
`Direct`. Any other argument is an `OpaqueSource` site.

#### Values that leave the fn

`classify_fallible(expr, site)` classifies a value at a `Try` or `Tail` site. It peels `.await` and
parentheses, then takes the first case that applies:

1. An awaited `spawn(..)` or `spawn_blocking(..)`, directly or as the value of a closure that runs
   in place (`(|| tokio::spawn(..))().await`): an `OpaqueError` site, since `?` turns its join
   error into some variant of the fn's error type.
2. A conversion (`x.into()`, `Into::into(x)`, `X::from(x)`): classify `x`. A converted value that
   isn't a call is an `OpaqueSource` site, so a tail `r.into()` never suggests forwarding `into`.
3. `Err(e)`: as in [Error expressions](#error-expressions). `Ok(..)` and `Some(..)` give nothing.
4. `.ok_or(e)` and `.ok_or_else(|..| e)`: `Direct` for the errors of the argument or the closure
   body. The receiver is an `Option` and is not forwarded.
5. `.map_err(f)` on a receiver whose callee is `c`, as `callee_of` finds it after peeling the
   receiver's adaptors:
   * A conversion mapper (`Into::into`, `From::from`, `X::from`, or a closure whose body is `e`,
     `e.into()` or `X::from(e)`): `Forward(c)`.
   * A mapper that constructs errors (an UpperCamelCase path such as `Error::Wrap`, a closure whose
     body classifies to errors, or a helper or closure binding that resolves to errors): `Either {
     c, errors }`.
   * An unknown mapper: `EitherUnknown { c }` and an `OpaqueError` site.
   * When the receiver has no callee (a variable), the errors become `Direct`, a conversion is an
     `OpaqueSource` site, and an unknown mapper an `OpaqueError` site.
6. `.context(..)` and `.with_context(..)`: `Either { c, [anyhow::Error] }`, with the receiver's
   callee `c` found as in case 5. anyhow's `Context` is also implemented for `Option`, so
   documenting `anyhow::Error` alone is accepted. Without a callee, it is an `OpaqueSource` site.
7. `.and_then(f)`: classify the receiver and `f`, since both errors leave the fn.
8. `.or_else(f)`: classify `f` only, since it replaces the receiver's error.
9. Pass-through adaptors (`PASS_THROUGH_METHODS`): classify the receiver. A closure argument only
   adds mentions.
10. Sinks, whose errors come from the items of an iterator or stream:
    * `.try_for_each(f)`, `.try_fold(init, f)` and `.try_for_each_concurrent(n, f)`: classify the
      value of `f`.
    * `SINK_METHODS` and `SINK_FNS`: walk the receiver chain back past `ITEM_KEEPING_METHODS` to one
      of `ITEM_PRODUCING_METHODS`, and classify the value of its callable. In this position any
      lower-case path is a fn, so `.map(parse).collect()?` forwards `parse`.
    * When the walk finds no callable: an `OpaqueSource` site.
11. Value methods, which return a value instead of a callee's result:
    * `RECEIVER_VALUE_METHODS` (`expect`, `unwrap`, `clone`, ...) may return the receiver's value:
      an `OpaqueSource` site. The `Result` that `handle.await.expect(..)` returns comes from the
      joined task.
    * `ARGUMENT_VALUE_METHODS` (`map_or`, `unwrap_or_else`, ...) may return an argument: each
      argument, or a closure's body, is classified at the same site.
12. An inner `?`, the outer `?` of `y??`: when the call at the root of `y`'s chain ends in `timeout`
    or `timeout_at`, `Forward` of its future's callee. Otherwise (a `JoinHandle`, a channel reply),
    an `OpaqueSource` site.
13. Blocks, `if` and `match`: classify each branch's value at the same site.
14. A macro: a `select!`'s arm bodies and a `try_join!`'s futures are classified at the same site.
    An anyhow macro gives nothing here, since `visit_macro` records it. Any other macro at a `?`
    site is an `OpaqueSource` site.
15. A call that runs a closure in place: the closure called on the spot, `(|..| ..)(..)`, or the
    only argument of an `IN_PLACE_FNS` fn, such as `std::thread::scope(|scope| ..)`, unless the
    closure is `async` or its body is an `async` block. The closure's body is visited at the current
    depth, and its value and `return` values are classified at the same site.
16. Any other call or method call: `Forward(callee_of(expr))`, or `HigherOrder` when it has closure
    errors. A call whose callee can't be named, such as `(self.callback)(..)`, is an `OpaqueSource`
    site.
17. A variable with a call binding: `Forward` of the bound callee. A variable with a spawn binding:
    an `OpaqueError` site.
18. Any other variable or field: an `OpaqueSource` site.
19. Anything else: an `OpaqueSource` site at a `?` site, and nothing at a tail site, where `Ok(..)`
    and literals are the success value.

#### Callees

`callee_of` gives a call's callee path (`foo`, `Self::foo`, `module::func`), or a method call's
method path with the chain's earlier method calls as alternative spellings, and then adds the
spellings of callable arguments:

* The error adaptors are peeled first: `.map_err(..)`, `.context(..)`, `.with_context(..)` and the
  `PASS_THROUGH_METHODS`, after `.await` and parentheses. So a closure binding `|d|
  create_dir_all(d).context(..)` gives `create_dir_all`, not `context`.
* `block_on` is transparent. For a call whose path ends in `block_on`, or a method call
  `.block_on(x)`, it gives `callee_of(x)`. So
  `runtime.block_on(provider.provide_credentials()).context(..)?` forwards `provide_credentials`.
* A call that runs a closure in place (case 15 of [Values that leave the
  fn](#values-that-leave-the-fn)) is transparent too: it gives the callee of the closure's value. So
  `let r = std::thread::scope(|_| load()); r?` forwards `load`. The in-place fn itself is never a
  callee, so a `Forwards` bullet for `std::thread::scope` is stale, and the closure's callees are
  asked for instead.
* A call of a `let` closure binding gives the callee of the closure body's tail. So `fetch(1)?`
  after `let fetch = |key| load(key);` forwards `load`. A closure can't be named in a doc link, so
  the binding itself is never a callee.
* `Ok`, `Some`, `Err`, value methods and calls of anything other than a path have no callee.

A call of the scanned fn itself forwards nothing. A call whose last segment is the fn's name is
recursive when the path is unqualified and the fn isn't associated. Inside an impl or trait, an
unqualified `parse(x)` can't call the method `parse`, so it forwards the free fn. A call whose
qualifier is a name of `Self`, or a method call `self.name(..)`, which calls through `Self`, has one
of three outcomes, which `BodyScanner::self_call_recursion` decides as a `Recursion`:

* `Certain`: the call is recursive and records nothing. This is the outcome outside a trait impl,
  for a trait impl without a self type (`FnItem::trait_impl_self_type`), and for a qualifier that is
  the trait's name, such as `Submitter::submit(self)` or `<Self as Submitter>::submit(self)`.
* `No`: in a trait impl, the qualifier is `Self` or the self type, and
  `FileIndex::has_inherent_fn` finds the type's inherent fn of that name. Rust looks up inherent fns
  before trait methods, so the call forwards that fn like any other call, spelled as written.
* `Possible`: the same, but the file has no such inherent fn. Another file may declare it, so the
  call records its callee as a nested callee only. A bullet for it is kept, and a missing bullet
  isn't reported. A `self.name(..)` call's nested callee is `Self::name`, since an unqualified one
  would keep the bullet of any fn whose last segment is `name`.

The `Recursion` is part of the `Callee`, so `callee_of`'s adaptor peeling and in-place transparency
keep it: a possibly recursive call under `.inspect_err(..)`, in a call or closure binding, or as the
value of an in-place closure records its nested callee too. Cases 5 and 6 of [Values that leave the
fn](#values-that-leave-the-fn) build their `Either` and `EitherUnknown` obligations from the
callee's spellings without checking the recursion. The mapped errors leave the fn either way, so a
bullet for them is right even for recursion, and in the possible case the callee, spelled
`Self::name`, documents the call when it forwards the inherent fn.

#### Higher-order calls

A call or method call whose arguments include callables gets their callees as its first spellings,
so documenting either the helper or the callable documents the call. The callables are:

* Inline closures and async blocks, through their value's callee. So `call_with_retry(cfg, move ||
  async move { client.start_job(req).await })` is spelled `start_job` first and `call_with_retry`
  second.
* Callable paths: a qualified path whose last segment isn't UpperCamelCase (`Self::init`,
  `JobExecutionState::ensure_running`), a free fn of the file that no parameter shadows, a `let`
  closure binding, or a callable parameter of the fn.

The closure errors of the call are the errors that its closures and async blocks build and let leave
them: in their value, their `return` expressions and the operands of their `?` expressions
(`Err(..)`, `.ok_or(..)`, `.ok_or_else(..)` and `.map_err(..)`, but not calls). A forwarded call
with closure errors is a `HigherOrder` obligation instead of a `Forward`. For example, a
`decode_frame(buffer, |reader| ..)` call whose closure returns
`Err(ProtocolError::MalformedFrame(..).into())` is documented by a `ProtocolError::MalformedFrame`
bullet or by forwarding `decode_frame`. The calls inside the closure stay the closure's, as `?` in
closures does unless the closure runs in place.

#### Conversions

`?` converts an error into the fn's own error type with `From`, and so do `.into()` and `X::from` in
an `Err(..)` argument. When the error's type has a conversion into the own error type in the file
(`FileIndex::convert`), the error is respelled with the conversion's variants first and the old
spellings after them. In a `Result<_, DecodeError>` fn with `impl From<ProtocolError> for
DecodeError { fn from(e: ProtocolError) -> Self { Self::Invalid(e) } }`:

* `Err(ProtocolError::MalformedFrame(..).into())` is `Direct(DecodeError::Invalid)`, also spelled
  `ProtocolError::MalformedFrame`.
* `usize::try_from(n).map_err(|_| ProtocolError::MalformedFrame(..))?` maps `try_from`'s error into
  `DecodeError::Invalid`.

The conversion applies at a `?` site to `Direct`, `Either` and closure errors, and to an `Err(..)`
whose argument is converted. Errors whose type has no such conversion in the file keep their
spellings.

#### Query builders

A method on a `sqlx` query builder is qualified by the builder's type, so that the forward names the
exact function. `BUILDER_FNS` maps the builder fns, called as `sqlx::<fn>` or unqualified, to their
types: `query` and `query_with` build a `Query`, `query_as` and `query_as_with` a `QueryAs`, and
`query_scalar` and `query_scalar_with` a `QueryScalar`. The type passes through
`BUILDER_KEEPING_METHODS` (`bind`, `persistent`) and through `let` and assignment bindings. So
`sqlx::query_scalar(SQL).bind(id).fetch_one(&pool).await?` forwards `QueryScalar::fetch_one`.

#### Match-forwarding

The y-scope code often forwards a callee's error through a binding:

```rust
match self.start_job(spider_job_id).await {
    Ok(_) | Err(ClientError::InvalidJobState(_)) => {}
    Err(error) => return Err(error.into()),
}
```

The rule applies to `match scrutinee { .. }` and to `if let Err(e) = scrutinee`, including an `if
let` in a `&&` let-chain. It classifies the scrutinee as a tail value when all of these hold:

* The peeled scrutinee is a call.
* An arm's pattern, or the let pattern, is `Err(ident)`.
* That arm's body, or the `if` block, passes the binding on unchanged: `Err(ident)`,
  `Err(ident.into())`, `Err(X::from(ident))`, `bail!(ident)`, `Err(anyhow!(ident))`, or `return
  Err(..)` of any of these.

A binding passed on this way records no `Direct(anyhow::Error)` for its `bail!` or `anyhow!`, and no
opaque site for its `Err(..)`. Two related shapes are handled too:

* An arm that wraps the binding in a variant, `Err(Error::Cache(error))` or `return Err(Error::Db {
  source: error })`, maps the callee's error into `Either { callee, errors }`, and the `Err(..)`
  records nothing itself.
* A catch-all arm that applies `?` to its binding, `result => result?`, forwards the scrutinee.

#### Opaque sites

An opaque site is a depth-0 place where an error leaves the fn but the scan can't attribute it. They
never produce a finding. They only relax the [reverse checks](#comparison).

* `OpaqueSource`: the error's source is unknown. Examples are `?` on a variable or field, a tail
  variable without a call binding, an awaited handle, the outer `?` of `x??` (except `timeout`), a
  sink without a callable, a receiver-value method, an unnameable callee, an unknown macro at a `?`
  site, an `Err(..)` of a value, and an unparsable macro with a postfix `?` among its tokens.
* `OpaqueError`: the source is known but the mapping isn't. Examples are an unresolved mapper or
  constructor, a converted value in `Err(..)`, and an awaited spawn or spawn binding.

A postfix `?` in macro tokens is a `?` after an identifier or a parenthesized or bracketed group, so
tracing's `?id` sigil in `tracing::info!(id = ?id)` doesn't count.

#### Macros

`macros::kind` dispatches on the macro path's last segment:

* anyhow: `bail!`, `ensure!`, `anyhow!` and `format_err!`, with or without the `anyhow::` prefix.
  `bail!` and `ensure!` record `Direct(anyhow::Error)` at depth 0, unless match-forwarding applies.
  `anyhow!` and `format_err!` only build a value, so they record nothing outside an `Err(..)` in a
  return position. All of them mention `anyhow::Error`.
* `select!` (`tokio::select!`, `futures::select!`, bare `select!`): parsed with a custom `syn`
  parser for `[biased;] (pat = expr [, if cond] => body [,])* [else => body [,]]`. The futures,
  guards and arm bodies are visited at the current depth, so a `?` in an arm is an obligation of the
  fn. The patterns are ignored. In a return position, the arm bodies are the macro's value.
* `try_join!`: parsed as an expression list. In a return position, each future is classified as a
  call whose error leaves the fn.
* Generators: the bodies of `stream!` and `try_stream!` are visited as a nested scope, like an async
  block.
* Pattern-taking macros: `matches!`, `assert_matches!` and `debug_assert_matches!` are parsed as an
  expression, a comma, and a `syn::Pat` with an optional `if` guard. Only the expression and the
  guard are visited, so `matches!(r, Err(X::Y(_)))` mentions nothing.
* Everything else, including the assert family: the tokens are parsed as a comma-separated
  expression list, and then as a block's statements. If one parse succeeds, the result is visited at
  the current depth. If both fail (`sqlx::query!` with SQL, or `tracing::info!(id = ?id, "..")`),
  the scanner records only mentions, one per identifier in the tokens, and the postfix-`?` opaque
  site.

#### Library API tables

`known_apis.rs` holds every table of library APIs that the scan recognizes by name:
`PASS_THROUGH_METHODS`, `SINK_METHODS`, `SINK_FNS`, `ITEM_PRODUCING_METHODS`,
`ITEM_KEEPING_METHODS`, `RECEIVER_VALUE_METHODS`, `ARGUMENT_VALUE_METHODS`, `BUILDER_FNS`,
`BUILDER_KEEPING_METHODS`, `SPAWN_FNS`, `IN_PLACE_FNS`, and the `CALLABLE_TRAITS` of the item walk.
Every table is matched by the last segment of a fn path or by a method name only, so a same-named fn
of any type is treated the same way. A new library shape is supported by extending a table, and by a
test in `body.rs` that shows the shape it covers.

### Comparison

`FnCheck::findings` compares the `ErrorsSection` with the `BodyFacts`, using the fn's scope, the
`FileIndex` and the `QualifierLinks`. Obligations are first deduplicated by `ObligationKey`, which
leaves out an `EitherUnknown`'s mapper description, so each key is reported once, at its first site.
All matching uses the two-pass `SymbolScope::pair`, once for errors against variant heads and prose
links, and once for callees against forward paths and variant linked paths.

Forward checks find missing documentation:

* `Direct(e)` is satisfied when a variant head path or a prose link matches `e`.
* `Forward(c)` is satisfied when a path of a `Forward`, `ForwardList` or `ForwardUnnamed` bullet
  matches `c`, or a variant bullet's linked path does. The second form documents a converted forward
  as the variant it becomes, with the function in its condition.
* `Either` and `HigherOrder` are satisfied by `Forward(c)`'s condition, or when every one of their
  errors satisfies `Direct`'s condition.
* `EitherUnknown { c }` is satisfied by `Forward(c)`'s condition, or by any variant bullet or prose
  link at all.
* `HelperError { error_type }` is satisfied by a variant head path or a prose link that names
  `error_type` or one of its variants, or by any of them when `error_type` is unknown. A Forwards
  bullet never satisfies it.

A converted error needs a head that names it. When the file declares the conversion, the converted
variant documents it, and so does the inner error. Otherwise, `Err(DbError::Corrupted.into())` in a
`Result<_, Error>` fn is documented by ``[`Error`] converted from [`DbError::Corrupted`] if ...``,
and not by an unrelated `` [`Error::Corrupted`] `` bullet: nothing in the file links
`Error::Corrupted` to the conversion.

Reverse checks find stale documentation. They apply to bullets only, never to prose links.

* A variant bullet is stale when none of its head paths is paired with an error or matched by a
  mention, and none of its linked paths is paired with a callee. `is_kept_variant` keeps it in these
  cases:
  * A head path equals the fn's own error type, which the signature guarantees.
  * Every head path names an error type rather than a variant (no UpperCamelCase qualifier, such as
    `` [`std::io::Error`] ``), and the body has an `OpaqueSource` site. A `?` on a handle or a
    variable can return such a foreign type through `From`.
  * A head path `X::V` has a conversion target `X`, and `conversion_variants_into(X)` is unknown or
    holds `X::V`. `.map_err(ProtocolError::from)` may build any `#[from]` variant of a
    `ProtocolError` declared in another file.
* A body with an `OpaqueError` site disables the stale-variant check, since the unresolved mapper
  may build any variant.
* A forward path, unless it names a type, is stale when no callee is paired with it and
  `is_stale_forward` holds. A path is kept in these cases:
  * A nested callee matches it, exactly or loosely, such as a `?` inside the async block the body
    hands to `Self::spawn_body`, or the callee of a possibly recursive call of a trait impl method.
  * The body mentions it and has an opaque site, since the call's error may leave through that site,
    as in `let r = Wrapper(foo()); r.0?`.
  * The body has an `OpaqueSource` site and the file calls the path's last segment anywhere
    (`FileIndex::is_called`). The opaque `Result` may come from that call:
    `indexing_task.await.context(..)??` forwards the `run_indexer` of a task that another fn
    spawned.
  * It names a callable parameter of the fn that the body mentions, such as `body` in a fn that
    hands `body` to a spawned task.
* `ForwardUnnamed` bullets get no reverse check.

Delegation merge: when a fn has at least one missing `Forward` and at least one stale variant
bullet, those findings are replaced by one delegation finding at the header. It lists the callees to
forward and the number of stale variants. This turns a public facade that lists the leaf variants of
each delegated call into one finding per fn instead of one per variant. For example,
`components/spider-client/src/client.rs` lists 41 leaf variants across 10 fns that tail-call
`self.job_orchestration.<fn>(..)`. Other findings of the fn are reported as usual.

The intro finding is dropped for a section without bullets in a fn whose body is infallible. Such a
section states that the fn never fails, such as one kept for `clippy::missing_errors_doc`.

Positions: missing findings are at the obligation's position. Stale and format findings are at the
bullet's first doc line and column. Delegation and duplicate-section findings are at the header. The
messages are built in `check.rs` (`missing_finding`, `stale_variant_finding`,
`stale_forward_finding`, `delegation_finding` and `format_message`), and name callees as the body
writes them (`Self::load`, `Type::func` or a bare method name).

## Parameter sections

`type-params-doc` and `trait-params-doc` check sections of the same shape: one top-level bullet per
parameter, ``* `name` - description``. `doc/param_section.rs` holds everything they share, so the
two rules differ only in which items they check and which names those items have.

`ParamSectionKind` names a kind of section by its title and the noun its messages use:
`TYPE_PARAMETERS` (`Type Parameters`, "generic parameter") and `PARAMETERS` (`Parameters`,
"parameter"). The title must match exactly, so `# Type parameters` and `# Arguments` are not
sections of either kind.

`ParamSectionKind::parse(doc_lines)` selects the section with
[`section::find`](#parsing-the--errors-section) and groups it with `DocSection::content`, so
selection, fences, link definitions and continuation lines behave exactly as for `# Errors`. Each
top-level bullet becomes a `ParamBullet`, whose `BulletForm` is one of:

* `Nameless`: the bullet starts with prose.
* `NonCanonical { name }`: the bullet starts with a reference whose code is `name`, either a code
  span (`` `T` ``) or a code link (``[`T`]``), but isn't canonical.
* `Canonical { name }`: the bullet starts with a bare code span, then ` - `, then a non-blank
  description. The description may continue on the next lines.

Sub-bullets belong to their bullet and are never parameters. Prose in the section is ignored, and so
are the lists it introduces. `DocSection::content` marks a top-level bullet with `follows_prose`
when a prose paragraph comes right before it, and `parse` splits the bullets into lists at those
bullets. A list that follows prose and has only `Nameless` bullets is dropped, so a prose list such
as "Short names are used to: * Reduce verbosity." isn't reported bullet by bullet. A `Nameless`
bullet in any other list is still a parameter bullet that lacks its name.

`ParamSection::compare(required, accepted)` compares the bullets with the item's parameters.
`required` are the names that must be documented, `accepted` are further names a bullet may document
without being required. It reports, in this order:

1. `DuplicateHeader` for each later header with the same title at the same level.
2. Per bullet, in source order: `NamelessBullet` without a name; `UnknownName` for a name in neither
   list; `DuplicateBullet` for a name an earlier bullet has; else `NonCanonicalBullet` if the bullet
   isn't canonical. A non-canonical bullet still documents its name.
3. `Missing` at the header for each required name that no bullet has.

`compare` is only meaningful for an item that has parameters. The rules handle the other two cases
themselves, with the shared messages:

* An item with parameters but no section: `missing_section_message`, at the docstring's first line.
* A section on an item without parameters: `no_parameters_message`, at the header, instead of an
  `UnknownName` per bullet.

Messages, with `T` the title, `N` the noun and `I` the item as `DocItem::description` gives it (for
example ``fn `load` ``):

* `` I has Ns but no `# T` section; add one with a bullet for each of `'a`, `Item` ``
* `` I has no Ns; remove the `# T` section ``
* `` the docstring has a second `# T` section at the same level; merge them ``
* `` this `# T` bullet doesn't start with the N it documents; write "* `name` - description" ``
* `` write this `# T` bullet as "* `name` - description" ``
* `` `# T` documents `x`, which isn't a N of I; remove the bullet ``
* `` `x` is already documented on line 12; remove this bullet ``
* `` `# T` doesn't document `x`; add "* `x` - ..." ``

The order of the bullets isn't checked against the order of the parameters.

`syntax/doc_items.rs` walks the items both rules look at. `doc_items::collect(syntax_tree,
skip_tests)` returns a `DocItem` (kind, name, attributes and `ItemLines`) for every fn, struct,
enum, union, trait and type alias, including the ones nested in modules and fn bodies. Its
`DocItemKind` tells free fns, inherent methods, trait impl methods and trait method declarations
apart, and each kind carries the item's signature if it is a fn, or its generics otherwise.
`DocItem::generics` gives the item's own generics either way. With `skip_tests`, it skips
`#[cfg(test)]` items with everything in them and fns with a `test` attribute, as the `errors-doc`
item walk does. Both rules check only documented items: `doc_lines::collect_documented` gives an
item's doc lines only when at least one of them is non-blank.

## symbol-order

The rule checks the "Symbol ordering" convention of the y-scope Rust `coding-style.md`: items are
grouped by visibility, ordered by kind within a group, and each type is directly followed by its
impl blocks.

### Checked scopes

The rule checks two kinds of scopes, each on its own:

* Modules: the file root and the body of every inline `mod name { .. }`, nested ones included.
  Items in fn bodies and other blocks are not in a module scope and aren't checked.
* Inherent impl blocks (`impl Type { .. }` without a trait) in a checked module, whatever module
  their type is defined in.

Trait impl blocks and trait declarations aren't checked inside. Their item order follows the trait
or rustfmt's `reorder_impl_items`.

If the rule skips test code, an out-of-line test module file is skipped, as
`FileContext::is_test_module` decides, and so is every item with `#[cfg(test)]` (or a `cfg(all(..))`
that requires `test`), including whole `mod tests` blocks and inherent impl blocks, and every fn
with a `#[test]`-like attribute, as `doc_items` skips them. Otherwise, those items are counted like
any other, and the body of a `#[cfg(test)] mod tests` is checked as a module.

In a module, these items are exempt: they are neither checked nor counted, so they may sit anywhere,
and the items around them are compared as if they weren't there.

* `use` and `extern crate` items.
* Modules, both `mod x;` declarations and inline modules. An inline module's body is still checked
  as its own scope.
* `macro_rules!` definitions and item-position macro invocations.
* `extern` blocks and items `syn` keeps as verbatim tokens.
* Impl blocks whose self type isn't a struct, enum or union defined in the same module (see below).

In an inherent impl, associated types, macro invocations and verbatim items are exempt.

### Sort keys

Each counted item gets a sort key. The scope is in order when the keys don't decrease.

In a module, the key is `(group, category)`, and the type category carries `(anchor, impl rank)`
(`Category::Type` in `keys.rs`), compared after the category itself:

* `group`: the item's `VisibilityGroup` (`syntax/visibility.rs`): `pub`, `pub(crate)`,
  `pub(super)`, `pub(in path)`, private. `pub(in crate)` counts as `pub(crate)`, `pub(in super)` as
  `pub(super)`, and `pub(self)` as private. An impl block has no visibility of its own and takes
  its type's group.
* `category`: const 0, static 1, type 2, fn 3. Structs, enums, unions, traits, trait aliases and
  type aliases are all types, in any order among themselves, and an impl block has its type's
  category.
* `anchor`: for a type, its index in the module's items; for an impl block, its type's index. Types
  never conflict on it, since their indices increase in source order, and it puts each impl block
  right after its own type.
* `impl rank`: 0 for the type itself, 1 for an inherent impl, 2 for an impl of a crate-local trait,
  3 for an impl of any other trait.

An impl block belongs to a type when its self type, without parentheses, is a path of exactly one
segment (generic arguments allowed) that names a struct, enum or union defined in the same module.
`impl crate::a::Foo`, `impl<T> Trait for T`, `impl Trait for &Foo`, `impl dyn Trait` and impls on
type aliases are exempt. When the module defines the name more than once, as `#[cfg]` variants do,
the impl block belongs to the nearest definition before it, or else to the first one.

A trait is crate-local when `CrateNames::is_crate_path` (`syntax/crate_names.rs`) holds for its
path: the path starts with `crate`, `self` or `super`, or its first segment is a trait or module the
file defines, or a name the file imports with a `use` rooted at `crate`, `self`, `super` or one of
its own modules. A path with a leading `::`, a std or prelude trait (`Display`, `From`, `Clone`),
and a trait of another crate of the same workspace (`spider_core::Task`) are external. A negative
impl (`impl !Send for Foo`) is ranked by its trait like any other.

In an inherent impl, the key is `(category, group)`: an associated const is `(0, pub)` whatever its
visibility, and a fn is `(1, group)`. So consts come before fns, and fns are ordered `pub` first,
private last. Consts aren't ordered among themselves.

### Minimal moves

Reporting every item whose key is smaller than the key of some earlier item would cascade: one
private fn at the top of a module would flag every `pub` item after it. Instead, the rule keeps a
longest non-decreasing subsequence of the keys in place, and flags only the items outside it. Moving
exactly the flagged items, each to the place its diagnostic names, sorts the scope, and no smaller
set of moves does.

Among the longest subsequences, the rule keeps the one whose indices are lexicographically smallest,
so for equally long choices the earlier items stay and the later ones move. With `pub a`, `fn b`,
`pub c`, the kept items are `pub a` and `fn b`, and `pub c` is flagged. With `fn a`, `pub b`, either
item could move, and `pub b` is flagged.

The sequence is found by the quadratic dynamic program: the length `L[i]` of the longest
non-decreasing subsequence that starts at each index, then a greedy walk that starts at the smallest
`i` with the maximum `L[i]` and steps to the smallest later `j` with `key[j] >= key[i]` and `L[j] =
L[i] - 1`. Scopes hold at most a few hundred items, so the quadratic cost is negligible.

A flagged item always conflicts with some kept item: with no larger kept item before it and no
smaller kept item after it, it would extend the kept subsequence. `moves::place` panics in that
impossible case, since it means `moves::kept` is wrong.

### symbol-order diagnostics

One diagnostic per flagged item, at the item's first token after its outer attributes (its
visibility, or its keyword):

```text
<item> should come <after|before> <reference>: <reason>
```

* `<item>` and `<reference>` describe an item by its group, its kind and its name, such as
  ``pub(crate) fn `load` `` or ``private struct `State` ``, and the same way inside an inherent
  impl, such as ``pub const `LIMIT` ``. An impl block is described without a group, as
  ``impl `State` `` or ``impl `Display for State` ``, and a negative impl as
  ``impl `!Send for State` ``. A trait's path segments are joined by `::`, and its generic
  arguments are dropped, as are the self type's.
* `<reference>` is the last kept item whose key is at most the flagged item's key, with `after`. If
  there is none, it is the first kept item, with `before`. So a misplaced impl block is told to come
  after its type or its type's last kept impl.
* `<reason>` compares the flagged item with the kept item it conflicts with: the first kept item
  before it with a larger key, or else the last kept item after it with a smaller key. The first key
  component that differs gives the reason:
  * group: `pub items come before private items`, with the two groups' names.
  * category: `consts come before fns`, with the plurals `consts`, `statics`, `types and their
    impls` and `fns`.
  * anchor, or impl rank against rank 0: `a type's impl blocks directly follow it`.
  * impl rank: `inherent impls come before impls of crate traits`, `impls of crate traits come
    before impls of external traits`, or `inherent impls come before impls of external traits`.
  * In an inherent impl: `associated consts come before fns`, or `pub fns come before private fns`.

Examples:

```text
src/a.rs:12:1: ORG001 symbol-order: pub fn `run` should come after pub struct `Runner`: pub items come before private items
src/a.rs:30:1: ORG001 symbol-order: impl `Display for Runner` should come after impl `Runner`: a type's impl blocks directly follow it
src/a.rs:41:5: ORG001 symbol-order: pub fn `new` should come before private fn `helper`: pub fns come before private fns
```

The anchor span runs from the first line of the earlier of the flagged and the reference item to the
last line of the later one, so a change anywhere between them, such as an inserted item that makes
an old one misplaced, reports it. The fn header is the flagged item's `ItemLines::header`, so an
`allow(symbol-order)` right above the item, or between its doc and its signature, covers it.

## type-params-doc

The rule checks the `# Type Parameters` section that the y-scope Rust `coding-style.md` requires
whenever an item has generic parameters, as the review guide's "`# Type Parameters` is required
when type parameters exist" repeats.

Checked items, from `doc_items::collect`: documented free fns (nested ones too), inherent methods,
trait method declarations (with or without a default body), structs, enums, unions, traits and type
aliases. Not checked:

* Trait impl methods: their docstring is optional, and the trait declaration documents the
  parameters.
* An item without a docstring. Like `errors-doc`, which skips a fn without an `# Errors` section,
  the rule only checks docstrings that exist.
* Test code, if the rule skips it, as `doc_items::collect` and `FileContext::is_test_module` decide.

An item's generic parameters are its own `Generics::params`, in declaration order:

* A lifetime parameter is named with its tick, `'a`, as Spider writes it in
  `components/spider-tdl/src/ffi.rs`.
* A type parameter and a const generic are named by their identifier.

Not parameters of the item: an impl block's own generics (a method documents only its own), a
trait's generics on its methods, elided and anonymous lifetimes (`&self`, `'_`), `impl Trait`
argument types, and `for<'a>` bound lifetimes.

Checks, with the [shared messages](#parameter-sections) of `TYPE_PARAMETERS`:

* A documented item with at least one generic parameter and no `# Type Parameters` section: the
  missing-section message at the docstring's first line, listing every parameter.
* A section on an item without generic parameters: the no-parameters message at the header.
* Otherwise, `compare(params, [])`: missing, unknown, duplicate, nameless and non-canonical bullets,
  and duplicate headers. A bullet must spell a lifetime with its tick, so `` `a` `` for `'a` is an
  unknown name plus a missing parameter.

Every diagnostic has the item's span as its anchor and the item's header as its fn header.

## trait-params-doc

The rule checks the two `# Parameters` conventions of the y-scope Rust `coding-style.md` and review
guide ("`# Parameters` only in trait declarations"): a trait declaration documents its methods'
parameters, and nothing else has a `# Parameters` section. The heading is `# Parameters`, as the
guides and the existing uses in CLP and Spider write it; `# Arguments` isn't recognized.

Checked items, from `doc_items::collect`, documented, and not test code if the rule skips it:

* Trait method declarations, with or without a default body.
* For the reverse check, every other item `doc_items::collect` returns: free fns, inherent methods,
  trait impl methods, structs, enums, unions, traits and type aliases.

A trait method's parameters are its typed inputs, without the receiver (`self`, `&self`, `&mut
self`, `self: Box<Self>`):

* An identifier pattern (`job_id: JobId`, `mut buffer: Vec<u8>`) is a required name. A name with a
  leading underscore, such as `_context`, is required too.
* `_` is skipped.
* A destructuring pattern (`(a, b): (u8, u8)`, `Point { x, y }: Point`) is skipped. The identifiers
  it binds are accepted names: a bullet may document them, but none is required. A binding with a
  subpattern (`point @ Point { .. }`) counts as a destructuring pattern, so `point` is accepted.

Checks, with the [shared messages](#parameter-sections) of `PARAMETERS`:

* A documented trait method with at least one required name and no `# Parameters` section: the
  missing-section message at the docstring's first line, listing the required names.
* A section on a trait method with neither required nor accepted names: the no-parameters message
  at the header.
* Otherwise, `compare(required, accepted)`.
* A `# Parameters` section on any other documented item: at the header, `` `# Parameters` belongs
  only in trait method declarations; remove it from <item> ``. Its bullets aren't checked.

Every diagnostic has the item's span as its anchor and the item's header as its fn header.

## Suppression internals

The suppression forms and their user-facing semantics are in
[`docs/suppression.md`](suppression.md). This section describes `src/suppression.rs`.

`suppression::parse(comment_lines, header_end, width)` runs inside `FileContext::new`, before any
rule, with the run's `comment-greedy-wrap` width:

* A comment line is a candidate when it is a plain `//` comment and its trimmed text starts with
  `yscope-linter:`. Doc comments are never candidates, so a suppression can't leak into rustdoc.
* `split_candidates` parses each candidate once with `Directive::parse` into a `Candidate`. The
  reason of an own-line candidate whose directive parses with a reason (`Directive.has_reason`)
  continues on the following lines for which `is_reason_continuation` holds: the line continues the
  block (`CommentLine::continues_block`), isn't empty or a candidate, and the line above it, the
  candidate or the last continuation line, is full before it. `greedy_wrap::is_full` decides that
  with the rule's own fit test (`UnderfilledLine::find` on the two lines as one paragraph), so the
  width, the column counting and the code spans and links that move whole are those of
  `comment-greedy-wrap`, and a reason that rustfmt's `wrap_comments` wraps stays one reason. These
  lines are taken from the stream, and `Candidate.last_line` is the last of them. A misplaced
  candidate continues too, since its reason is still a reason. An invalid or reasonless one, or a
  trailing one, continues on no line. Indentation plays no part.
* Candidates and their continuation lines are removed from the comment stream that
  `comment_lines()` returns. The removed lines break the run of consecutive lines, so a candidate of
  any form, valid or not, ends a `comment-greedy-wrap` block.
* `Directive::parse` reads the keyword of a `SuppressionKind`, the list of `RuleSelector`s and the
  reason. It expands the selectors to rules, without duplicates, and splits off the deprecated ones.
  An unknown keyword, an unknown or meta selector, or a missing reason (for every form but
  `allow-end`) is an `InvalidSuppression`.
* A misplaced directive, a trailing `allow-item` or an `allow-file` after the header, is invalid
  before its rules are looked at, so it gets no other diagnostic.
* A placed directive that names a deprecated rule gets one `unused-suppression` per deprecated rule,
  at its comment. If it names only deprecated rules, it becomes no suppression at all, so it opens
  and closes no region.
* A `Parser` processes the candidates in source order, and keeps the open regions.

The result is `ParsedComments`: the `Suppressions`, the remaining comment lines, the
`invalid-suppression` diagnostics, and the `unused-suppression` diagnostics of deprecated rules.

Each valid suppression has a `Scope`:

* `Unit { lines, is_statement_unit }`, for `allow`. A trailing `allow` covers its own line. An
  own-line `allow` covers the next unit, which starts at the first following line that isn't an
  own-line candidate or one of their continuation lines: the whole comment block starting there
  (`block_last_line`), or else that single line, with `is_statement_unit` set.
* `Item { lines }`, for `allow-item`, once located.
* `Region { start_line, end_line }`, for a matched `allow-start` and `allow-end`. `open_regions`
  opens one region per rule, and `close_regions` closes the open region of each rule it names.
  Regions that opened at the same `allow-start` and close at the same `allow-end` become one
  suppression. An unclosed start, an end without an open region, and a second start of an open rule
  are invalid.
* `File`, for `allow-file` at or before `header_end`. A later one is invalid.

`Suppression::covers` decides whether a suppression covers a diagnostic. Only rule codes that the
suppression names are covered, never meta codes. A `Unit` also covers a diagnostic whose fn header
intersects its lines, so one `allow` above a fn, or between its doc and its signature, covers every
`errors-doc` finding of the fn. A `Region` covers the lines strictly between its ends.

`allow-item` needs the syntax tree, so it is resolved in `FileContext::finish`:

* Parsing records an `UnlocatedItem` with its target line: the first following line that isn't an
  own-line candidate. A trailing `allow-item` is invalid.
* When some item is unlocated, `finish` parses the file and calls `syntax/item_lines.rs`, which
  collects every item with its span and its header, from its first attribute or doc line through the
  line where the item itself starts. Items include nested ones, and the fns, consts and types of
  impls and traits.
* `locate_items` picks the outermost item whose header holds the target line, and the suppression
  covers that item's span. Without one, the suppression is invalid.
* When the file failed to lex or parse, `reject_items` makes each `allow-item` invalid, with a
  message that says why its item can't be located.

A statement unit is extended in `FileContext::finish` too, after the `allow-item`s:

* When some unit has `is_statement_unit` (`has_statement_units`), `finish` gets the tree with
  `optional_syntax_tree`, so a parse failure records no skipped rule and adds no `parse-error`.
* `syntax/statement_lines.rs` collects every `syn::Stmt` but `Stmt::Item`, nested ones included,
  with `item_lines::of`, so each has a span and a header from its first attribute through the line
  where the statement itself starts.
* `statement_lines::collect` also records each statement's start, the position of its first token.
* `extend_statements` takes the statements whose header holds the unit's line and that start at or
  before the line's first token in the source text, and the unit then runs through the latest of
  their last lines. An item in a body, a line inside a statement, a statement that starts after
  other code on the line, or a statement in a macro body that `syn` doesn't parse, has no such
  statement, so its unit stays one line.
* When the file failed to lex or parse, every unit stays one line, with no diagnostic.

Since `covers` checks the fn header of a diagnostic against a `Unit`'s lines, an extended unit also
covers every finding whose rule records a header (`errors-doc`, `symbol-order`, `type-params-doc`
and `trait-params-doc`) of an item nested in the statement.

`Suppressions::apply` drops every diagnostic that some suppression covers, marking each covering
suppression as used. Then it adds an `unused-suppression` for every unused suppression whose rules
are all enabled in this run, at the suppression's comment (for a region, its `allow-start`). The
invalid-suppression diagnostics are added after `apply`, so they can't be suppressed. The line
filter then runs on everything, so an unused or invalid suppression is reported only when its own
line is in range.

## Diagnostics and output

`src/diagnostic.rs` defines:

* `DiagnosticKind`: `Rule(RuleId)`, or one of the meta diagnostics `ParseError`,
  `InvalidSuppression` and `UnusedSuppression`. `name` gives the rule or meta name, and `code` the
  stable `Code`, which is `LNT001` to `LNT003` for the meta diagnostics. Meta diagnostics are always
  on, aren't selected by `--rule`, and can't be suppressed.
* `Position`: a 1-based line and column, with columns in Unicode scalar values. `proc-macro2`
  columns are 0-based scalar counts, so `syntax/span.rs` adds 1.
* `Diagnostic`: a kind, a position, a message, an anchor span, and an optional fn header.
  `Diagnostic::new` sets the anchor to the position's line. `with_anchor` and `with_fn_header` widen
  it.

The anchor span is used only by the line filter, and is never printed:

* `comment-greedy-wrap`: the flagged line and the next line.
* `errors-doc`: the whole fn, from its first attribute or doc line to its closing brace. A CI run
  then reports every `errors-doc` finding of any fn it touches, including a body change that makes
  the docstring stale.
* `type-params-doc` and `trait-params-doc`: the whole item, as `item_lines::of` finds it.
* `symbol-order`: the lines from the misplaced item through the item it should be placed next to,
  so a change anywhere between them reports it.
* Meta codes: their own line.

The fn header is used only by `allow` suppressions, as [Suppression
internals](#suppression-internals) describes.

`FileDiagnostic` pairs a diagnostic with its file path, as `source::discover` normalized it.
`sort_and_dedup` sorts by path, line, column, rule or meta name and message, and removes the
duplicates that would print the same line. Sorting by name rather than by code keeps the order the
output had before codes existed. `render` prints the text format, `<path>:<line>:<column>: <code>
<name>: <message>`, or a GitHub workflow command titled `<code> <name>`. In the GitHub format, `%`,
`\r` and `\n` are percent-encoded everywhere, and `:` and `,` are also encoded in the property
values.

## Line ranges and the diff filter

Line filters let CI lint only the changed lines.

`src/line_ranges.rs` defines `LineSpan`, an inclusive 1-based span, and `LineRanges`, a set of spans
with `insert`, `union` and `intersects(span)`. `LineRanges` parses `N[-M](,N[-M])*`, where a bare
`N` means `N-N`, and rejects `0`, reversed ranges and overflow.

`source.rs` turns the inputs into `SourceFile`s:

* `TargetSpec::from_str` reads `FILE:RANGES` only when the text after the last `:` has the ranges
  form. Otherwise the whole argument is a path, so a path with a colon still works.
* `source::discover` normalizes every path lexically (dropping `.` components), drops the files
  under an exclude by whole components, and merges the inputs of the same file with
  `LineFilter::union`. A file given once without ranges is `LineFilter::All`.
* A directory is walked in sorted order. Directories named `target`, hidden directories and
  directory symlinks are skipped. A file named explicitly is linted even inside them.
* `is_generated` looks for the word `@generated` in a plain `//` comment among the first five lines.
  This skips prost and tonic output, which says `// This file is @generated by prost-build.`. A doc
  comment that mentions the marker doesn't count.

`diff::parse` turns a unified diff into the changed lines on the new side of each `.rs` file:

* A `+++ b/<path>` line starts a file, and `+++ /dev/null` (a deleted file) starts one whose hunks
  are skipped. A path without the `b/` prefix, or a quoted path (`+++ "b/a b.rs"`), is invalid.
* A hunk header `@@ -a[,b] +c[,d] @@` selects lines of the current file, where `d` defaults to 1.
  With `d > 0`, it selects `c..=c+d-1`. With `d == 0`, a pure deletion after line `c`, it selects
  `max(c, 1)..=c+1`, the lines on both sides of the deleted text.
* A header outside a file, a new side that starts at line 0 with lines (`+0`, `+0,1`), and a number
  that overflows `usize` are invalid, so external input never panics.
* The hunk's content lines are counted against its header, so a content line that looks like a
  header (`+++ x`) stays in its hunk. Context lines of a diff made without `-U0` are selected, since
  the header's range covers them.
* Every other line is ignored.

The pure-deletion rule is why no further widening is needed. A deleted `Forwards` bullet, a deleted
`foo()?;` line, or a deleted line that kept two comment paragraphs apart still selects a line of the
fn or comment it belonged to, and the anchor spans cover the rest.

## Testing

* Unit tests live in each module's `#[cfg(test)] mod tests`. A test module's `//!` doc names only
  the subject under test, never the cases.
* Classifiers are tested by tables, such as one test that classifies a sample body of every
  `LineKind`, including `#544)` and `<https://...>` as `Text`. `markdown/block_start.rs` has a test
  per construct, each in every `Preceding` it differs in, with the edge cases of the spec: an indent
  of 3 versus 4 columns, tabs, empty items, `1.` versus `2.`, and condition 6 versus 7 tags.
* Every `comment-greedy-wrap` exemption has a fixture that isn't flagged and a twin that is. The
  CommonMark-only kinds have only their row in the classification table.
* Every `errors-doc` finding kind has a minimal fixture that produces it, and a satisfied version
  that is clean. Each body-scan case and each keep rule of the reverse checks has its own test.
* `driver.rs` tests the interactions of the pipeline: a lex failure, a `syn` failure in a run with
  and without `errors-doc`, a rule that skips test code dropping only its own findings, the anchor
  spans against line filters, suppressions ending paragraphs, and the lazy parse for `allow-item`.
* Every `symbol-order`, `type-params-doc` and `trait-params-doc` check and exemption has a fixture
  that produces it or is clean, in the rule's own unit tests. `doc/section.rs`,
  `doc/param_section.rs`, `syntax/visibility.rs`, `syntax/crate_names.rs` and `syntax/doc_items.rs`
  have their own unit tests.
* `diagnostic.rs` freezes the code table: `codes_are_frozen` lists every `(code, name)` pair, so
  renumbering or reusing a code fails a test. Other tests check that the codes and names are unique,
  that each code is well-formed, and that each set is numbered from 1 without gaps.
* `diagnostic.rs` also tests that diagnostics at one position sort by name, not by code.
* The deprecation paths have no tests until a rule is deprecated. See [Deprecating a
  rule](#deprecating-a-rule).
* `tests/cli.rs` runs the built binary (`CARGO_BIN_EXE_yscope-linter`) on files each test writes
  under `CARGO_TARGET_TMPDIR`. It covers the output format and order, exit codes, the directory
  walk, `@generated` skipping, `--rule`, `--exclude`, the checked test code, `--skip-tests`,
  `--skip-tests=<ID>,...` and `--no-skip-tests`, the warning of `--include-tests`, `--diff -` and
  `--diff <FILE>`, including a diff that isn't UTF-8, `--format github` and `--version`.
* `cargo/metadata.rs` tests the package selection and the rejected manifest paths on hand-written
  `cargo metadata` output, `cargo/settings.rs` the parsing, the error context and the precedence of
  the settings, including a `skip-tests` boolean or list and the list replacing a lower level's, and
  `cli.rs` the settings of the command-line options, including the last of the `--skip-tests` and
  `--no-skip-tests` flags winning.
* `tests/cargo.rs` runs `CARGO_BIN_EXE_cargo-yscope-linter` with cargo's `yscope-linter` first
  argument on cargo workspaces that each test writes, so the real `cargo metadata` answers. It
  covers the default selection from the root and from a member, `-p`, `--workspace` and
  `--exclude-package`, a single package's targets and a custom target directory, the files outside
  every package and in nested packages, the settings' precedence and relative excludes, including
  `..` components and command-line excludes from a member directory, `skip-tests` overridden by
  `--skip-tests` and `--no-skip-tests`, a package's `skip-tests` list replacing the workspace's and
  an invalid entry in it, `--diff` limited to the selected packages and run from a member directory,
  relative paths in the GitHub format, and the fatal errors: a failing `cargo metadata`, a missing
  cargo, an invalid package or workspace setting and an unknown package.
* The repository lints itself, test code included, with `yscope-linter src tests` and with
  `cargo yscope-linter`, which must both be clean.

Before a release, run the tool on current y-scope/clp and y-scope/spider and triage every finding as
a true positive, a false positive (fix the tool, or add it to [Known limits](#known-limits)), or a
convention question. Seeding known misses (delete a `Forwards` bullet, add a stale variant bullet,
under-fill a bullet continuation) and checking that each is caught, both with a full run and with
`--diff` on the seeding change, verifies the diff filter end to end.

## Known limits

Comments:

* Block doc comments (`/** */`, `/*! */`) aren't checked by `comment-greedy-wrap`. Comments inside
  `macro_rules!` bodies are lexed and checked like any others.
* Columns count Unicode scalar values, and a tab counts as 1. rustfmt-formatted code has no tabs in
  comments.
* Commented-out code or pseudo-code without a formula operator (`// return x;` followed by `// }`)
  may be flagged.
* A hanging-indent continuation (`// total = a + b +` followed by `// c + d`) starts a preformatted
  paragraph, so the pair isn't checked. Telling a hanging indent from an aligned list or diagram
  needs more than syntax.

What `errors-doc` checks:

* A file that `syn` can't parse gets no `errors-doc` check, only a `parse-error`.
* Return-type aliases not ending in `Result` (`type Fallible<T> = ...`) aren't recognized, so the fn
  isn't checked.
* An `impl Future<Output = ..>` whose `Output` doesn't end in `Result` or whose body doesn't end
  with an `async` block, and `Pin<Box<dyn Future>>` and `Poll<Result>` returns, aren't scanned. For
  example, `components/spider-storage/src/cache/tree/control_block.rs` returns an `Output` alias.
* In a rule that skips test code, out-of-line test modules declared with `#[path]`, or inside an
  inline module of the parent, are checked as non-test code.
* `?` inside `assert!` arguments counts normally. It is rare outside tests, whose fns seldom have an
  `# Errors` section.
* The section's wording isn't read, so a section that states the fn never fails gets the intro
  finding when its body has an error source.

Errors the scan can't attribute:

* Macros whose tokens parse neither as expressions, statements nor the `select!` grammar
  (`sqlx::query!` with SQL) hide their `?` and `Err(..)`. Their identifiers are mentions, and a
  postfix `?` is an opaque source, so a `Forwards` bullet naming a call inside is kept.
* Errors flowing through variables other than call bindings (`Err(error)` outside match-forwarding,
  `self.reply.await?`, `x??` through a `JoinHandle`, `commit?` on a tuple binding, a variable
  reassigned after its `let`) are opaque sources, not obligations.
* An `Err(..)` bound with `let` and returned later, or the `break Err(..)` value of a loop, isn't in
  a return position, so it records nothing.
* `?` in closures and async blocks records no obligation, by design. Their callees are nested
  callees, which only keep `Forwards` bullets. A closure that a call in a return position runs in
  place is the exception.
* An in-place closure keeps its `?` sites and `return`s nested when its call isn't classified in a
  return position: when its result is bound and returned later (`let result = (|| ..)(); result?`),
  and when the call is under `.context(..)`, `.with_context(..)` or `.map_err(..)`, whose case only
  names the receiver's callee (`(|| ..)().context(..)?`), or in a `match` whose `Err(e)` arm wraps
  the error. Bindings store no syntax, so the first can't be deferred. Re-visiting the second would
  record a plain `Forward` where the adaptor makes the error an `Either`.
* The bindings are flat per fn: a `let` inside a closure or a block replaces the outer binding of
  the same name for the rest of the walk. For a tail in-place closure that uses a binding and then
  shadows it (`let result = load(); std::thread::scope(|_| { result?; let result = store(); result
  })`), the second visit resolves the earlier use to the shadowing binding, so `load` isn't
  required. Saving and restoring the binding maps around closures and blocks would fix it.
* The async exclusion of in-place closures is syntactic. A closure whose value is a future without
  a literal `async` block, such as `(|| Box::pin(async { .. }))().await?` or
  `(|| async { .. }.boxed())().await?`, runs in place, and its value is classified as the future
  itself: `Box::pin(async { load().await })` forwards `load` (or `Box::pin`), and
  `async { .. }.boxed()` forwards `boxed`.
* `IN_PLACE_FNS` is matched by last segment, so any fn named `scope` or `block_in_place` whose only
  argument is a closure runs it in place, `crossbeam::scope` included, whose `Err` is a panic
  payload. Method forms (`pool.scope(..)`, `span.in_scope(..)`) aren't recognized.
* A bound closure or call that ends in an error adaptor (`let create = |d|
  create_dir_all(d).context(..)`) keeps only the receiver's callee, not the adaptor's mapped errors,
  so a call of it is a `Forward`. A bullet for `anyhow::Error` or for the mapped variants alone
  doesn't document it, while the same call written in place, `create_dir_all(a).context(..)?`, is
  an `Either`. Bindings could carry the adaptor's `ObligationKind` instead.
* A call of a `let` closure binding forwards only the `tail_callee` of the closure's body, since the
  binding stores no syntax. The body's `?` sites, `return Err(..)`s and anyhow macros stay nested,
  and a call whose result the fn handles records nothing, as any call outside a return position
  does. [y-scope/yscope-linter#22](https://github.com/y-scope/yscope-linter/issues/22) tracks
  visiting the body at the site of each call in a return position, as `classify_in_place_closure`
  does.
* A helper that handles the errors of the closure it is given (`retry(|| self.send())`) is
  documented by the closure's callee too.
* Mappers and constructors defined in another file, or more than one helper level deep, are
  `EitherUnknown` with an `OpaqueError` site, which disables the fn's stale-variant check.
* A helper that passes on the error it is given in a shape other than the two pass-through shapes
  (`match error { DbError::Sql(e) => .., other => other }`) makes the doc forward the helper or list
  the variant.
* `From` conversions declared in another file aren't resolved. A variant converted into a wrapper
  enum by `.into()` or `?` is the inner variant, and a forwarded call's converted error is a
  `Forward` of the callee.

Matching:

* Name collisions (two different `send` methods) let one bullet satisfy both.
* Helpers of the same name in several impls of the file are narrowed by owner. When nothing fits,
  every fn of that name is merged.
* Renamed imports and aliases from another file, and a trait method called through a type whose
  trait the file names more than once, give a missing and stale pair.
* Only the `sqlx` builder fns of `BUILDER_FNS` are known, so a method on another builder matches by
  last segment only.
* A callable argument named by an unqualified path that the file doesn't declare (`execute(cfg,
  imported_fn)`) isn't a spelling of the call, except in a sink's callable position.
* A stale `Forwards` bullet is kept in a fn with an `OpaqueSource` site when another fn of the file
  calls its function. A function called only inside a task spawned in another file is stale, which
  is a false positive.
* A `Forwards` bullet whose call's error is handled (`if let Err(e) = foo() { log }`) isn't stale in
  a fn with an opaque site.
* The inherent fns of the [recursion rule](#callees) are found by the last segment of the impl's
  self type, file-wide, including nested modules and `#[cfg(test)]` code. So an inherent fn of an
  unrelated type with the same last segment makes a trait impl's same-name call a forward.
* A trait impl whose self type isn't a path (`&Client`, tuples, slices, `dyn Trait`) or is a type
  parameter of the impl keeps the recursion rule, although auto-deref may reach an inherent fn.
* A wrapper self type such as `Box<Client>` is looked up as `Box`, so its same-name `Self::` call is
  a possible forward, although Rust resolves it to the trait method.
* Method syntax ignores receiver kinds, so `self.m()` counts as a call of an inherent `m` even when
  Rust's method probe picks the trait method, such as for an inherent `fn m(self)` or a `self:
  Arc<Self>` receiver.
* In the possible case, a bullet that names the trait's method (`` [`Submitter::submit`] ``) is
  kept too, since the trait's name is a name of `Self`.
* `<Self>::m(..)` without a trait has no qualifier, so it forwards `m`.

What `symbol-order` checks:

* Crate-local traits are recognized by syntax only. A trait imported through a glob (`use
  super::*`), re-exported under another name in another file, or reached through a `use` that names
  a crate-local module of another file is external.
* `use` and inline modules are scoped file-wide, not per module, when `CrateNames` collects
  crate-local names.
* An impl block whose self type is a qualified path, a type alias or a generic parameter is exempt,
  even if it is placed far from its type.
* Lexicographically smallest kept items may flag an old item rather than the one a change inserted:
  inserting a private fn between two `pub` fns flags the later `pub` fn. The anchor span still
  covers the inserted lines.
* Items generated by macros are invisible, and macro invocations are exempt.

What `type-params-doc` and `trait-params-doc` check:

* Sections are recognized by their exact title, so a misspelled title (`# Type parameters`, `#
  Arguments`) is neither checked nor reported.
* A docstring built only from `#[doc = include_str!(..)]` counts as undocumented.
* Bullet order isn't checked.

Input:

* Quoted paths in `--diff` input (names with spaces or non-ASCII bytes) are an invalid argument.

Cargo mode:

* A target whose source file is outside its package directory (`[lib] path = "../shared/lib.rs"`)
  isn't linted.
* `.gitignore` isn't read. A package directory's untracked `.rs` files are linted, so a workspace
  whose root is also a package lints every untracked file under the root that isn't in another
  package, the target directory or a hidden directory.
* `-p` takes a package name only, not a package ID spec (`name@version`) or a glob.

## Rejected alternatives

* Keeping a trailing-`:` boundary, everywhere or in plain `//` comments only. The boundary would
  hide real misses, such as `// There should be 3 submissions:` followed by a short sentence in
  `components/log-ingestor/tests/test_compression_listener.rs`. The only false positive it prevented
  is a formula line after a colon, which `Formula` exempts on its own.
* Keeping a hard-break `\` boundary. The only `\` line ending in the y-scope code is in an already
  preformatted ASCII tree. An earlier `Html` line kind, matched on a leading `<`, misfired on
  `<https://...>` autolinks. `Html` is back as the classifier's HTML block start conditions, which
  autolinks don't meet.
* Splitting the next line into words at whitespace everywhere, code spans and links included. The
  rule then asked to pull up the first half of a code span, such as `` `AsyncFnOnce() `` from
  `` `AsyncFnOnce() -> DbConnectorType` ``, which leaves the span split across lines: unreadable,
  and not found by a search for the whole span.
* Tokenizing each line on its own. A code span or a link that already spans two lines would then be
  two runs of plain text, and the split that the rule exists to prevent would go unreported.
* Leaving split tokens in list items unreported, since rustfmt fills list items at whitespace. The
  rule stays strict there, as it does for the fields that rustfmt wraps one column early, and its
  message asks to reword the item.
* Asking for the first word only, and leaving the Markdown check to the reader. A move that turns
  the rest of the line into a block can't be applied, so the rule would ask for a fill it then
  rejects, or worse, one that changes the docs.
* Dropping the `#[cfg(test)]` skip. It is kept behind `--skip-tests`, and the inconsistency between
  inline and out-of-line modules is fixed by the parent-file lookup instead. Integration test crates
  under `tests/` are neither, so their helpers stay checked, which is why pattern-taking macros are
  handled.
* Dropping `ForwardList`. It has a real use in
  `components/log-ingestor/src/ingestion_job/sqs_listener.rs`, which would otherwise be an
  unnamed-forward false positive.
* Treating a tail call as not forwarding. A tail call returns the callee's `Result`, so its errors
  leave the fn as they do with `?`. The false positives of tail calls such as
  `OnceCell::get_or_try_init(Self::init)` came from the callable argument, which now documents the
  call.
* Requiring the calls inside a closure passed to a helper. `?` in a closure belongs to the closure,
  unless the helper is one of the `IN_PLACE_FNS`, which return the closure's value. The closure's
  built errors document the helper call instead, and its calls keep `Forwards` bullets as nested
  callees.
* Skipping the nested-scope depth for every in-place closure in the visitor. A closure called on the
  spot and handled locally, `let r = (|| ..)(); if let Err(e) = r { warn!(..) }`, is the try-block
  idiom, and its `?` sites would then become obligations of the fn. The second visit is driven by
  classification in a return position instead.
* Resolving `From` conversions across files. It needs module resolution, which is a non-goal.
  Same-file conversions cover the false positives seen, and a variant bullet can name the converted
  callee in its condition.
* Skipping the stale-variant check whenever the body has any opaque site. A `?` on a variable is
  common and says nothing about which variants the fn builds. Only an unresolved mapper
  (`OpaqueError`) disables the check, and an `OpaqueSource` keeps only the bullets whose heads name
  error types.
* Pairing a converted foreign error with any unmatched own-type variant bullet. It accepted any
  unrelated `Error::*` bullet and kept every such bullet from going stale, so one conversion
  silenced the section.
* Pairing any two qualified paths with the same last segment in the loose pass. It accepted a
  `Forwards` bullet that names the wrong function, such as `` [`Cache::load`] `` for
  `Storage::load()?`. The loose pass needs a trait or an alias to link the qualifiers.
* Checking more `impl Future` shapes. `Output` is often an alias that doesn't end in `Result`, so
  the gain is small.
* A `--targets-from` input beside `--diff`. `--diff` covers CI, including pure deletions, and
  `FILE:RANGES` targets cover ad hoc use.
* Positional targets in cargo mode, intersected with the packages' files. The path mode already
  lints given files, and the intersection would have to define which package's settings a file
  outside every package gets. `--diff` is the narrowing that CI needs.
* Naming the package exclusion `--exclude`, as cargo does. It would change the meaning of the
  existing `--exclude <PATH>` between the two binaries.
* Warning, as cargo does, about an `--exclude-package` name that matches no member. A typo would
  then lint the package it meant to skip, with only a warning that CI doesn't fail on.
* Overriding excludes by precedence, like the other settings. A package that excludes its generated
  module would then drop the workspace's excludes, which can point into any package.
* Reading `Cargo.toml` settings in the path mode. A path target has no package, and a file's nearest
  `Cargo.toml` isn't necessarily its package's, so the path mode stays free of cargo.
* Skipping test code by default, with `--include-tests` to check it. The y-scope guides apply to
  test code too, and a skip that is on by default hides findings that nobody asked to hide.
* One `--skip-tests[=<BOOL>]` flag with an optional value. `--no-skip-tests` beside it is the form
  clap and cargo users know for a boolean, and the optional value is the list of rules instead.
* A separate repeatable flag for the list, such as `--skip-tests-for <RULE>`. It would be a fourth
  flag for one setting, and would need its own rule for how it combines with `--skip-tests` and
  `--no-skip-tests`. `--skip-tests=<ID>,...` mirrors the `skip-tests` setting, which is a boolean or
  a list.
* Adding up `skip-tests` lists across levels, as `excludes` do. A package's list replaces the
  workspace's, as `rules` does, so the package's table says in full which rules skip its test code.
  An exclude is a path that can point into any package, which is why excludes add up.
* Two per-rule `bool`s, whether a rule has a test-code mode and its default. A rule without a
  test-code mode would still declare a default that nothing reads. One `TestCodeMode` rules that
  combination out, and its exhaustive match still makes every rule, including a future one, decide.
* Keeping `include-tests` as a deprecated setting. No release read it, so there is no `Cargo.toml`
  to migrate, and a second key for one setting would need its own precedence rule.
* Making `--include-tests` act as `--no-skip-tests`. No release read settings, so the flag had no
  setting to override, and as a no-op it keeps the output of every released invocation.

## Open questions

* `_`-prefixed trait method parameters. `trait-params-doc` requires a bullet for `_context` as for
  any named parameter. If CLP and Spider treat such parameters as undocumented by convention, they
  could join `_` in being skipped.
* Non-canonical parameter bullets. A few existing sections write ``* [`TypeMarker`]: ...``. The
  param-doc rules report them; if that is too strict, `NonCanonicalBullet` could be dropped while
  the name still counts.

* Public facades that list leaf variants. `components/spider-client/src/client.rs` documents the
  leaf `ClientError` variants of each delegated call instead of forwarding the call. The convention
  says to forward helpers instead of re-listing every leaf error, so the tool reports one delegation
  finding per fn. If public client APIs may flatten leaf variants, the merge could be skipped for
  `pub` fns whose body is a single tail call.
* A `NOTE:` paragraph followed by an unrelated sentence.
  `components/spider-storage/src/cache/job.rs` has a `// NOTE:` paragraph followed, with no blank
  line, by a sentence about `AcqRel`. The tool treats them as one paragraph and flags the short
  line. The fix in the code is a blank `//` line between them. It is open whether that is the
  intended convention, or whether a tagged paragraph should end before such a sentence.
