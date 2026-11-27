//! Per-run classification: measurement state, input convention, spend class,
//! caveats, dimension values and the USD outcome.
//!
//! Everything here is a pure function of one [`UsageRunRecord`] (and its
//! task), so each rule has a one-case unit test and the tree builder never has
//! to re-derive one.

use boss_protocol::{
    DRIVER_SLUG_AGY, DRIVER_SLUG_CLAUDE, DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK, DriverSource, ExecutionKind,
    InputConvention, RunCoverage, SpendClass, TokenClasses, UsageCaveat, UsageDimension,
};

pub(crate) use crate::fold::UsdOutcome;
use crate::fold::{RunCalls, RunSample, RunTokens};
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
    /// Non-zero cache-write tokens were priced at the single 5m `cache_write`
    /// rate rather than distinct 5m/1h rates — either because the run recorded
    /// no 5m/1h split, or because the model entry has no separate 1h rate.
    pub const UNSPLIT_CACHE_WRITE: &str = "unsplit_cache_write";
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, bon::Builder)]
pub struct PricedTokens {
    pub input: i64,
    pub output: i64,
    pub cache_creation: i64,
    pub cache_creation_5m: Option<i64>,
    pub cache_creation_1h: Option<i64>,
    pub cache_read: i64,
}

/// One run's USD estimate plus whether non-zero cache-write tokens used the
/// single 5-minute fallback rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsdEstimateResult {
    pub usd: f64,
    pub unsplit_cache_write: bool,
}

/// Prices one run. Implemented by the engine over its rate table; kept behind
/// a trait so this crate does not depend on the (much larger) crate that owns
/// the table. Returns `None` when the model has no complete price.
pub trait UsdEstimator {
    fn estimate_usd(&self, model: &str, tokens: PricedTokens) -> Option<f64>;

    /// Detailed estimate carrying whether non-zero cache-write tokens fell
    /// back to the single 5-minute `cache_write` rate (either because the run
    /// recorded no 5m/1h split or because the model entry has no separate 1h
    /// rate).
    fn estimate_usd_detailed(&self, model: &str, tokens: PricedTokens) -> Option<UsdEstimateResult> {
        let usd = self.estimate_usd(model, tokens)?;
        let unsplit_cache_write =
            tokens.cache_creation > 0 && (tokens.cache_creation_5m.is_none() || tokens.cache_creation_1h.is_none());
        Some(UsdEstimateResult {
            usd,
            unsplit_cache_write,
        })
    }
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

    /// This run as the call-statistics accumulator sees it.
    pub(crate) fn calls(&self) -> RunCalls {
        let tokens = run_tokens(self.rec);
        RunCalls {
            rounds: self.rec.rounds,
            known_tokens: tokens.is_measured().then(|| tokens.known_total()),
            first_call_context: self.rec.first_call_context_tokens,
            peak_context: self.rec.peak_context_tokens,
            compactions: self.rec.compactions,
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
    price_if_requested(facts, ctx);
    facts.caveats = caveats(facts, ctx);
}

/// Prices a run when the caller requested USD. The series path uses this
/// without attaching per-run caveat detail strings.
pub(crate) fn price_if_requested(facts: &mut RunFacts<'_>, ctx: &ClassifyContext<'_>) {
    facts.usd = price_run(facts.rec, facts.driver, facts.coverage, facts.convention, ctx.pricer);
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
    match pricer.estimate_usd_detailed(
        model,
        PricedTokens {
            input,
            output,
            cache_creation,
            cache_creation_5m: rec.cache_creation_5m_tokens,
            cache_creation_1h: rec.cache_creation_1h_tokens,
            cache_read,
        },
    ) {
        Some(est) => UsdOutcome::Estimate {
            usd: est.usd,
            unsplit_cache_write: est.unsplit_cache_write,
        },
        None => UsdOutcome::Unpriceable(cause::UNKNOWN_MODEL),
    }
}

/// Single source of truth for per-run caveat predicates. Detail strings are
/// computed lazily so callers that only need caveat codes (such as
/// `build_usage_series_report`) avoid per-run allocations without duplicating
/// any predicate.
fn visit_caveats(
    facts: &RunFacts<'_>,
    ctx: &ClassifyContext<'_>,
    mut visit: impl FnMut(&'static str, &dyn Fn() -> Option<String>),
) {
    let rec = facts.rec;
    let measured = facts.coverage != RunCoverage::Unmeasured;

    if rec.run_created_at_epoch_s < ctx.capture_start_epoch_s {
        visit(caveat::PRE_CAPTURE, &|| None);
    }
    if facts.driver.is_none() {
        visit(caveat::DRIVER_NOT_RECORDED, &|| None);
    }
    if measured && facts.convention == InputConvention::Gross {
        visit(caveat::GROSS_INPUT, &|| None);
    }
    if measured && facts.driver == Some(DRIVER_SLUG_CLAUDE) {
        visit(caveat::SUBAGENTS_EXCLUDED, &|| None);
    }
    if rec.run_model.as_deref() == Some(SYNTHETIC_MODEL) {
        visit(caveat::SYNTHETIC_MODEL, &|| rec.execution_model.clone());
    }
    if rec.run_model.is_none() && rec.execution_model.is_some() {
        visit(caveat::MODEL_FROM_LAUNCH_TUPLE, &|| rec.execution_model.clone());
    }
    if facts.coverage == RunCoverage::Partial {
        visit(caveat::TOKENS_PARTIAL, &|| {
            let missing: Vec<&str> = run_tokens(rec)
                .classes()
                .into_iter()
                .filter(|(_, value)| value.is_none())
                .map(|(name, _)| name)
                .collect();
            Some(format!("not reported: {}", missing.join(", ")))
        });
    }
    if let Some(reason) = &rec.usage_unavailable {
        visit(caveat::USAGE_UNAVAILABLE, &|| Some(reason.clone()));
    }
    match facts.usd {
        UsdOutcome::Estimate {
            unsplit_cache_write, ..
        } => {
            visit(caveat::ESTIMATE_ONLY, &|| None);
            if unsplit_cache_write {
                visit(caveat::UNSPLIT_CACHE_WRITE, &|| None);
            }
        }
        UsdOutcome::Unpriceable(cause::UNKNOWN_MODEL) => {
            visit(caveat::UNPRICED_MODEL, &|| Some(model_label(rec)));
        }
        UsdOutcome::Unpriceable(_) | UsdOutcome::NotRequested => {}
    }
}

fn caveats(facts: &RunFacts<'_>, ctx: &ClassifyContext<'_>) -> Vec<UsageCaveat> {
    let mut out = Vec::new();
    visit_caveats(facts, ctx, |code, detail| {
        out.push(UsageCaveat {
            code: code.to_owned(),
            detail: detail(),
        });
    });
    out
}

/// The model a USD gap is reported under: the observed model, or a marker
/// when none was observed.
pub(crate) fn model_label(rec: &UsageRunRecord) -> String {
    rec.run_model
        .clone()
        .unwrap_or_else(|| "(no model recorded)".to_owned())
}

/// Fields a dimension reads. Built from a run, from an execution (latest-run
/// model/host, aggregated coverage) or from a completed task (representative
/// run), so group keys and labels cannot drift across those grains.
#[derive(Clone, Copy, bon::Builder)]
pub(crate) struct DimensionSource<'a> {
    pub task: Option<&'a UsageTaskRecord>,
    pub execution_kind: Option<&'a str>,
    pub execution_status: Option<&'a str>,
    pub execution_effort: Option<&'a str>,
    pub execution_model: Option<&'a str>,
    pub driver: Option<&'a str>,
    pub driver_source: DriverSource,
    pub run_model: Option<&'a str>,
    pub coverage: Option<RunCoverage>,
    pub host_id: Option<&'a str>,
    pub spend_class: Option<SpendClass>,
}

impl<'a> DimensionSource<'a> {
    pub(crate) fn from_run(facts: &'a RunFacts<'a>) -> Self {
        Self {
            task: facts.task,
            execution_kind: Some(facts.rec.execution_kind.as_str()),
            execution_status: Some(facts.rec.execution_status.as_str()),
            execution_effort: facts.rec.execution_effort.as_deref(),
            execution_model: facts.rec.execution_model.as_deref(),
            driver: facts.driver,
            driver_source: facts.rec.driver.source(),
            run_model: facts.rec.run_model.as_deref(),
            coverage: Some(facts.coverage),
            host_id: Some(facts.rec.run_host_id.as_str()),
            spend_class: Some(facts.spend_class),
        }
    }
}

/// A run's value for a dimension: the machine value filters match on, and the
/// label a client shows. Both are borrowed so scanning the window for filter
/// counts and grouping allocates nothing per run.
pub(crate) fn dimension_value<'a>(facts: &'a RunFacts<'a>, dimension: UsageDimension) -> (&'a str, &'a str) {
    dimension_value_of(&DimensionSource::from_run(facts), dimension)
}

pub(crate) fn dimension_value_of<'a>(src: &DimensionSource<'a>, dimension: UsageDimension) -> (&'a str, &'a str) {
    let same = |value: &'a str| (value, value);
    let none = |label: &'static str| (NONE_VALUE, label);
    match dimension {
        UsageDimension::Product => match src.task {
            Some(task) => (
                task.product_id.as_str(),
                task.product_name.as_deref().unwrap_or(task.product_id.as_str()),
            ),
            None => none("(no task)"),
        },
        UsageDimension::Project => match src
            .task
            .and_then(|task| task.project_id.as_deref().map(|id| (id, task)))
        {
            Some((id, task)) => (id, task.project_name.as_deref().unwrap_or(id)),
            None => none("(no project)"),
        },
        UsageDimension::TaskKind => src
            .task
            .map_or_else(|| none("(no task)"), |task| same(task.kind.as_str())),
        UsageDimension::ExecutionKind => src
            .execution_kind
            .filter(|kind| !kind.is_empty())
            .map_or_else(|| none("(none)"), same),
        UsageDimension::SpendClass => src
            .spend_class
            .map_or_else(|| none("(none)"), |class| same(class.as_str())),
        UsageDimension::Driver => src.driver.map_or_else(|| none("not recorded"), same),
        UsageDimension::Model => src
            .run_model
            .or(src.execution_model)
            .map_or_else(|| none("(no model recorded)"), same),
        UsageDimension::Effort => src
            .execution_effort
            .or(src.task.and_then(|task| task.effort_level.as_deref()))
            .map_or_else(|| none("(none)"), same),
        UsageDimension::Reasoning => src
            .task
            .and_then(|task| task.reasoning.as_deref())
            .map_or_else(|| none("(none)"), same),
        UsageDimension::TaskStatus => src
            .task
            .map_or_else(|| none("(no task)"), |task| same(task.status.as_str())),
        UsageDimension::ExecutionStatus => src
            .execution_status
            .filter(|status| !status.is_empty())
            .map_or_else(|| none("(none)"), same),
        UsageDimension::Coverage => src
            .coverage
            .map_or_else(|| none("(no runs)"), |coverage| same(coverage.as_str())),
        UsageDimension::Host => src.host_id.map_or_else(|| none("(none)"), same),
    }
}

/// Whether a dimension has a recorded (not placeholder) value. Driver uses
/// the launch-tuple source so `dimension_started` tracks when slugs were
/// written on the execution row, not later resolution.
pub(crate) fn has_recorded_dimension(src: &DimensionSource<'_>, dimension: UsageDimension) -> bool {
    match dimension {
        UsageDimension::Driver => src.driver_source == DriverSource::LaunchTuple,
        UsageDimension::Model => src.run_model.is_some() || src.execution_model.is_some(),
        UsageDimension::Effort => src.execution_effort.is_some(),
        _ => dimension_value_of(src, dimension).0 != NONE_VALUE,
    }
}

/// Latest run by `created_at`, then `run_id`. Shared by execution grouping
/// and completed-task representative selection.
pub(crate) fn latest_run<'a, I>(runs: I) -> Option<&'a RunFacts<'a>>
where
    I: IntoIterator<Item = &'a RunFacts<'a>>,
{
    runs.into_iter().max_by(|a, b| {
        a.rec
            .run_created_at_epoch_s
            .cmp(&b.rec.run_created_at_epoch_s)
            .then_with(|| a.rec.run_id.cmp(&b.rec.run_id))
    })
}

/// Visit each caveat code for a run without allocating per-run detail strings
/// or a temporary `Vec`.
pub(crate) fn for_each_caveat_code(
    facts: &RunFacts<'_>,
    ctx: &ClassifyContext<'_>,
    mut visit: impl FnMut(&'static str),
) {
    visit_caveats(facts, ctx, |code, _| visit(code));
}

/// Collect caveat codes without allocating per-run detail strings.
#[cfg(test)]
pub(crate) fn count_caveat_codes(facts: &RunFacts<'_>, ctx: &ClassifyContext<'_>) -> Vec<&'static str> {
    let mut codes = Vec::new();
    for_each_caveat_code(facts, ctx, |code| codes.push(code));
    codes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RecordedDriver;

    struct FixedPricer;

    impl UsdEstimator for FixedPricer {
        fn estimate_usd(&self, model: &str, _tokens: PricedTokens) -> Option<f64> {
            (model == "claude-sonnet-4").then_some(1.25)
        }
    }

    fn base_run(id: &str, created_at: i64) -> UsageRunRecord {
        UsageRunRecord::builder()
            .run_id(id)
            .run_status("completed")
            .run_created_at_epoch_s(created_at)
            .run_host_id("host-1")
            .execution_id("e1")
            .work_item_id("t1")
            .execution_kind("task_implementation")
            .execution_status("completed")
            .execution_created_at_epoch_s(created_at)
            .build()
    }

    #[test]
    fn count_caveat_codes_matches_caveats_across_every_condition() {
        let capture_start = 1_000;
        let pricer = FixedPricer;
        let ctx = ClassifyContext {
            capture_start_epoch_s: capture_start,
            pricer: Some(&pricer),
        };

        // 1. Pre-capture + driver_not_recorded + model_from_launch_tuple + usage_unavailable
        let mut r_pre = base_run("r_pre", capture_start - 10);
        r_pre.driver = RecordedDriver::Unknown;
        r_pre.execution_model = Some("opus".to_owned());
        r_pre.usage_unavailable = Some("transcript missing".to_owned());

        // 2. Codex gross_input + tokens_partial
        let mut r_codex = base_run("r_codex", capture_start + 10);
        r_codex.driver = RecordedDriver::LaunchTuple(DRIVER_SLUG_CODEX.to_owned());
        r_codex.input_tokens = Some(100);
        r_codex.output_tokens = Some(20);

        // 3. Claude subagents_excluded + estimate_only + unsplit_cache_write (priced USD)
        let mut r_claude_priced = base_run("r_claude_priced", capture_start + 20);
        r_claude_priced.driver = RecordedDriver::LaunchTuple(DRIVER_SLUG_CLAUDE.to_owned());
        r_claude_priced.run_model = Some("claude-sonnet-4".to_owned());
        r_claude_priced.input_tokens = Some(100);
        r_claude_priced.output_tokens = Some(20);
        r_claude_priced.cache_creation_tokens = Some(50);
        r_claude_priced.cache_read_tokens = Some(0);

        // 4. Claude synthetic_model + unpriced_model (unknown model USD)
        let mut r_synthetic = base_run("r_synthetic", capture_start + 30);
        r_synthetic.driver = RecordedDriver::LaunchTuple(DRIVER_SLUG_CLAUDE.to_owned());
        r_synthetic.run_model = Some(SYNTHETIC_MODEL.to_owned());
        r_synthetic.execution_model = Some("sonnet".to_owned());
        r_synthetic.input_tokens = Some(100);
        r_synthetic.output_tokens = Some(20);
        r_synthetic.cache_creation_tokens = Some(0);
        r_synthetic.cache_read_tokens = Some(0);

        let runs = [r_pre, r_codex, r_claude_priced, r_synthetic];
        let mut seen_codes = std::collections::BTreeSet::new();
        for rec in &runs {
            let mut facts = classify(rec, None);
            price_if_requested(&mut facts, &ctx);
            let from_caveats: Vec<String> = caveats(&facts, &ctx).into_iter().map(|c| c.code).collect();
            let from_codes: Vec<String> = count_caveat_codes(&facts, &ctx)
                .into_iter()
                .map(str::to_owned)
                .collect();
            assert_eq!(from_codes, from_caveats, "caveat code mismatch for {}", rec.run_id);
            seen_codes.extend(from_codes);
        }

        let expected_all: std::collections::BTreeSet<String> = [
            caveat::PRE_CAPTURE,
            caveat::DRIVER_NOT_RECORDED,
            caveat::GROSS_INPUT,
            caveat::SUBAGENTS_EXCLUDED,
            caveat::SYNTHETIC_MODEL,
            caveat::MODEL_FROM_LAUNCH_TUPLE,
            caveat::TOKENS_PARTIAL,
            caveat::USAGE_UNAVAILABLE,
            caveat::ESTIMATE_ONLY,
            caveat::UNSPLIT_CACHE_WRITE,
            caveat::UNPRICED_MODEL,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(seen_codes, expected_all, "parity test must cover every caveat code");
    }
}
