use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use syn::visit::Visit;

use crate::check::{Check, ConfiguredCheck, count_applicable, run_per_text_file};
use crate::input::{ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

/// Config-doorway field names a check must never declare at the top level of
/// its own deserialized config struct: each one answers "is this file a
/// target of this check at all", which is the framework's question to
/// answer via the `include` / `exclude` keys on the check entry, not the
/// check's own config.
///
/// Both `include` and `applies_to` are live framework spellings (see
/// `src/config.rs`'s check-entry `include` and
/// `src/external/declarative/resolve.rs::override_applies_to`) and both are
/// denylisted here even though they are valid framework keys: a check that
/// parses the framework's own word for a check-level scope is
/// re-implementing the framework's job under a name that reads as
/// legitimate, which is *harder* to catch in review than a novel word like
/// `paths`.
///
/// A check MAY still declare one of these words nested under a finer-axis
/// construct it owns (e.g. `rules[].include`, `patterns[].include`) — see
/// `src/checks/forbidden_imports_deps.rs`'s `ForbiddenImportsDepsRuleConfig`
/// for the sanctioned shape. This denylist is applied only to the fields of
/// the top-level struct resolved at the check's config doorway; a nested
/// struct's own fields are out of scope by construction.
const DENYLISTED_TOP_LEVEL_FIELDS: &[&str] = &[
    "include",
    "applies_to",
    "paths",
    "path",
    "path_globs",
    "include_globs",
    "file_globs",
    "files",
    "only",
    "targets",
    "scope",
    "globs",
];

const BUILTIN_CHECKS_ROOT: &str = "tools/checkleft/src/checks/";

#[derive(Debug, Default)]
pub struct NoCheckLevelFileScopingCheck;

#[async_trait]
impl Check for NoCheckLevelFileScopingCheck {
    fn id(&self) -> &str {
        "checkleft/no-check-level-file-scoping"
    }

    fn description(&self) -> &str {
        "flags a check's own config struct for declaring a top-level file-scoping field the framework already owns via `include`/`exclude`"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(NoCheckLevelFileScopingConfigured))
    }
}

struct NoCheckLevelFileScopingConfigured;

#[async_trait]
impl ConfiguredCheck for NoCheckLevelFileScopingConfigured {
    fn applicable_file_count(&self, changeset: &ChangeSet) -> usize {
        count_applicable(changeset, is_check_source_path)
    }

    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        let findings = run_per_text_file(
            changeset,
            tree,
            is_check_source_path,
            &*on_file_processed,
            |changed_file, contents, findings| {
                findings.extend(inspect_check_source(&changed_file.path, contents, tree));
            },
        );

        Ok(CheckResult {
            check_id: "checkleft/no-check-level-file-scoping".to_owned(),
            findings,
        })
    }
}

/// Defensive mirror of the framework `include` scope this check is
/// registered under (`tools/checkleft/checks/**/src/lib.rs` and
/// `tools/checkleft/src/checks/**/*.rs`). The framework already restricts
/// the changeset this check receives; this predicate just keeps
/// `applicable_file_count`'s reported denominator accurate if it is ever
/// invoked without that scoping (e.g. a future `--all` codepath change).
fn is_check_source_path(path: &Path) -> bool {
    let path = path.to_string_lossy();
    (path.starts_with("tools/checkleft/checks/") && path.ends_with("/src/lib.rs"))
        || path.starts_with(BUILTIN_CHECKS_ROOT)
}

fn inspect_check_source(path: &Path, contents: &str, tree: &dyn SourceTree) -> Vec<Finding> {
    let file = match syn::parse_file(contents) {
        Ok(file) => file,
        Err(err) => {
            return vec![inspection_failure_finding(
                path,
                &format!("failed to parse as Rust source: {err}"),
            )];
        }
    };

    let doorway_types = find_doorway_config_types(&file);
    if doorway_types.is_empty() {
        // No config doorway in this file: nothing for this check to scope,
        // which is a legitimate state (many check sources have no config,
        // or this file is a helper module the doorway lives elsewhere in).
        return Vec::new();
    }

    let mut findings = Vec::new();
    for type_name in doorway_types {
        match resolve_top_level_struct(&type_name, &file, path, tree) {
            Ok(Some(item_struct)) => findings.extend(denylisted_field_findings(path, contents, &item_struct)),
            Ok(None) => findings.push(inspection_failure_finding(
                path,
                &format!(
                    "its config doorway deserializes into `{type_name}`, but no `struct {type_name}` definition \
                     could be found anywhere in this check's source — cannot verify it declares no check-level \
                     file-scoping field"
                ),
            )),
            Err(err) => findings.push(inspection_failure_finding(path, &format!("{err:#}"))),
        }
    }
    findings
}

/// Find every distinct type resolved at this file's config-deserialization
/// doorway: a `let <ident>: <Type> = <expr>;` whose initializer calls
/// `.try_into()` (the built-in-check doorway, `src/check.rs`'s
/// `Check::configure`) or `.config()` (the wasm-guest doorway,
/// `CheckInput::config::<T>()` in `sdk/src/lib.rs`) anywhere in its
/// expression tree. Test code (`#[cfg(test)]` modules) is excluded so
/// fixture-only types never produce a doorway candidate.
fn find_doorway_config_types(file: &syn::File) -> Vec<String> {
    struct DoorwayVisitor {
        found: Vec<String>,
    }

    impl<'ast> Visit<'ast> for DoorwayVisitor {
        fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
            if has_cfg_test(&node.attrs) {
                return;
            }
            syn::visit::visit_item_mod(self, node);
        }

        fn visit_local(&mut self, node: &'ast syn::Local) {
            if let syn::Pat::Type(pat_type) = &node.pat
                && let Some(init) = &node.init
                && expr_calls_config_doorway(&init.expr)
                && let Some(name) = type_ident(&pat_type.ty)
            {
                self.found.push(name);
            }
            syn::visit::visit_local(self, node);
        }
    }

    let mut visitor = DoorwayVisitor { found: Vec::new() };
    visitor.visit_file(file);
    visitor.found.sort();
    visitor.found.dedup();
    visitor.found
}

/// Does `expr`'s tree contain a `.try_into()` or `.config()` method call
/// anywhere (through `?`, `.unwrap_or_default()`, a `match`, etc.)?
fn expr_calls_config_doorway(expr: &syn::Expr) -> bool {
    struct DoorwayCallFinder(bool);

    impl<'ast> Visit<'ast> for DoorwayCallFinder {
        fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
            if node.method == "try_into" || node.method == "config" {
                self.0 = true;
            }
            syn::visit::visit_expr_method_call(self, node);
        }
    }

    let mut finder = DoorwayCallFinder(false);
    finder.visit_expr(expr);
    finder.0
}

fn type_ident(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(type_path) => type_path.path.segments.last().map(|segment| segment.ident.to_string()),
        _ => None,
    }
}

fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("cfg") && attr.parse_args::<syn::Ident>().ok().is_some_and(|id| id == "test"))
}

/// Resolve `type_name` to its struct definition, first within `file` itself
/// (every check in the tree today keeps its doorway and its config struct in
/// the same file), then falling back to the rest of the check's own source —
/// the whole `src/checks/**` crate for a built-in, or the sibling files under
/// the same wasm check's `src/` directory — so a check that splits the two
/// across files is still resolved correctly rather than silently skipped.
fn resolve_top_level_struct(
    type_name: &str,
    file: &syn::File,
    path: &Path,
    tree: &dyn SourceTree,
) -> Result<Option<syn::ItemStruct>> {
    if let Some(item) = find_struct_in_file(file, type_name) {
        return Ok(Some(item));
    }

    let fallback_glob = fallback_search_glob(path);
    let candidates = tree.glob(&fallback_glob).with_context(|| {
        format!(
            "failed to glob `{fallback_glob}` while resolving `{type_name}` for {}",
            path.display()
        )
    })?;

    for candidate in candidates {
        if candidate == path {
            continue;
        }
        let bytes = tree
            .read_file(&candidate)
            .with_context(|| format!("failed to read {} while resolving `{type_name}`", candidate.display()))?;
        let text = String::from_utf8(bytes).with_context(|| format!("{} is not valid UTF-8", candidate.display()))?;
        let sibling_file = syn::parse_file(&text)
            .with_context(|| format!("failed to parse {} as Rust source", candidate.display()))?;
        if let Some(item) = find_struct_in_file(&sibling_file, type_name) {
            return Ok(Some(item));
        }
    }

    Ok(None)
}

fn fallback_search_glob(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    if path_str.starts_with(BUILTIN_CHECKS_ROOT) {
        format!("{BUILTIN_CHECKS_ROOT}**/*.rs")
    } else {
        match path.parent() {
            Some(parent) => format!("{}/**/*.rs", parent.display()),
            None => "**/*.rs".to_owned(),
        }
    }
}

fn find_struct_in_file(file: &syn::File, name: &str) -> Option<syn::ItemStruct> {
    struct StructFinder<'a> {
        name: &'a str,
        found: Option<syn::ItemStruct>,
    }

    impl<'ast> Visit<'ast> for StructFinder<'_> {
        fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
            if has_cfg_test(&node.attrs) || self.found.is_some() {
                return;
            }
            syn::visit::visit_item_mod(self, node);
        }

        fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
            if self.found.is_none() && node.ident == self.name {
                self.found = Some(node.clone());
            }
        }
    }

    let mut finder = StructFinder { name, found: None };
    finder.visit_file(file);
    finder.found
}

fn denylisted_field_findings(path: &Path, contents: &str, item_struct: &syn::ItemStruct) -> Vec<Finding> {
    let syn::Fields::Named(fields) = &item_struct.fields else {
        return Vec::new();
    };

    fields
        .named
        .iter()
        .filter_map(|field| {
            let ident = field.ident.as_ref()?;
            let name = ident.to_string();
            if !DENYLISTED_TOP_LEVEL_FIELDS.contains(&name.as_str()) {
                return None;
            }

            Some(Finding {
                fixable: false,
                severity: Severity::Error,
                message: format!(
                    "check config struct `{}` declares a top-level `{name}` field — deciding \"is this file a \
                     target of this check\" is the framework's job (the check-entry `include` / `exclude` keys), \
                     not the check's own config",
                    item_struct.ident
                ),
                location: Some(Location {
                    path: path.to_path_buf(),
                    line: field_declaration_line(contents, &name),
                    column: None,
                }),
                surface: None,
                remediations: vec![
                    "Remove this field and let the CHECKS-file `include` / `exclude` keys decide which files this \
                     check runs on. If you need a selector finer than \"is this file a target at all\" — e.g. \
                     which of several rules or patterns applies — nest it under that finer construct using the \
                     framework's own word, e.g. `rules[].include` or `patterns[].include`."
                        .to_owned(),
                ],
                suggested_fix: None,
            })
        })
        .collect()
}

fn inspection_failure_finding(path: &Path, detail: &str) -> Finding {
    Finding {
        fixable: false,
        severity: Severity::Error,
        message: format!(
            "checkleft/no-check-level-file-scoping could not verify {} has no check-level file-scoping field: {detail}",
            path.display()
        ),
        location: Some(Location {
            path: path.to_path_buf(),
            line: None,
            column: None,
        }),
        surface: None,
        remediations: vec![
            "Fix this check source so its config-deserialization doorway is statically resolvable (a single `let \
             cfg: Config = ...` conversion of a struct defined somewhere in this check's own source)."
                .to_owned(),
        ],
        suggested_fix: None,
    }
}

/// Scan `source` for the 1-based line number where `<field_name>:` is
/// declared as a struct field. Text-based rather than span-based because
/// `syn::parse_file` spans carry no line/column info without the
/// `proc-macro2` `span-locations` feature.
fn field_declaration_line(source: &str, field_name: &str) -> Option<u32> {
    for (index, line) in source.lines().enumerate() {
        let trimmed = strip_pub_prefix(line.trim_start());
        let Some(after_name) = trimmed.strip_prefix(field_name) else {
            continue;
        };
        if after_name.trim_start().starts_with(':') {
            return Some((index + 1) as u32);
        }
    }
    None
}

fn strip_pub_prefix(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("pub") else {
        return line;
    };
    match rest.chars().next() {
        Some('(') => rest.find(')').map(|end| rest[end + 1..].trim_start()).unwrap_or(line),
        Some(c) if c.is_whitespace() => rest.trim_start(),
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::NoCheckLevelFileScopingCheck;
    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::source_tree::LocalSourceTree;

    async fn run_check(path: &str, contents: &str) -> Vec<crate::output::Finding> {
        let temp = tempfile::tempdir().expect("create temp dir");
        let full_path = temp.path().join(path);
        fs::create_dir_all(full_path.parent().expect("parent dir")).expect("create dirs");
        fs::write(&full_path, contents).expect("write check source");

        let check = NoCheckLevelFileScopingCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new(path).to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::map::Map::new()),
            )
            .await
            .expect("run check");
        result.findings
    }

    /// The shape mono#2554 originally shipped: a generic wasm check that
    /// rolls its own top-level `paths` scoping key instead of relying on
    /// the framework's `include` / `exclude`.
    const VIOLATING_WASM_CHECK: &str = r#"
use checkleft_check_sdk::{CheckInput, Finding, check};
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    message: Option<String>,
}

#[check(id = "example/generic", description = "an example generic check")]
fn run(input: &CheckInput) -> Vec<Finding> {
    let cfg: Config = input.config().unwrap_or_default();
    let _ = cfg.paths;
    Vec::new()
}
"#;

    #[tokio::test]
    async fn flags_top_level_path_key_on_a_generic_wasm_check() {
        let findings = run_check(
            "tools/checkleft/checks/example/generic/src/lib.rs",
            VIOLATING_WASM_CHECK,
        )
        .await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("`paths`"), "{findings:?}");
        assert_eq!(findings[0].location.as_ref().and_then(|l| l.line), Some(8));
    }

    #[tokio::test]
    async fn flags_top_level_include_key_even_though_include_is_a_valid_framework_word() {
        let source = r#"
use checkleft_check_sdk::{CheckInput, Finding, check};
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    include: Vec<String>,
}

#[check(id = "example/generic", description = "an example generic check")]
fn run(input: &CheckInput) -> Vec<Finding> {
    let cfg: Config = input.config().unwrap_or_default();
    let _ = cfg.include;
    Vec::new()
}
"#;
        let findings = run_check("tools/checkleft/checks/example/generic/src/lib.rs", source).await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("`include`"), "{findings:?}");
    }

    #[tokio::test]
    async fn flags_top_level_applies_to_key_the_second_framework_spelling() {
        let source = r#"
use checkleft_check_sdk::{CheckInput, Finding, check};
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    applies_to: Vec<String>,
}

#[check(id = "example/generic", description = "an example generic check")]
fn run(input: &CheckInput) -> Vec<Finding> {
    let cfg: Config = input.config().unwrap_or_default();
    let _ = cfg.applies_to;
    Vec::new()
}
"#;
        let findings = run_check("tools/checkleft/checks/example/generic/src/lib.rs", source).await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("`applies_to`"), "{findings:?}");
    }

    /// The permitted finer-axis shape: a top-level `rules` list, with the
    /// framework's own word nested under it — mirrors
    /// `ForbiddenImportsDepsRuleConfig` in `src/checks/forbidden_imports_deps.rs`.
    #[tokio::test]
    async fn allows_nested_include_under_a_finer_axis_rules_list() {
        let source = r#"
use checkleft_check_sdk::{CheckInput, Finding, check};
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    rules: Vec<RuleConfig>,
}

#[derive(Deserialize)]
struct RuleConfig {
    #[serde(default)]
    include: Vec<String>,
    message: String,
}

#[check(id = "example/generic", description = "an example generic check")]
fn run(input: &CheckInput) -> Vec<Finding> {
    let cfg: Config = input.config().unwrap_or_default();
    let _ = cfg.rules;
    Vec::new()
}
"#;
        let findings = run_check("tools/checkleft/checks/example/generic/src/lib.rs", source).await;
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[tokio::test]
    async fn flags_top_level_scoping_key_on_a_builtin_check() {
        let source = r#"
use std::sync::Arc;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::check::{Check, ConfiguredCheck};

struct ExampleCheck;

#[async_trait]
impl Check for ExampleCheck {
    fn id(&self) -> &str {
        "example/builtin"
    }

    fn description(&self) -> &str {
        "an example built-in check"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        let parsed: ExampleConfig = config.clone().try_into().context("invalid example config")?;
        let _ = parsed.file_globs;
        todo!()
    }
}

#[derive(Debug, Deserialize)]
struct ExampleConfig {
    #[serde(default)]
    file_globs: Vec<String>,
}
"#;
        let findings = run_check("tools/checkleft/src/checks/example.rs", source).await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("`file_globs`"), "{findings:?}");
    }

    #[tokio::test]
    async fn allows_a_builtin_check_with_no_denylisted_field() {
        let source = r#"
use std::sync::Arc;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::check::{Check, ConfiguredCheck};

struct ExampleCheck;

#[async_trait]
impl Check for ExampleCheck {
    fn id(&self) -> &str {
        "example/builtin"
    }

    fn description(&self) -> &str {
        "an example built-in check"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        let parsed: ExampleConfig = config.clone().try_into().context("invalid example config")?;
        let _ = parsed.max_lines;
        todo!()
    }
}

#[derive(Debug, Deserialize)]
struct ExampleConfig {
    #[serde(default)]
    max_lines: Option<u64>,
}
"#;
        let findings = run_check("tools/checkleft/src/checks/example.rs", source).await;
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[tokio::test]
    async fn ignores_a_config_doorway_that_only_exists_inside_cfg_test() {
        // A `try_into()` doorway and its violating struct, both declared
        // entirely inside a `#[cfg(test)]` module, must never surface a
        // doorway candidate: test-only fixture code isn't the check's real
        // config surface.
        let source = r#"
use std::sync::Arc;
use anyhow::Result;
use async_trait::async_trait;

use crate::check::{Check, ConfiguredCheck};

struct ExampleCheck;

#[async_trait]
impl Check for ExampleCheck {
    fn id(&self) -> &str {
        "example/builtin"
    }

    fn description(&self) -> &str {
        "an example built-in check with no production config"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct TestOnlyConfig {
        paths: Vec<String>,
    }

    #[test]
    fn parses_a_fixture_config() {
        let cfg: TestOnlyConfig = toml::Value::Table(Default::default()).try_into().unwrap();
        let _ = cfg.paths;
    }
}
"#;
        let findings = run_check("tools/checkleft/src/checks/example.rs", source).await;
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[tokio::test]
    async fn fails_loudly_on_unparseable_check_source() {
        let findings = run_check("tools/checkleft/src/checks/example.rs", "this is not valid { rust (").await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, crate::output::Severity::Error);
        assert!(findings[0].message.contains("failed to parse"), "{findings:?}");
    }

    #[tokio::test]
    async fn fails_loudly_when_the_doorway_type_cannot_be_resolved() {
        // A doorway call whose target type is never defined anywhere in this
        // check's own source (e.g. imported from elsewhere) must not be
        // silently skipped — the guard cannot verify it, so it must say so.
        let source = r#"
use checkleft_check_sdk::{CheckInput, Finding, check};
use some_other_crate::ImportedConfig;

#[check(id = "example/generic", description = "an example generic check")]
fn run(input: &CheckInput) -> Vec<Finding> {
    let cfg: ImportedConfig = input.config().unwrap_or_default();
    let _ = cfg;
    Vec::new()
}
"#;
        let findings = run_check("tools/checkleft/checks/example/generic/src/lib.rs", source).await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, crate::output::Severity::Error);
        assert!(findings[0].message.contains("ImportedConfig"), "{findings:?}");
    }

    #[tokio::test]
    async fn allows_a_check_source_with_no_config_doorway() {
        let source = r#"
use std::sync::Arc;
use anyhow::Result;
use async_trait::async_trait;

use crate::check::{Check, ConfiguredCheck};

struct ExampleCheck;

#[async_trait]
impl Check for ExampleCheck {
    fn id(&self) -> &str {
        "example/builtin"
    }

    fn description(&self) -> &str {
        "an example built-in check with no config"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        todo!()
    }
}
"#;
        let findings = run_check("tools/checkleft/src/checks/example.rs", source).await;
        assert!(findings.is_empty(), "{findings:?}");
    }
}
