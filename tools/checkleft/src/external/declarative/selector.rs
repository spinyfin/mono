//! A jaq-backed JSON selector: evaluates any jq/jaq filter against a JSON
//! value, returning the matching items as the set of "rows" each finding is
//! projected from.
//!
//! The filter string is validated at parse time (syntax errors are caught
//! early) and compiled at evaluation time. Both of buildifier's filters work
//! without modification:
//!
//! - `.files[] | select(.formatted == false)` (format pass)
//! - `.files[].warnings[]` (lint pass)
//!
//! Richer jq expressions — variable binding, arithmetic, `|=`, arbitrary
//! function calls — are supported by jaq and do not need a separate wasm tier
//! unless they require side-effectful computation. That seam now lives at the
//! boundary of jaq's own feature set rather than at hand-rolled parsing limits.
//!
//! ## jaq prelude
//!
//! jaq 1.x parses `false`, `true`, and `null` as zero-arity filter calls
//! rather than literals (the token grammar does not have dedicated keyword
//! variants for them). `jaq_core::core()` does not define these filters
//! either. The prelude below registers the minimum set needed to support the
//! kinds of filters declarative checks are expected to use.

use anyhow::{Result, bail};
use jaq_interpret::{Ctx, FilterT as _, Native, ParseCtx, RcIter, Val, ValR, ValRs};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    filter: String,
}

impl Selector {
    /// Parse and syntax-validate a jaq filter string. Returns an error on
    /// any jq syntax problem so bad manifests are rejected at load time, not
    /// at the moment a tool runs and produces output.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim().to_owned();
        if raw.is_empty() {
            bail!("selector filter must not be empty");
        }
        // Syntax-check at construction time so manifest load fails fast.
        let (_f, errs) = jaq_parse::parse(&raw, jaq_parse::main());
        if !errs.is_empty() {
            bail!("selector `{raw}` has jaq parse errors: {errs:?}");
        }
        Ok(Self { filter: raw })
    }

    /// Evaluate the filter against `root`, returning each output value as a
    /// separate row. Evaluation errors (e.g. type mismatches inside the
    /// filter) are surfaced as `Err`.
    pub fn select(&self, root: &Value) -> Result<Vec<Value>> {
        // jaq 1.x parses `false`, `true`, `null` as Call("false"/"true"/"null",
        // []) — not as literals. They are not provided by jaq_core::core().
        // Register them as natives, plus `empty` (also not in core) so that
        // `select(f)` from the stdlib can be defined.
        //
        // Order matters: natives must be registered before insert_defs so that
        // the stdlib def that uses `empty` compiles correctly.
        let (stdlib_defs, errs) = jaq_parse::parse(
            "def select(f): if f then . else empty end; \
             def not: if . then false else true end;",
            jaq_parse::defs(),
        );
        if !errs.is_empty() {
            bail!("jaq prelude parse errors: {errs:?}");
        }

        let (f, errs) = jaq_parse::parse(&self.filter, jaq_parse::main());
        if !errs.is_empty() {
            bail!("selector `{}` jaq parse errors: {errs:?}", self.filter);
        }
        let Some(f) = f else {
            bail!("selector `{}` produced no filter", self.filter);
        };

        let mut ctx = ParseCtx::new(Vec::new());
        ctx.insert_natives(jaq_core::core());
        ctx.insert_native("empty".to_string(), 0, Native::new(jaq_empty));
        ctx.insert_native("false".to_string(), 0, Native::new(jaq_false));
        ctx.insert_native("true".to_string(), 0, Native::new(jaq_true));
        ctx.insert_native("null".to_string(), 0, Native::new(jaq_null));
        ctx.insert_defs(stdlib_defs.unwrap_or_default());
        let filter = ctx.compile(f);
        if !ctx.errs.is_empty() {
            bail!(
                "selector `{}` jaq compile errors: {} error(s)",
                self.filter,
                ctx.errs.len()
            );
        }

        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);
        let input = Val::from(root.clone());

        let mut rows = Vec::new();
        for result in filter.run((ctx, input)) {
            match result {
                Ok(val) => rows.push(Value::from(val)),
                Err(e) => bail!("selector `{}` evaluation error: {e}", self.filter),
            }
        }
        Ok(rows)
    }
}

fn jaq_empty<'a>(_: jaq_interpret::Args<'a>, _: (Ctx<'a>, Val)) -> ValRs<'a> {
    Box::new(core::iter::empty::<ValR>())
}

fn jaq_false<'a>(_: jaq_interpret::Args<'a>, _: (Ctx<'a>, Val)) -> ValRs<'a> {
    Box::new(core::iter::once(Ok(Val::Bool(false))))
}

fn jaq_true<'a>(_: jaq_interpret::Args<'a>, _: (Ctx<'a>, Val)) -> ValRs<'a> {
    Box::new(core::iter::once(Ok(Val::Bool(true))))
}

fn jaq_null<'a>(_: jaq_interpret::Args<'a>, _: (Ctx<'a>, Val)) -> ValRs<'a> {
    Box::new(core::iter::once(Ok(Val::Null)))
}
