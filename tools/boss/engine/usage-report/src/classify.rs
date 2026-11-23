//! Per-run classification: measurement state, input convention, spend class,
//! caveats, dimension values and the USD outcome.
//!
//! Everything here is a pure function of one [`UsageRunRecord`] (and its
//! task), so each rule has a one-case unit test and the tree builder never has
//! to re-derive one.

use boss_protocol::{
    DRIVER_SLUG_AGY, DRIVER_SLUG_CLAUDE, DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK, ExecutionKind, InputConvention,
    RunCoverage, SpendClass, TokenClasses, UsageCaveat, UsageDimension,
};

pub(crate) use crate::fold::UsdOutcome;
use crate::fold::{RunSample, RunTokens};
use crate::record::{UsageRunRecord, UsageTaskRecord};

/// Caveat codes, closed for this engine build. A client labels the ones it
/// knows and shows an unknown code verbatim, so adding a code is a one-line
/// change for the client rather than a layout change.
pub mod caveat {
    /// The run was created before per-run token capture existed. Its token
    /// columns are NULL by construction, not by data loss.
    pub const PRE_CAPTURE: &str = "pre_capture";
    /// The execution recorded no driver, so its input convention is unknown.
    pub const DRIVER_NOT_RECORDED: &str = "driver_not_recorded";
    /// The driver reports `input` inclusive of cache reads, so a total that
    /// adds the cache-read class counts those tokens twice and is not
    /// comparable with a net-input driver's.
    pub const GROSS_INPUT: &str = "gross_input";
    /// Claude subagent transcripts are not tailed, so a Claude run's figure
    /// omits their tokens.
    pub const SUBAGENTS_EXCLUDED: &str = "subagents_excluded";
    /// The run's model is the literal `<synthetic>` marker some rows carry.
    pub const SYNTHETIC_MODEL: &str = "synthetic_model";
    /// No model was observed; the execution's launched alias stands in.
    pub const MODEL_FROM_LAUNCH_TUPLE: &str = "model_from_launch_tuple";
    /// The provider reported usage for some token classes but not others.
    /// The known classes are a floor.
    pub const TOKENS_PARTIAL: &str = "tokens_partial";
    /// `work_runs.usage_unavailable` is set; the detail is its reason verbatim.
    pub const USAGE_UNAVAILABLE: &str = "usage_unavailable";
    /// The run's model has no complete price in the rate table.
    pub const UNPRICED_MODEL: &str = "unpriced_model";
    /// Attached to every run that carries a USD figure: it is an estimate
    /// against an uncited rate table, not billing truth.
    pub const ESTIMATE_ONLY: &str = "estimate_only";
}

/// Why a run has no USD estimate. Closed for this engine build.
pub mod cause {
    /// No token class was reported, so there is nothing to price.
    pub const UNMEASURED: &str = "unmeasured";
    /// A subscription-billed driver (Grok, Antigravity): API list prices do
    /// not apply even where a model name matches the rate table.
    pub const SUBSCRIPTION_UNPRICED: &str = "subscription_unpriced";
    /// No driver recorded, so the input convention is unknown.
    pub const DRIVER_UNKNOWN: &str = "driver_unknown";
    /// Gross `input` already contains the cache reads, so the flat pricing
    /// formula would charge them twice.
    pub const CAPTURE_UNNORMALISED: &str = "capture_unnormalised";
    /// At least one token class is unknown, so a complete price cannot be made.
    pub const TOKENS_INCOMPLETE: &str = "tokens_incomplete";
    /// The observed model is absent or has no complete price in the table.
    pub const UNKNOWN_MODEL: &str = "unknown_model";
}

/// Value used for "no value" in every dimension, so filters can name it.
pub const NONE_VALUE: &str = "(none)";

const SYNTHETIC_MODEL: &str = "<synthetic>";

/// The token classes a USD estimate is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricedTokens {
    pub input: i64,
    pub output: i64,
    pub cache_creation: i64,
    pub cache_read: i64,
}

/// Prices one run. Implemented by the engine over its rate table; kept behind
/// a trait so this crate does not depend on the (much larger) crate that owns
/// the table. Returns `None` when the model has no complete price.
pub trait UsdEstimator {
    fn estimate_usd(&self, model: &str, tokens: PricedTokens) -> Option<f64>;
}

impl<F: Fn(&str, PricedTokens) -> Option<f64>> UsdEstimator for F {
    fn estimate_usd(&self, model: &str, tokens: PricedTokens) -> Option<f64> {
        self(model, tokens)
    }
}

/// One run with everything the builders derive from it.
#[derive(bon::Builder)]
pub(crate) struct RunFacts<'a> {
    pub rec: &'a UsageRunRecord,
    pub coverage: RunCoverage,
    pub convention: InputConvention,
    pub spend_class: SpendClass,
    pub usd: UsdOutcome,
    #[builder(default)]
    pub caveats: Vec<UsageCaveat>,
    pub task: Option<&'a UsageTaskRecord>,
    pub driver: Option<&'a str>,
}

impl RunFacts<'_> {
    /// This run as the shared accumulator sees it.
    pub(crate) fn sample(&self) -> RunSample<'_> {
        RunSample {
            tokens: run_tokens(self.rec),
            agent_active_ms: self.rec.agent_active_ms,
            usage_unavailable: self.rec.usage_unavailable.as_deref(),
            usd: self.usd,
        }
    }
}

/// What classification needs from the request.
pub(crate) struct ClassifyContext<'a> {
    pub capture_start_epoch_s: i64,
    pub pricer: Option<&'a dyn UsdEstimator>,
}

/// Classifies the fields needed for filtering, dimension counts and grouping.
/// Pricing and caveat generation are deferred to [`finalize_kept`] so runs
/// dropped by filters or the run cap do not pay for them.
pub(crate) fn classify<'a>(rec: &'a UsageRunRecord, task: Option<&'a UsageTaskRecord>) -> RunFacts<'a> {
    let driver = rec.driver.slug();
    let coverage = run_coverage(rec);
    let convention = input_convention(driver);
    RunFacts::builder()
        .rec(rec)
        .coverage(coverage)
        .convention(convention)
        .spend_class(spend_class(&rec.execution_kind))
        .usd(UsdOutcome::NotRequested)
        .maybe_task(task)
        .maybe_driver(driver)
        .build()
}

/// Populates pricing and caveats on a run that survived filtering and the cap.
pub(crate) fn finalize_kept(facts: &mut RunFacts<'_>, ctx: &ClassifyContext<'_>) {
    facts.usd = price_run(facts.rec, facts.driver, facts.coverage, facts.convention, ctx.pricer);
    facts.caveats = caveats(facts, ctx);
}

/// The execution kind's spend class. The match is exhaustive over
/// [`ExecutionKind`], so a new kind fails to compile here until someone
/// decides which class it belongs to.
pub fn spend_class(kind: &str) -> SpendClass {
    let Ok(kind) = kind.parse::<ExecutionKind>() else {
        return SpendClass::Unclassified;
    };
    match kind {
        ExecutionKind::TaskImplementation
        | ExecutionKind::ChoreImplementation
        | ExecutionKind::RevisionImplementation
        | ExecutionKind::InvestigationImplementation
        | ExecutionKind::CiRemediation
        | ExecutionKind::ConflictResolution => SpendClass::Implementation,
        ExecutionKind::ProjectDesign | ExecutionKind::ProductDesign => SpendClass::Design,
        ExecutionKind::PrReview
        | ExecutionKind::PrReviewGuide
        | ExecutionKind::AutomationTriage
        | ExecutionKind::AnswerAgent
        | ExecutionKind::Progression => SpendClass::Overhead,
    }
}

/// How a driver reports `input`. Claude, Grok (ACP `inputTokens` minus cached
/// reads) and Antigravity (planner-step fresh input) report it net of cache
/// reads; Codex preserves the provider's gross field. See
/// `tools/boss/docs/run-token-usage.md`.
pub fn input_convention(driver: Option<&str>) -> InputConvention {
    match driver {
        Some(DRIVER_SLUG_CLAUDE | DRIVER_SLUG_GROK | DRIVER_SLUG_AGY) => InputConvention::Net,
        Some(DRIVER_SLUG_CODEX) => InputConvention::Gross,
        _ => InputConvention::Unknown,
    }
}

fn run_tokens(rec: &UsageRunRecord) -> RunTokens {
    RunTokens {
        input: rec.input_tokens,
        output: rec.output_tokens,
        cache_creation: rec.cache_creation_tokens,
        cache_read: rec.cache_read_tokens,
    }
}

/// Measurement state, matching `CostMeasurement`'s own counting: a run is
/// measured when any class is present, zero when all four are exactly zero,
/// and partial when it is measured but a class is NULL.
pub fn run_coverage(rec: &UsageRunRecord) -> RunCoverage {
    run_tokens(rec).coverage()
}

/// The run's token classes, `None` when unmeasured.
pub(crate) fn token_classes(rec: &UsageRunRecord, coverage: RunCoverage) -> Option<TokenClasses> {
    (coverage != RunCoverage::Unmeasured).then_some(TokenClasses {
        input: rec.input_tokens,
        output: rec.output_tokens,
        cache_creation: rec.cache_creation_tokens,
        cache_creation_5m: rec.cache_creation_5m_tokens,
        cache_creation_1h: rec.cache_creation_1h_tokens,
        cache_read: rec.cache_read_tokens,
    })
}

/// The sum of the classes the run reported, `None` when it reported none.
/// Never coalesces an unreported class to zero: a partial run's figure is a
/// floor and the run's coverage says so.
pub(crate) fn known_total(rec: &UsageRunRecord, coverage: RunCoverage) -> Option<i64> {
    (coverage != RunCoverage::Unmeasured).then(|| run_tokens(rec).known_total())
}

fn price_run(
    rec: &UsageRunRecord,
    driver: Option<&str>,
    coverage: RunCoverage,
    convention: InputConvention,
    pricer: Option<&dyn UsdEstimator>,
) -> UsdOutcome {
    let Some(pricer) = pricer else {
        return UsdOutcome::NotRequested;
    };
    // Order matters: the first rule that applies is the reported cause.
    if coverage == RunCoverage::Unmeasured {
        return UsdOutcome::Unpriceable(cause::UNMEASURED);
    }
    // By driver, not by model name: an Antigravity run can name a model the
    // API table prices, but it is subscription-billed.
    if matches!(driver, Some(DRIVER_SLUG_GROK | DRIVER_SLUG_AGY)) {
        return UsdOutcome::Unpriceable(cause::SUBSCRIPTION_UNPRICED);
    }
    match convention {
        InputConvention::Unknown => return UsdOutcome::Unpriceable(cause::DRIVER_UNKNOWN),
        InputConvention::Gross => return UsdOutcome::Unpriceable(cause::CAPTURE_UNNORMALISED),
        InputConvention::Net => {}
    }
    let (Some(input), Some(output), Some(cache_creation), Some(cache_read)) = (
        rec.input_tokens,
        rec.output_tokens,
        rec.cache_creation_tokens,
        rec.cache_read_tokens,
    ) else {
        return UsdOutcome::Unpriceable(cause::TOKENS_INCOMPLETE);
    };
    let Some(model) = rec.run_model.as_deref() else {
        return UsdOutcome::Unpriceable(cause::UNKNOWN_MODEL);
    };
    match pricer.estimate_usd(
        model,
        PricedTokens {
            input,
            output,
            cache_creation,
            cache_read,
        },
    ) {
        Some(usd) => UsdOutcome::Estimate(usd),
        None => UsdOutcome::Unpriceable(cause::UNKNOWN_MODEL),
    }
}

fn caveats(facts: &RunFacts<'_>, ctx: &ClassifyContext<'_>) -> Vec<UsageCaveat> {
    let rec = facts.rec;
    let measured = facts.coverage != RunCoverage::Unmeasured;
    let mut out = Vec::new();
    let mut push = |code: &str, detail: Option<String>| {
        out.push(UsageCaveat {
            code: code.to_owned(),
            detail,
        })
    };

    if rec.run_created_at_epoch_s < ctx.capture_start_epoch_s {
        push(caveat::PRE_CAPTURE, None);
    }
    if facts.driver.is_none() {
        push(caveat::DRIVER_NOT_RECORDED, None);
    }
    if measured && facts.convention == InputConvention::Gross {
        push(caveat::GROSS_INPUT, None);
    }
    if measured && facts.driver == Some(DRIVER_SLUG_CLAUDE) {
        push(caveat::SUBAGENTS_EXCLUDED, None);
    }
    if rec.run_model.as_deref() == Some(SYNTHETIC_MODEL) {
        push(caveat::SYNTHETIC_MODEL, rec.execution_model.clone());
    }
    if rec.run_model.is_none() && rec.execution_model.is_some() {
        push(caveat::MODEL_FROM_LAUNCH_TUPLE, rec.execution_model.clone());
    }
    if facts.coverage == RunCoverage::Partial {
        let missing: Vec<&str> = run_tokens(rec)
            .classes()
            .into_iter()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name)
            .collect();
        push(
            caveat::TOKENS_PARTIAL,
            Some(format!("not reported: {}", missing.join(", "))),
        );
    }
    if let Some(reason) = &rec.usage_unavailable {
        push(caveat::USAGE_UNAVAILABLE, Some(reason.clone()));
    }
    match facts.usd {
        UsdOutcome::Estimate(_) => push(caveat::ESTIMATE_ONLY, None),
        UsdOutcome::Unpriceable(cause::UNKNOWN_MODEL) => push(caveat::UNPRICED_MODEL, Some(model_label(rec))),
        UsdOutcome::Unpriceable(_) | UsdOutcome::NotRequested => {}
    }
    out
}

/// The model a USD gap is reported under: the observed model, or a marker
/// when none was observed.
pub(crate) fn model_label(rec: &UsageRunRecord) -> String {
    rec.run_model
        .clone()
        .unwrap_or_else(|| "(no model recorded)".to_owned())
}

/// A run's value for a dimension: the machine value filters match on, and the
/// label a client shows. Both are borrowed so scanning the window for filter
/// counts and grouping allocates nothing per run.
pub(crate) fn dimension_value<'a>(facts: &RunFacts<'a>, dimension: UsageDimension) -> (&'a str, &'a str) {
    let rec = facts.rec;
    let task = facts.task;
    let same = |value: &'a str| (value, value);
    let none = |label: &'static str| (NONE_VALUE, label);
    match dimension {
        UsageDimension::Product => match task {
            Some(task) => (
                task.product_id.as_str(),
                task.product_name.as_deref().unwrap_or(task.product_id.as_str()),
            ),
            None => none("(no task)"),
        },
        UsageDimension::Project => match task.and_then(|task| task.project_id.as_deref().map(|id| (id, task))) {
            Some((id, task)) => (id, task.project_name.as_deref().unwrap_or(id)),
            None => none("(no project)"),
        },
        UsageDimension::TaskKind => task.map_or_else(|| none("(no task)"), |task| same(task.kind.as_str())),
        UsageDimension::ExecutionKind => same(rec.execution_kind.as_str()),
        UsageDimension::SpendClass => same(facts.spend_class.as_str()),
        UsageDimension::Driver => facts.driver.map_or_else(|| none("not recorded"), same),
        UsageDimension::Model => rec
            .run_model
            .as_deref()
            .or(rec.execution_model.as_deref())
            .map_or_else(|| none("(no model recorded)"), same),
        UsageDimension::Effort => rec
            .execution_effort
            .as_deref()
            .or(task.and_then(|task| task.effort_level.as_deref()))
            .map_or_else(|| none("(none)"), same),
        UsageDimension::Reasoning => task
            .and_then(|task| task.reasoning.as_deref())
            .map_or_else(|| none("(none)"), same),
        UsageDimension::TaskStatus => task.map_or_else(|| none("(no task)"), |task| same(task.status.as_str())),
        UsageDimension::ExecutionStatus => same(rec.execution_status.as_str()),
        UsageDimension::Coverage => same(facts.coverage.as_str()),
        UsageDimension::Host => same(rec.run_host_id.as_str()),
    }
}
