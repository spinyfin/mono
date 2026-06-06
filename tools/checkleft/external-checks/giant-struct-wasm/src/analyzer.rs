//! Giant-struct rule logic — a faithful **copy** of the syn-based analysis from
//! `tools/checkleft/src/checks/rust_giant_struct_common.rs`.
//!
//! PROTOTYPE NOTE: this is copied rather than shared because this guest compiles
//! to `wasm32-unknown-unknown` and cannot depend on the `checkleft` crate (which
//! pulls in wasmtime/tokio, neither of which targets wasm). It is copied verbatim
//! from the built-in so parity is exercised directly: the host-side tests below
//! feed the SAME golden sources the built-in's own tests use and assert the SAME
//! verdicts. A production version would factor this into a lean, dependency-light
//! crate that BOTH the built-in check and the wasm guest depend on, so parity is
//! guaranteed by construction. See PROTOTYPE-NOTES.md.
//!
//! Only the parts of `rust_giant_struct_common.rs` that depend purely on `syn`
//! (plus `std`) are copied: the struct-counting, clap-exemption, builder-detection,
//! and declaration-line logic. The `globset`/`anyhow`-based exclude-file machinery
//! and the stale-exclusion auditing are NOT ported — they need a source-tree walk
//! and `config_dir` scope the guest does not have. See PROTOTYPE-NOTES.md (d).

use std::collections::HashSet;

pub const DEFAULT_MAX_FIELDS: usize = 5;

#[derive(Clone, Debug)]
pub enum BuilderKind {
    Bon,
    DeriveBuilder,
}

impl BuilderKind {
    pub fn derive_display(&self) -> &str {
        match self {
            Self::Bon => "bon::Builder",
            Self::DeriveBuilder => "derive_builder::Builder",
        }
    }

    pub fn crate_name(&self) -> &str {
        match self {
            Self::Bon => "bon",
            Self::DeriveBuilder => "derive_builder",
        }
    }
}

/// Whether a giant struct has a builder derive or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GiantStructKind {
    WithBuilder,
    WithoutBuilder,
}

/// A giant struct found in a source file.
pub struct GiantStructInfo {
    pub name: String,
    pub kind: GiantStructKind,
}

pub fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Ident>()
                .ok()
                .map_or(false, |id| id == "test")
    })
}

/// Returns true if the struct carries a clap argument-parser derive.
/// Such structs are exempt because clap owns their construction via its derive.
pub fn has_clap_derive(attrs: &[syn::Attribute]) -> bool {
    const CLAP_TRAITS: &[&str] = &["Parser", "Args", "Subcommand"];
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let Ok(nested) = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
        ) else {
            continue;
        };
        for path in &nested {
            let segs: Vec<_> = path.segments.iter().collect();
            match segs.as_slice() {
                [seg] if CLAP_TRAITS.contains(&seg.ident.to_string().as_str()) => return true,
                [krate, trait_seg]
                    if krate.ident == "clap"
                        && CLAP_TRAITS.contains(&trait_seg.ident.to_string().as_str()) =>
                {
                    return true
                }
                _ => {}
            }
        }
    }
    false
}

pub fn has_required_builder(attrs: &[syn::Attribute], builder: &BuilderKind) -> bool {
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let Ok(nested) = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
        ) else {
            continue;
        };
        for path in &nested {
            let segs: Vec<_> = path.segments.iter().collect();
            match builder {
                BuilderKind::Bon => {
                    if segs.len() == 2 && segs[0].ident == "bon" && segs[1].ident == "Builder" {
                        return true;
                    }
                }
                BuilderKind::DeriveBuilder => {
                    if segs.len() == 2
                        && segs[0].ident == "derive_builder"
                        && segs[1].ident == "Builder"
                    {
                        return true;
                    }
                    // Unqualified Builder is also accepted for derive_builder
                    if segs.len() == 1 && segs[0].ident == "Builder" {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Recursively scan `items` for giant structs, returning info about each one found.
/// Skips: test-cfg structs/modules, clap-derived structs, tuple/unit structs.
/// Does NOT apply `exclude_structs`; the caller filters based on context.
pub fn collect_giant_struct_infos(
    items: &[syn::Item],
    in_test_mod: bool,
    builder: &BuilderKind,
    max_fields: usize,
) -> Vec<GiantStructInfo> {
    let mut infos = Vec::new();
    for item in items {
        match item {
            syn::Item::Struct(s) => {
                if in_test_mod || has_cfg_test(&s.attrs) {
                    continue;
                }
                let syn::Fields::Named(named) = &s.fields else {
                    continue;
                };
                if named.named.len() <= max_fields {
                    continue;
                }
                if has_clap_derive(&s.attrs) {
                    continue;
                }
                let kind = if has_required_builder(&s.attrs, builder) {
                    GiantStructKind::WithBuilder
                } else {
                    GiantStructKind::WithoutBuilder
                };
                infos.push(GiantStructInfo {
                    name: s.ident.to_string(),
                    kind,
                });
            }
            syn::Item::Mod(m) => {
                let is_test = has_cfg_test(&m.attrs);
                if let Some((_, sub_items)) = &m.content {
                    infos.extend(collect_giant_struct_infos(
                        sub_items,
                        in_test_mod || is_test,
                        builder,
                        max_fields,
                    ));
                }
            }
            _ => {}
        }
    }
    infos
}

/// Recursively walk `items` collecting the names of giant structs that VIOLATE the rule
/// (giant + no required builder + not in `exclude_structs`).
/// This is a thin filter over [`collect_giant_struct_infos`].
pub fn collect_violations(
    items: &[syn::Item],
    in_test_mod: bool,
    builder: &BuilderKind,
    max_fields: usize,
    exclude_structs: &HashSet<String>,
) -> Vec<String> {
    collect_giant_struct_infos(items, in_test_mod, builder, max_fields)
        .into_iter()
        .filter(|info| {
            !matches!(info.kind, GiantStructKind::WithBuilder)
                && !exclude_structs.contains(&info.name)
        })
        .map(|info| info.name)
        .collect()
}

/// Find the 1-based line number where `struct <name>` is declared, or `None`.
/// Handles a leading visibility modifier (`pub`, `pub(crate)`, `pub(super)`, `pub(in path)`).
pub fn struct_declaration_line(source: &str, struct_name: &str) -> Option<u32> {
    let search = format!("struct {struct_name}");
    for (i, line) in source.lines().enumerate() {
        let candidate = strip_visibility(line.trim_start());
        if let Some(after) = candidate.strip_prefix(&search)
            && (after.is_empty()
                || matches!(after.chars().next(), Some(' ' | '\t' | '<' | '{' | '(')))
        {
            return Some((i + 1) as u32);
        }
    }
    None
}

/// Like [`struct_declaration_line`] but falls back to line 1 when the declaration can't
/// be located, for use as a finding location.
pub fn find_struct_line(source: &str, struct_name: &str) -> u32 {
    struct_declaration_line(source, struct_name).unwrap_or(1)
}

/// Strip a leading `pub` / `pub(...)` visibility modifier (and following whitespace) from
/// an already-`trim_start`ed line. Leaves the line untouched when there is no visibility
/// keyword (so `published` is not mistaken for `pub`).
pub fn strip_visibility(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("pub") else {
        return line;
    };
    match rest.chars().next() {
        Some('(') => match rest.find(')') {
            Some(close) => rest[close + 1..].trim_start(),
            None => line,
        },
        Some(c) if c.is_whitespace() => rest.trim_start(),
        _ => line,
    }
}

// ── parity tests (host target; `cargo test`) ──────────────────────────────────
//
// These mirror the built-in check's own tests in
// `tools/checkleft/src/checks/rust_giant_structs_use_builder.rs`. Same golden
// sources in, same verdicts out → analysis parity with the built-in, proven by
// running this copied logic on the host.
#[cfg(test)]
mod tests {
    use super::*;

    fn violations(source: &str, builder: BuilderKind, max_fields: usize) -> Vec<String> {
        let parsed = syn::parse_file(source).expect("parse");
        collect_violations(
            &parsed.items,
            false,
            &builder,
            max_fields,
            &HashSet::new(),
        )
    }

    #[test]
    fn flags_six_field_struct_without_builder() {
        let source = r#"
pub struct Big { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        let v = violations(source, BuilderKind::Bon, DEFAULT_MAX_FIELDS);
        assert_eq!(v, vec!["Big".to_owned()]);
    }

    #[test]
    fn allows_six_field_struct_with_bon_builder() {
        let source = r#"
#[derive(bon::Builder)]
pub struct Big { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        assert!(violations(source, BuilderKind::Bon, DEFAULT_MAX_FIELDS).is_empty());
    }

    #[test]
    fn allows_five_field_struct_without_builder() {
        let source = r#"
pub struct Small { a: String, b: String, c: String, d: String, e: String }
"#;
        assert!(violations(source, BuilderKind::Bon, DEFAULT_MAX_FIELDS).is_empty());
    }

    #[test]
    fn ignores_tuple_struct() {
        let source = "pub struct Tuple(String, String, String, String, String, String);\n";
        assert!(violations(source, BuilderKind::Bon, DEFAULT_MAX_FIELDS).is_empty());
    }

    #[test]
    fn ignores_struct_in_cfg_test_mod() {
        let source = r#"
#[cfg(test)]
mod tests {
    pub struct TestHelper { a: String, b: String, c: String, d: String, e: String, f: String }
}
"#;
        assert!(violations(source, BuilderKind::Bon, DEFAULT_MAX_FIELDS).is_empty());
    }

    #[test]
    fn ignores_struct_with_cfg_test_attr() {
        let source = r#"
#[cfg(test)]
pub struct TestHelper { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        assert!(violations(source, BuilderKind::Bon, DEFAULT_MAX_FIELDS).is_empty());
    }

    #[test]
    fn flags_with_derive_builder_param() {
        let source = r#"
pub struct Big { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        let v = violations(source, BuilderKind::DeriveBuilder, DEFAULT_MAX_FIELDS);
        assert_eq!(v, vec!["Big".to_owned()]);
    }

    #[test]
    fn allows_six_field_struct_with_derive_builder() {
        let source = r#"
#[derive(derive_builder::Builder)]
pub struct Big { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        assert!(violations(source, BuilderKind::DeriveBuilder, DEFAULT_MAX_FIELDS).is_empty());
    }

    #[test]
    fn respects_custom_max_fields() {
        let source = r#"
pub struct Medium { a: String, b: String, c: String }
"#;
        assert_eq!(violations(source, BuilderKind::Bon, 2), vec!["Medium".to_owned()]);
    }

    #[test]
    fn clap_parser_struct_is_exempt() {
        let source = r#"
#[derive(Debug, clap::Parser)]
pub struct Cli { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        assert!(violations(source, BuilderKind::Bon, DEFAULT_MAX_FIELDS).is_empty());
    }

    #[test]
    fn clap_args_struct_is_exempt() {
        let source = r#"
#[derive(Debug, Clone, Args)]
pub struct TaskArgs { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        assert!(violations(source, BuilderKind::Bon, DEFAULT_MAX_FIELDS).is_empty());
    }

    #[test]
    fn clap_subcommand_struct_is_exempt() {
        let source = r#"
#[derive(Debug, clap::Subcommand)]
pub struct Commands { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        assert!(violations(source, BuilderKind::Bon, DEFAULT_MAX_FIELDS).is_empty());
    }

    #[test]
    fn non_clap_giant_struct_is_still_flagged() {
        let source = r#"
#[derive(Debug, Clone)]
pub struct PlainBig { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        assert_eq!(
            violations(source, BuilderKind::Bon, DEFAULT_MAX_FIELDS),
            vec!["PlainBig".to_owned()]
        );
    }

    #[test]
    fn exclude_structs_exempts_named_struct() {
        let source = r#"
pub struct Grandfathered { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        let parsed = syn::parse_file(source).expect("parse");
        let mut exclude = HashSet::new();
        exclude.insert("Grandfathered".to_owned());
        let v = collect_violations(
            &parsed.items,
            false,
            &BuilderKind::Bon,
            DEFAULT_MAX_FIELDS,
            &exclude,
        );
        assert!(v.is_empty());
    }

    #[test]
    fn flags_new_giant_struct_even_when_sibling_is_grandfathered() {
        let source = r#"
pub struct Grandfathered { a: String, b: String, c: String, d: String, e: String, f: String }
pub struct FreshlyAdded { a: String, b: String, c: String, d: String, e: String, f: String }
"#;
        let parsed = syn::parse_file(source).expect("parse");
        let mut exclude = HashSet::new();
        exclude.insert("Grandfathered".to_owned());
        let v = collect_violations(
            &parsed.items,
            false,
            &BuilderKind::Bon,
            DEFAULT_MAX_FIELDS,
            &exclude,
        );
        assert_eq!(v, vec!["FreshlyAdded".to_owned()]);
    }

    #[test]
    fn reports_declaration_line_for_pub_struct() {
        let source = "// line 1\n// line 2\n#[derive(Debug)]\npub struct Big {\n a: u8,\n}\n";
        assert_eq!(struct_declaration_line(source, "Big"), Some(4));
    }

    #[test]
    fn reports_declaration_line_for_restricted_visibility() {
        let source = "\npub(crate) struct Big {\n a: u8,\n}\n";
        assert_eq!(struct_declaration_line(source, "Big"), Some(2));
    }

    #[test]
    fn missing_declaration_falls_back_to_line_one() {
        assert_eq!(find_struct_line("// nothing here\n", "Ghost"), 1);
    }
}
