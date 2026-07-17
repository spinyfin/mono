//! Selector and template unit tests, plus the jaq dependency smoke test. These
//! exercise the two rendering primitives the declarative transforms are built
//! on: the jaq-backed `Selector` (file filtering, nested flattening) and the
//! `Template` (item/context ref rendering, batch-mode input errors).

use serde_json::Value;

use super::selector::Selector;
use super::template::{RenderContext, Template};
use super::tests_common::{REAL_FORMAT_CLEAN, REAL_FORMAT_UNFORMATTED, REAL_LINT_WARNINGS};

// ── selector unit tests ────────────────────────────────────────────────────────

#[test]
fn selector_filters_files_by_formatted_flag() {
    let selector = Selector::parse(".files[] | select(.formatted == false)").unwrap();
    let root: Value = serde_json::from_slice(REAL_FORMAT_UNFORMATTED).unwrap();
    let rows = selector.select(&root).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("filename").unwrap(), "a/b/unformatted.bzl");

    let clean: Value = serde_json::from_slice(REAL_FORMAT_CLEAN).unwrap();
    assert!(selector.select(&clean).unwrap().is_empty());
}

#[test]
fn selector_flattens_nested_warnings() {
    let selector = Selector::parse(".files[].warnings[]").unwrap();
    let root: Value = serde_json::from_slice(REAL_LINT_WARNINGS).unwrap();
    let rows = selector.select(&root).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get("category").unwrap(), "module-docstring");
}

// ── template unit tests ────────────────────────────────────────────────────────

#[test]
fn template_renders_item_and_context_refs() {
    let item: Value = serde_json::json!({"start": {"line": 11}, "category": "no-effect"});
    let context = RenderContext {
        input_file: Some("x/y.bzl"),
        exit_code: Some(0),
        needs_invocations: None,
    };

    assert_eq!(
        Template::parse("{{item.start.line}}")
            .unwrap()
            .render(&item, context)
            .unwrap(),
        "11"
    );
    assert_eq!(
        Template::parse("{{input.file}}")
            .unwrap()
            .render(&item, context)
            .unwrap(),
        "x/y.bzl"
    );
    assert_eq!(
        Template::parse("{{item.category}}: hi")
            .unwrap()
            .render(&item, context)
            .unwrap(),
        "no-effect: hi"
    );
}

#[test]
fn template_input_file_unavailable_in_batch_errors() {
    let item: Value = serde_json::json!({});
    let context = RenderContext {
        input_file: None,
        exit_code: Some(0),
        needs_invocations: None,
    };
    let err = Template::parse("{{input.file}}")
        .unwrap()
        .render(&item, context)
        .unwrap_err();
    assert!(format!("{err:#}").contains("per_file mode"));
}

// ── jaq smoke test ─────────────────────────────────────────────────────────────

/// `empty` is not in jaq_core::core() in 1.x; register it as a native.
fn jaq_empty_run<'a>(
    _: jaq_interpret::Args<'a>,
    _: (jaq_interpret::Ctx<'a>, jaq_interpret::Val),
) -> jaq_interpret::ValRs<'a> {
    Box::new(core::iter::empty::<jaq_interpret::ValR>())
}

/// Prove that jaq-interpret parses and evaluates a filter with no C deps.
#[test]
fn jaq_deps_compile_and_evaluate() {
    use jaq_interpret::{Ctx, FilterT as _, Native, ParseCtx, RcIter, Val};
    use serde_json::json;

    let (stdlib_defs, errs) = jaq_parse::parse("def select(f): if f then . else empty end;", jaq_parse::defs());
    assert!(errs.is_empty(), "stdlib parse errors: {errs:?}");

    let (f, errs) = jaq_parse::parse(".a | select(.b == 1)", jaq_parse::main());
    assert!(errs.is_empty(), "parse errors: {errs:?}");

    let mut ctx = ParseCtx::new(Vec::new());
    ctx.insert_natives(jaq_core::core());
    ctx.insert_native("empty".to_string(), 0, Native::new(jaq_empty_run));
    ctx.insert_defs(stdlib_defs.unwrap_or_default());
    let filter = ctx.compile(f.unwrap());
    assert!(ctx.errs.is_empty(), "compile errors: {} error(s)", ctx.errs.len());

    let inputs = RcIter::new(core::iter::empty());
    let ctx = Ctx::new([], &inputs);
    let input = Val::from(json!({"a": {"b": 1}}));

    let output: Vec<serde_json::Value> = filter
        .run((ctx, input))
        .map(|r| serde_json::Value::from(r.unwrap()))
        .collect();

    assert_eq!(output, vec![json!({"b": 1})]);
}
