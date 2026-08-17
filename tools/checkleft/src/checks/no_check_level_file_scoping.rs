//! Guard: a check must not declare check-level file scoping in its own config.
//!
//! Anchored on the config-deserialization doorway (guest: `CheckInput::config`,
//! built-in: `try_into` of a `&toml::Value` configure parameter). The denylist
//! is matched against effective serde *config keys* (ident + `rename` +
//! `alias` + container `rename_all`), not bare Rust field identifiers.
//!
//! **Surfaces**
//! - Guests: `tools/checkleft/checks/**/src/lib.rs`
//! - Built-ins: `tools/checkleft/src/checks/**/*.rs`
//!
//! **Built-in vs guest denylist asymmetry** (design decision 4): framework
//! spellings `include` / `applies_to` are denylisted only on the *guest*
//! surface. Built-in doorway structs may accept those keys as deliberate
//! framework-key pass-throughs for `deny_unknown_fields` (see
//! `DocStructureConfig`) — they are legal by rule definition on built-ins,
//! not via per-check exemption. Bespoke scoping words (`paths`, `file_globs`,
//! …) are denylisted on both surfaces.
//!
//! **Test code.** Modules gated by any `cfg` predicate that mentions `test`
//! (including `cfg(all(test, …))`) are skipped. Sibling files named
//! `tests.rs` under the built-in tree are also skipped: the `#[cfg(test)]`
//! sits on `mod tests;` in the parent, not inside the file, so the guard
//! cannot see the attribute and would otherwise treat fixture doorways as
//! production config.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use syn::visit::Visit;

use crate::check::{Check, ConfiguredCheck, count_applicable};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

/// Bespoke check-level scoping field names, denylisted on both surfaces.
/// Framework spellings (`include`, `applies_to`) are guest-only — see module
/// docs and `denylist_for_path`.
const BESPOKE_DENYLISTED_TOP_LEVEL_FIELDS: &[&str] = &[
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
const GUEST_CHECKS_ROOT: &str = "tools/checkleft/checks/";

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
        // Iterate applicable files directly so a read or UTF-8 failure is a
        // loud inspection error, not a silent skip (unlike run_per_text_file).
        let mut findings = Vec::new();
        let mut processed = 0usize;

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) || !is_check_source_path(&changed_file.path) {
                continue;
            }

            match tree.read_file(&changed_file.path) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(contents) => {
                        findings.extend(inspect_check_source(&changed_file.path, &contents, tree));
                    }
                    Err(err) => {
                        findings.push(inspection_failure_finding(
                            &changed_file.path,
                            &format!("file is not valid UTF-8: {err}"),
                        ));
                    }
                },
                Err(err) => {
                    findings.push(inspection_failure_finding(
                        &changed_file.path,
                        &format!("failed to read file: {err:#}"),
                    ));
                }
            }

            processed += 1;
            on_file_processed(processed);
        }

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
    if path.starts_with(GUEST_CHECKS_ROOT) && path.ends_with("/src/lib.rs") {
        return true;
    }
    // Built-in production sources only. Sibling `tests.rs` files are
    // `#[cfg(test)] mod tests` targets whose attribute lives on the parent
    // declaration, not inside the file — skip them so fixture doorways
    // never become blocking false positives.
    path.starts_with(BUILTIN_CHECKS_ROOT) && path.ends_with(".rs") && !path.ends_with("/tests.rs")
}

fn is_guest_source_path(path: &Path) -> bool {
    path.to_string_lossy().starts_with(GUEST_CHECKS_ROOT)
}

fn denylist_for_path(path: &Path) -> &'static [&'static str] {
    if is_guest_source_path(path) {
        // Guest surface: bespoke words + both framework spellings (design
        // decision 4: "Both framework spellings belong on it for guest-side
        // config"). Built-in surface deliberately omits include/applies_to so
        // framework-key pass-throughs (DocStructureConfig) are legal by rule.
        static GUEST_DENYLIST: &[&str] = &[
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
        GUEST_DENYLIST
    } else {
        BESPOKE_DENYLISTED_TOP_LEVEL_FIELDS
    }
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

    let doorway = find_doorway_resolution(&file);
    if doorway.types.is_empty() && doorway.unresolvable.is_empty() {
        // No config doorway in this file: legitimate (helpers, no-config checks).
        return Vec::new();
    }

    let mut findings = Vec::new();
    for detail in &doorway.unresolvable {
        findings.push(inspection_failure_finding(path, detail));
    }

    let denylist = denylist_for_path(path);
    for type_name in doorway.types {
        match resolve_top_level_struct(&type_name, &file, path, tree) {
            Ok(Some(item_struct)) => {
                findings.extend(denylisted_field_findings(path, contents, &item_struct, denylist));
            }
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

struct DoorwayResolution {
    types: Vec<String>,
    unresolvable: Vec<String>,
}

/// Resolve every distinct config type at this file's deserialization doorway,
/// and record call sites where a doorway is present but no type can be named.
///
/// Doorways are anchored on the receiver, not bare method names:
/// - built-in: `try_into()` whose receiver chain references a `&toml::Value`
///   parameter of the enclosing function (typically `configure` /
///   `configure_scoped` / a `parse_config` helper);
/// - guest: `.config()` whose receiver is a `CheckInput` / `&CheckInput`
///   parameter.
///
/// Type extraction covers typed `let` bindings, turbofish on the call, and
/// `T::deserialize` / `from_str::<T>`-style paths. Test-gated modules are
/// excluded.
fn find_doorway_resolution(file: &syn::File) -> DoorwayResolution {
    struct DoorwayVisitor {
        types: BTreeSet<String>,
        unresolvable: Vec<String>,
    }

    impl DoorwayVisitor {
        fn record_type(&mut self, name: String) {
            self.types.insert(name);
        }

        fn record_unresolvable(&mut self, detail: String) {
            if !self.unresolvable.contains(&detail) {
                self.unresolvable.push(detail);
            }
        }
    }

    impl<'ast> Visit<'ast> for DoorwayVisitor {
        fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
            if has_cfg_test(&node.attrs) {
                return;
            }
            syn::visit::visit_item_mod(self, node);
        }

        fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
            if has_cfg_test(&node.attrs) {
                return;
            }
            self.visit_fn_like(&node.sig, &node.block.stmts);
        }

        fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
            if has_cfg_test(&node.attrs) {
                return;
            }
            self.visit_fn_like(&node.sig, &node.block.stmts);
        }
    }

    impl DoorwayVisitor {
        fn visit_fn_like(&mut self, sig: &syn::Signature, stmts: &[syn::Stmt]) {
            let toml_params = toml_value_param_names(sig);
            let input_params = check_input_param_names(sig);

            if toml_params.is_empty() && input_params.is_empty() {
                // Still walk nested items (inner fns / mods) for further doorways.
                for stmt in stmts {
                    if let syn::Stmt::Item(item) = stmt {
                        self.visit_item(item);
                    }
                }
                return;
            }

            let mut body = FnBodyVisitor {
                outer: self,
                toml_params: &toml_params,
                input_params: &input_params,
            };
            for stmt in stmts {
                body.visit_stmt(stmt);
            }
        }
    }

    struct FnBodyVisitor<'a> {
        outer: &'a mut DoorwayVisitor,
        toml_params: &'a [String],
        input_params: &'a [String],
    }

    impl<'ast> Visit<'ast> for FnBodyVisitor<'_> {
        fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
            if has_cfg_test(&node.attrs) {
                return;
            }
            // Nested modules: re-enter outer visitor so nested fns re-compute params.
            self.outer.visit_item_mod(node);
        }

        fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
            self.outer.visit_item_fn(node);
        }

        fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
            self.outer.visit_impl_item_fn(node);
        }

        fn visit_local(&mut self, node: &'ast syn::Local) {
            if let Some(init) = &node.init {
                let kind = doorway_kind_in_expr(&init.expr, self.toml_params, self.input_params);
                if kind != DoorwayKind::None {
                    match type_from_pat(&node.pat).or_else(|| type_from_doorway_expr(&init.expr, kind)) {
                        Some(name) => self.outer.record_type(name),
                        None => self.outer.record_unresolvable(
                            "found a config doorway call but could not resolve the config type from the binding \
                             or turbofish at this site — annotate the `let` with an explicit type or use turbofish \
                             (e.g. `input.config::<Config>()` / `config.try_into::<Config>()`)"
                                .to_owned(),
                        ),
                    }
                }
            }
            // Nested lets inside the initializer (blocks, match arms) still need
            // scanning; doorway detection only runs on visit_local, so re-walking
            // the init expression does not double-count.
            syn::visit::visit_local(self, node);
        }
    }

    // Second pass for free-standing return expressions that are pure doorway
    // calls (no local). Collected via a dedicated scan of function tails.
    struct TailVisitor<'a> {
        outer: &'a mut DoorwayVisitor,
    }

    impl<'ast> Visit<'ast> for TailVisitor<'_> {
        fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
            if has_cfg_test(&node.attrs) {
                return;
            }
            syn::visit::visit_item_mod(self, node);
        }

        fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
            if has_cfg_test(&node.attrs) {
                return;
            }
            self.scan_fn(&node.sig, &node.block.stmts);
        }

        fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
            if has_cfg_test(&node.attrs) {
                return;
            }
            self.scan_fn(&node.sig, &node.block.stmts);
        }
    }

    impl TailVisitor<'_> {
        fn scan_fn(&mut self, sig: &syn::Signature, stmts: &[syn::Stmt]) {
            let toml_params = toml_value_param_names(sig);
            let input_params = check_input_param_names(sig);
            if toml_params.is_empty() && input_params.is_empty() {
                for stmt in stmts {
                    if let syn::Stmt::Item(item) = stmt {
                        self.visit_item(item);
                    }
                }
                return;
            }
            for stmt in stmts {
                match stmt {
                    syn::Stmt::Expr(expr, _) => {
                        self.consider_tail(expr, &toml_params, &input_params);
                    }
                    syn::Stmt::Item(item) => self.visit_item(item),
                    _ => {}
                }
            }
        }

        fn consider_tail(&mut self, expr: &syn::Expr, toml_params: &[String], input_params: &[String]) {
            // Strip try/await wrappers.
            let mut current = expr;
            loop {
                match current {
                    syn::Expr::Try(inner) => current = &inner.expr,
                    syn::Expr::Await(inner) => current = &inner.base,
                    syn::Expr::Paren(inner) => current = &inner.expr,
                    _ => break,
                }
            }
            let kind = doorway_kind_in_expr(current, toml_params, input_params);
            if kind == DoorwayKind::None {
                return;
            }
            match type_from_doorway_expr(current, kind) {
                Some(name) => self.outer.record_type(name),
                None => self.outer.record_unresolvable(
                    "found a config doorway call in tail position without a resolvable config type — use a typed \
                     `let` binding or turbofish so the config struct can be inspected"
                        .to_owned(),
                ),
            }
        }
    }

    let mut visitor = DoorwayVisitor {
        types: BTreeSet::new(),
        unresolvable: Vec::new(),
    };
    visitor.visit_file(file);

    let mut tail = TailVisitor { outer: &mut visitor };
    tail.visit_file(file);

    DoorwayResolution {
        types: visitor.types.into_iter().collect(),
        unresolvable: visitor.unresolvable,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DoorwayKind {
    None,
    BuiltinTryInto,
    GuestConfig,
}

fn toml_value_param_names(sig: &syn::Signature) -> Vec<String> {
    sig.inputs
        .iter()
        .filter_map(|input| {
            let syn::FnArg::Typed(pat_type) = input else {
                return None;
            };
            if !type_is_toml_value(&pat_type.ty) {
                return None;
            }
            pat_ident_name(&pat_type.pat)
        })
        .collect()
}

fn check_input_param_names(sig: &syn::Signature) -> Vec<String> {
    sig.inputs
        .iter()
        .filter_map(|input| {
            let syn::FnArg::Typed(pat_type) = input else {
                return None;
            };
            if !type_is_check_input(&pat_type.ty) {
                return None;
            }
            pat_ident_name(&pat_type.pat)
        })
        .collect()
}

fn pat_ident_name(pat: &syn::Pat) -> Option<String> {
    match pat {
        syn::Pat::Ident(ident) => Some(ident.ident.to_string()),
        syn::Pat::Reference(reference) => pat_ident_name(&reference.pat),
        _ => None,
    }
}

fn type_is_toml_value(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::Reference(reference) => type_is_toml_value(&reference.elem),
        syn::Type::Path(type_path) => {
            path_ends_with(&type_path.path, &["toml", "Value"]) || path_ends_with(&type_path.path, &["Value"])
        }
        _ => false,
    }
}

fn type_is_check_input(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::Reference(reference) => type_is_check_input(&reference.elem),
        syn::Type::Path(type_path) => {
            path_ends_with(&type_path.path, &["CheckInput"])
                || path_ends_with(&type_path.path, &["checkleft_check_sdk", "CheckInput"])
        }
        _ => false,
    }
}

fn path_ends_with(path: &syn::Path, expected: &[&str]) -> bool {
    let segments: Vec<_> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    if segments.len() < expected.len() {
        return false;
    }
    segments[segments.len() - expected.len()..]
        .iter()
        .zip(expected.iter())
        .all(|(got, want)| got == *want)
}

/// Classify whether `expr`'s tree contains a receiver-anchored config doorway.
fn doorway_kind_in_expr(expr: &syn::Expr, toml_params: &[String], input_params: &[String]) -> DoorwayKind {
    struct Finder<'a> {
        toml_params: &'a [String],
        input_params: &'a [String],
        kind: DoorwayKind,
    }

    impl<'ast> Visit<'ast> for Finder<'_> {
        fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
            if self.kind == DoorwayKind::None {
                if node.method == "try_into" && expr_references_idents(&node.receiver, self.toml_params) {
                    self.kind = DoorwayKind::BuiltinTryInto;
                } else if node.method == "config" && expr_references_idents(&node.receiver, self.input_params) {
                    self.kind = DoorwayKind::GuestConfig;
                }
            }
            syn::visit::visit_expr_method_call(self, node);
        }
    }

    let mut finder = Finder {
        toml_params,
        input_params,
        kind: DoorwayKind::None,
    };
    finder.visit_expr(expr);
    finder.kind
}

fn expr_references_idents(expr: &syn::Expr, names: &[String]) -> bool {
    if names.is_empty() {
        return false;
    }
    struct Finder<'a> {
        names: &'a [String],
        found: bool,
    }
    impl<'ast> Visit<'ast> for Finder<'_> {
        fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
            if !self.found
                && node.path.segments.len() == 1
                && self.names.iter().any(|n| node.path.segments[0].ident == n)
            {
                self.found = true;
            }
            syn::visit::visit_expr_path(self, node);
        }
    }
    let mut finder = Finder { names, found: false };
    finder.visit_expr(expr);
    finder.found
}

fn type_from_pat(pat: &syn::Pat) -> Option<String> {
    match pat {
        syn::Pat::Type(pat_type) => type_ident(&pat_type.ty),
        _ => None,
    }
}

fn type_from_doorway_expr(expr: &syn::Expr, kind: DoorwayKind) -> Option<String> {
    struct Extractor {
        kind: DoorwayKind,
        found: Option<String>,
    }

    impl<'ast> Visit<'ast> for Extractor {
        fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
            if self.found.is_none() {
                let want = match self.kind {
                    DoorwayKind::BuiltinTryInto => Some("try_into"),
                    DoorwayKind::GuestConfig => Some("config"),
                    DoorwayKind::None => None,
                };
                if want.is_some_and(|w| node.method == w)
                    && let Some(args) = &node.turbofish
                {
                    for arg in &args.args {
                        if let syn::GenericArgument::Type(ty) = arg
                            && let Some(name) = type_ident(ty)
                        {
                            self.found = Some(name);
                            break;
                        }
                    }
                }
            }
            syn::visit::visit_expr_method_call(self, node);
        }
    }

    let mut extractor = Extractor { kind, found: None };
    extractor.visit_expr(expr);
    extractor.found
}

fn type_ident(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(type_path) => type_path.path.segments.last().map(|segment| segment.ident.to_string()),
        _ => None,
    }
}

/// True when any `cfg` attribute's token stream contains the `test` token —
/// covers bare `#[cfg(test)]`, `#[cfg(all(test, feature = "…"))]`,
/// `#[cfg(any(test, …))]`, etc.
fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        // Prefer the meta list tokens; fall back to full attribute tokens.
        let tokens = match &attr.meta {
            syn::Meta::List(list) => list.tokens.to_string(),
            _ => return false,
        };
        tokens
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|tok| tok == "test")
    })
}

/// Resolve `type_name` to its struct definition, first within `file` itself,
/// then falling back to the rest of the check's own source.
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
        // Skip out-of-file test modules when resolving across the built-in tree.
        if candidate.to_string_lossy().ends_with("/tests.rs") {
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

fn denylisted_field_findings(
    path: &Path,
    contents: &str,
    item_struct: &syn::ItemStruct,
    denylist: &[&str],
) -> Vec<Finding> {
    let syn::Fields::Named(fields) = &item_struct.fields else {
        return Vec::new();
    };

    let rename_all = container_rename_all(&item_struct.attrs);

    fields
        .named
        .iter()
        .filter_map(|field| {
            let ident = field.ident.as_ref()?;
            let rust_name = ident.to_string();
            let keys = effective_serde_keys(&rust_name, &field.attrs, rename_all.as_deref());
            let denied = keys.iter().find(|key| denylist.contains(&key.as_str()))?;

            Some(Finding {
                fixable: false,
                severity: Severity::Error,
                message: format!(
                    "check config struct `{}` declares a top-level `{denied}` field — deciding \"is this file a \
                     target of this check\" is the framework's job (the check-entry `include` / `exclude` keys), \
                     not the check's own config",
                    item_struct.ident
                ),
                location: Some(Location {
                    path: path.to_path_buf(),
                    line: field_declaration_line(contents, &item_struct.ident.to_string(), &rust_name),
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

/// Effective serde config key names for a field: the raw Rust ident (after
/// optional container `rename_all`), any `rename = "…"`, and every `alias = "…"`.
fn effective_serde_keys(rust_name: &str, field_attrs: &[syn::Attribute], rename_all: Option<&str>) -> Vec<String> {
    let mut keys = Vec::new();
    let (explicit_rename, aliases) = field_serde_rename_and_aliases(field_attrs);

    if let Some(rename) = explicit_rename {
        keys.push(rename);
    } else if let Some(transform) = rename_all {
        keys.push(apply_rename_all(rust_name, transform));
    } else {
        keys.push(rust_name.to_owned());
    }
    keys.extend(aliases);
    keys.sort();
    keys.dedup();
    keys
}

fn container_rename_all(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let Ok(meta) = attr.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
        else {
            continue;
        };
        for item in meta {
            if let syn::Meta::NameValue(nv) = item
                && nv.path.is_ident("rename_all")
                && let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s), ..
                }) = nv.value
            {
                return Some(s.value());
            }
        }
    }
    None
}

fn field_serde_rename_and_aliases(attrs: &[syn::Attribute]) -> (Option<String>, Vec<String>) {
    let mut rename = None;
    let mut aliases = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let Ok(meta) = attr.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
        else {
            continue;
        };
        for item in meta {
            match item {
                syn::Meta::NameValue(nv) if nv.path.is_ident("rename") => {
                    if let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s), ..
                    }) = nv.value
                    {
                        rename = Some(s.value());
                    }
                }
                syn::Meta::NameValue(nv) if nv.path.is_ident("alias") => {
                    if let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s), ..
                    }) = nv.value
                    {
                        aliases.push(s.value());
                    }
                }
                syn::Meta::List(list) if list.path.is_ident("alias") => {
                    // #[serde(alias("a", "b"))] is not the common form; ignore.
                }
                _ => {}
            }
        }
    }
    (rename, aliases)
}

fn apply_rename_all(ident: &str, transform: &str) -> String {
    match transform {
        "lowercase" => ident.to_ascii_lowercase(),
        "UPPERCASE" => ident.to_ascii_uppercase(),
        "snake_case" => to_snake_case(ident),
        "SCREAMING_SNAKE_CASE" => to_snake_case(ident).to_ascii_uppercase(),
        "kebab-case" => to_snake_case(ident).replace('_', "-"),
        "camelCase" => to_camel_case(ident, false),
        "PascalCase" => to_camel_case(ident, true),
        // Unknown / unsupported transforms: fall back to the raw ident so we
        // still catch a denylisted Rust name even if the rename form is exotic.
        _ => ident.to_owned(),
    }
}

fn to_snake_case(ident: &str) -> String {
    if ident.contains('_') {
        return ident.to_owned();
    }
    let mut out = String::new();
    for (i, ch) in ident.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn to_camel_case(ident: &str, pascal: bool) -> String {
    let mut out = String::new();
    let mut capitalize_next = pascal;
    let mut first = true;
    for ch in ident.chars() {
        if ch == '_' {
            capitalize_next = true;
            continue;
        }
        if capitalize_next {
            out.extend(ch.to_uppercase());
            capitalize_next = false;
        } else if first && !pascal {
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
        first = false;
    }
    out
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
            "Fix this check source so its config-deserialization doorway is statically resolvable (a typed `let \
             cfg: Config = …` or turbofish `input.config::<Config>()` / `config.try_into::<Config>()`, deserializing \
             a struct defined somewhere in this check's own source)."
                .to_owned(),
        ],
        suggested_fix: None,
    }
}

/// Locate the 1-based line of `field_name` inside the text span of
/// `struct struct_name { … }`. Searching only within that brace range avoids
/// pointing at a nested struct's same-named field (or a doc/string example)
/// that appears earlier in the file.
fn field_declaration_line(source: &str, struct_name: &str, field_name: &str) -> Option<u32> {
    let (start_line, end_line) = struct_body_line_range(source, struct_name)?;
    for (index, line) in source.lines().enumerate() {
        let line_no = index + 1;
        if line_no < start_line || line_no > end_line {
            continue;
        }
        let trimmed = strip_pub_prefix(line.trim_start());
        // Skip attribute / comment lines.
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        let Some(after_name) = trimmed.strip_prefix(field_name) else {
            continue;
        };
        if after_name.trim_start().starts_with(':') {
            return Some(line_no as u32);
        }
    }
    None
}

/// Return 1-based inclusive line range covering `struct Name { … }` body.
fn struct_body_line_range(source: &str, struct_name: &str) -> Option<(usize, usize)> {
    let mut in_block_comment = false;
    let mut struct_start_line = None;
    let mut brace_depth: i32 = 0;
    let mut seen_struct_brace = false;

    for (index, line) in source.lines().enumerate() {
        let line_no = index + 1;
        let mut i = 0;
        let bytes = line.as_bytes();
        while i < bytes.len() {
            if in_block_comment {
                if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    in_block_comment = false;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                break;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                in_block_comment = true;
                i += 2;
                continue;
            }
            // Detect `struct <Name>` only before we've entered its body.
            if struct_start_line.is_none() {
                // Cheap line-level check first.
                let trimmed = line.trim_start();
                if let Some(after) = trimmed
                    .strip_prefix("struct")
                    .or_else(|| trimmed.strip_prefix("pub struct"))
                    .or_else(|| {
                        // pub(crate) struct / pub(super) struct
                        let rest = trimmed.strip_prefix("pub")?;
                        let rest = rest.trim_start();
                        if let Some(rest) = rest.strip_prefix('(') {
                            let close = rest.find(')')?;
                            let rest = rest[close + 1..].trim_start();
                            rest.strip_prefix("struct")
                        } else {
                            None
                        }
                    })
                {
                    let after = after.trim_start();
                    if after.starts_with(struct_name)
                        && after.get(struct_name.len()..).is_some_and(|rest| {
                            rest.is_empty() || !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
                        })
                    {
                        struct_start_line = Some(line_no);
                    }
                }
            }
            if let Some(start_line) = struct_start_line {
                match bytes[i] {
                    b'{' => {
                        brace_depth += 1;
                        seen_struct_brace = true;
                    }
                    b'}' => {
                        brace_depth -= 1;
                        if seen_struct_brace && brace_depth == 0 {
                            return Some((start_line, line_no));
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
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

    async fn run_check_bytes(path: &str, contents: &[u8]) -> Vec<crate::output::Finding> {
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

    /// Guest surface: `#[serde(rename = "paths")]` on a differently-named field
    /// still ships the mono#2554 config key and must be flagged.
    #[tokio::test]
    async fn flags_serde_rename_to_a_denylisted_config_key() {
        let source = r#"
use checkleft_check_sdk::{CheckInput, Finding, check};
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default, rename = "paths")]
    my_files: Vec<String>,
}

#[check(id = "example/generic", description = "an example generic check")]
fn run(input: &CheckInput) -> Vec<Finding> {
    let cfg: Config = input.config().unwrap_or_default();
    let _ = cfg.my_files;
    Vec::new()
}
"#;
        let findings = run_check("tools/checkleft/checks/example/generic/src/lib.rs", source).await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("`paths`"), "{findings:?}");
    }

    /// Built-in surface rule: framework-key pass-through via rename (the
    /// DocStructureConfig shape) is legal by definition — not a silent
    /// rename-bypass and not a per-check exemption.
    #[tokio::test]
    async fn allows_builtin_framework_key_passthrough_via_serde_rename() {
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
        let _ = parsed;
        todo!()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExampleConfig {
    #[serde(default, rename = "include")]
    _framework_include: Option<toml::Value>,
    #[serde(default, rename = "applies_to")]
    _framework_applies_to: Option<toml::Value>,
    #[serde(default)]
    max_lines: Option<u64>,
}
"#;
        let findings = run_check("tools/checkleft/src/checks/example.rs", source).await;
        assert!(findings.is_empty(), "{findings:?}");
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

    /// Turbofish doorway without a typed `let` must still resolve Config and
    /// flag its denylisted field.
    #[tokio::test]
    async fn flags_paths_via_turbofish_config_doorway() {
        let source = r#"
use checkleft_check_sdk::{CheckInput, Finding, check};
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    paths: Vec<String>,
}

#[check(id = "example/generic", description = "an example generic check")]
fn run(input: &CheckInput) -> Vec<Finding> {
    let cfg = input.config::<Config>().unwrap_or_default();
    let _ = cfg.paths;
    Vec::new()
}
"#;
        let findings = run_check("tools/checkleft/checks/example/generic/src/lib.rs", source).await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("`paths`"), "{findings:?}");
    }

    /// Un-annotated doorway with no resolvable type must fail loudly.
    #[tokio::test]
    async fn fails_loudly_on_unannotated_unresolvable_doorway() {
        let source = r#"
use checkleft_check_sdk::{CheckInput, Finding, check};

#[check(id = "example/generic", description = "an example generic check")]
fn run(input: &CheckInput) -> Vec<Finding> {
    let cfg = input.config().unwrap_or_default();
    let _ = cfg;
    Vec::new()
}
"#;
        let findings = run_check("tools/checkleft/checks/example/generic/src/lib.rs", source).await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, crate::output::Severity::Error);
        assert!(
            findings[0].message.contains("could not resolve") || findings[0].message.contains("could not verify"),
            "{findings:?}"
        );
    }

    /// Benign numeric `try_into` next to a real config doorway must not be
    /// registered as a doorway type; only the config struct's denylisted
    /// field produces a finding.
    #[tokio::test]
    async fn ignores_benign_numeric_try_into_alongside_config_doorway() {
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
        let x: u64 = 42;
        let n: u32 = x.try_into().unwrap_or(0);
        let _ = n;
        let parsed: ExampleConfig = config.clone().try_into().context("invalid example config")?;
        let _ = parsed.paths;
        todo!()
    }
}

#[derive(Debug, Deserialize)]
struct ExampleConfig {
    #[serde(default)]
    paths: Vec<String>,
}
"#;
        let findings = run_check("tools/checkleft/src/checks/example.rs", source).await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("`paths`"), "{findings:?}");
        assert!(
            !findings
                .iter()
                .any(|f| f.message.contains("`u32`") || f.message.contains("u32")),
            "{findings:?}"
        );
    }

    /// Nested `RuleConfig { include }` declared before the violating top-level
    /// `Config { include }` must report the top-level field's line, not the
    /// nested one's.
    #[tokio::test]
    async fn reports_line_of_top_level_field_not_earlier_nested_namesake() {
        let source = r#"
use checkleft_check_sdk::{CheckInput, Finding, check};
use serde::Deserialize;

#[derive(Deserialize)]
struct RuleConfig {
    #[serde(default)]
    include: Vec<String>,
    message: String,
}

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    rules: Vec<RuleConfig>,
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
        // Nested include is on line 7 of the source string (1-based within file);
        // top-level include is later. Assert we did not pick the first occurrence.
        let line = findings[0].location.as_ref().and_then(|l| l.line).expect("line");
        // Count: blank, uses, blank, derive, struct RuleConfig, attr, include, ...
        // Top-level Config's include comes after RuleConfig's closing brace.
        assert!(
            line > 7,
            "expected top-level include line > nested line 7, got {line}; {findings:?}"
        );
        // Top-level Config.include is line 15 in this fixture (nested is line 8).
        assert_eq!(line, 15, "{findings:?}");
    }

    #[tokio::test]
    async fn ignores_a_config_doorway_that_only_exists_inside_cfg_test() {
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
    async fn ignores_cfg_all_test_gated_modules() {
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

#[cfg(all(test, feature = "extra"))]
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
    async fn fails_loudly_on_non_utf8_check_source() {
        let findings = run_check_bytes("tools/checkleft/src/checks/example.rs", &[0xff, 0xfe, 0xfd]).await;

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, crate::output::Severity::Error);
        assert!(
            findings[0].message.contains("UTF-8") || findings[0].message.contains("utf-8"),
            "{findings:?}"
        );
        assert!(
            findings[0].message.contains("example.rs"),
            "finding must name the file: {findings:?}"
        );
    }

    #[tokio::test]
    async fn fails_loudly_when_the_doorway_type_cannot_be_resolved() {
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
