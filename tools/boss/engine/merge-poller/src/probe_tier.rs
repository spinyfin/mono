//! Which PRs a detection pass probes in full, which it probes only to learn
//! whether they merged or closed, and how often.
//!
//! A full probe (CI rollup, reviews, mergeability) is the expensive query
//! and is only worth its cost for a PR the poller will *act on*. Everything
//! else is tracked solely so a merge or a close is still recorded, and a
//! shallow probe (see [`crate::probe::PR_SHALLOW_FIELDS`]) answers that for a
//! tenth of the work, for many PRs per query, on a slower cadence.
//!
//! | Tier    | Rows                                                                                         | Cadence       |
//! | ------- | -------------------------------------------------------------------------------------------- | ------------- |
//! | full    | blocked rows (merge conflict, CI failure, dependency / review feedback, stranded)            | every pass    |
//! |         | `in_review` rows in an active product, not parked, not dormant                               | every pass    |
//! |         | `in_review` rows with a merge in flight (a recorded merge request, durable queue state, or last seen queued), including parked rows | every pass    |
//! | shallow | parked rows without a merge in flight, rows in other products, rows GitHub has not touched for [`DORMANT_AFTER_SECS`] | every [`SHALLOW_PROBE_INTERVAL`] |
//!
//! Merge and close detection covers every tracked PR in every product; only
//! how often a PR is asked about, and how much, depends on the tier. Nothing
//! here remembers that a PR was "not merged": a shallow row that is not due is
//! simply not asked about this pass, and a due row is always asked afresh.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::probe::ProbeDepth;

/// How long a PR watched only for merge or close goes between probes.
pub(crate) const SHALLOW_PROBE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// A PR GitHub last updated at least this long ago is dormant: nobody has
/// pushed, commented, reviewed or labelled it for two weeks, so it is watched
/// like a parked one until something touches it. The shallow probe reads
/// `updatedAt`, so activity returns the PR to the full tier on the next pass.
pub(crate) const DORMANT_AFTER_SECS: i64 = 14 * 24 * 60 * 60;

/// What the poller remembers about one PR between passes.
#[derive(Debug)]
struct UrlMemory {
    /// When the poller first tracked the URL, the age of one never observed.
    first_seen: Instant,
    /// When any probe last answered for it.
    last_observed: Option<Instant>,
    /// GitHub's `updatedAt` at the last probe that reported one.
    updated_at: Option<String>,
    /// The last full probe saw the PR in a merge queue or with auto-merge armed.
    queued: bool,
}

/// Per-PR memory the tier decision reads, kept across passes in
/// [`crate::PassState`].
///
/// It holds facts about *when and what was observed*, never a conclusion
/// that a PR is still unmerged: every shallow probe asks GitHub afresh.
#[derive(Debug, Default)]
pub(crate) struct ProbeMemory {
    by_url: HashMap<String, UrlMemory>,
}

impl ProbeMemory {
    /// Track exactly `live`: new URLs start unobserved, URLs no longer
    /// tracked are forgotten, so the memory stays the size of the candidate set.
    pub(crate) fn track(&mut self, live: &HashSet<&str>, now: Instant) {
        self.by_url.retain(|url, _| live.contains(url.as_str()));
        for url in live {
            self.by_url.entry((*url).to_owned()).or_insert_with(|| UrlMemory {
                first_seen: now,
                last_observed: None,
                updated_at: None,
                queued: false,
            });
        }
    }

    /// Record a successful full probe.
    pub(crate) fn observe_full(&mut self, url: &str, probe: &PrLifecycleProbe, now: Instant) {
        let Some(entry) = self.by_url.get_mut(url) else { return };
        entry.last_observed = Some(now);
        if let Some(updated_at) = &probe.updated_at {
            entry.updated_at = Some(updated_at.clone());
        }
        entry.queued =
            matches!(probe.state, PrLifecycleState::Open(_)) && (probe.in_merge_queue || probe.auto_merge_enabled);
    }

    /// Record a successful shallow probe, including whether it saw the PR
    /// queued or with auto-merge armed so the next pass can promote it.
    pub(crate) fn observe_shallow(&mut self, url: &str, updated_at: Option<&str>, queued: bool, now: Instant) {
        let Some(entry) = self.by_url.get_mut(url) else { return };
        entry.last_observed = Some(now);
        if let Some(updated_at) = updated_at {
            entry.updated_at = Some(updated_at.to_owned());
        }
        entry.queued = queued;
    }

    /// GitHub has not touched the PR for [`DORMANT_AFTER_SECS`], as of the
    /// last probe that reported when. A PR never probed, or whose probe gave no
    /// `updatedAt`, is not dormant: unknown is treated as active.
    pub(crate) fn is_dormant(&self, url: &str, epoch_now: i64) -> bool {
        self.by_url
            .get(url)
            .and_then(|entry| entry.updated_at.as_deref())
            .and_then(boss_engine_utils::iso8601::parse_iso8601_lenient)
            .is_some_and(|updated| epoch_now.saturating_sub(updated) >= DORMANT_AFTER_SECS)
    }

    /// The last probe saw the PR queued or with auto-merge armed.
    pub(crate) fn is_queued(&self, url: &str) -> bool {
        self.by_url.get(url).is_some_and(|entry| entry.queued)
    }

    /// A shallow probe of the PR is due: it has never been observed, or was
    /// last observed [`SHALLOW_PROBE_INTERVAL`] ago or more. Any observation
    /// counts, a full one included: a PR that has just left the full tier was
    /// seen a moment ago.
    pub(crate) fn shallow_due(&self, url: &str, now: Instant) -> bool {
        match self.by_url.get(url).and_then(|entry| entry.last_observed) {
            Some(observed) => now.saturating_duration_since(observed) >= SHALLOW_PROBE_INTERVAL,
            None => true,
        }
    }

    /// How long the PR has gone without any probe answering for it, counted
    /// from when it was first tracked if it never has.
    pub(crate) fn unobserved_for(&self, url: &str, now: Instant) -> Duration {
        self.by_url.get(url).map_or(Duration::ZERO, |entry| {
            now.saturating_duration_since(entry.last_observed.unwrap_or(entry.first_seen))
        })
    }
}

/// What the probe tiers remember between passes, kept in [`crate::PassState`].
#[derive(Debug, Default)]
pub(crate) struct TierState {
    /// When each tracked PR was last observed, and what it showed.
    pub(crate) memory: ProbeMemory,
    /// The active products: the product selected in the app plus any named
    /// by dispatch focus. Set by the loop before each pass. `None` singles
    /// out no product, so every product is active.
    pub(crate) active_products: Option<HashSet<String>>,
}

/// Why a PR is in the tier it is in. Counted into the per-pass log line so a
/// surprising tier is explainable without reading the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TierReason {
    /// Blocked on a conflict, a CI failure or another reason that keeps a PR
    /// on the poller's lists, or stranded without a signal.
    Blocked,
    /// `in_review` in an active product.
    ActiveProduct,
    /// A merge is in flight for the row.
    MergeInFlight,
    /// Parked: kept only to record that the PR merged or closed.
    Parked,
    /// `in_review` in a product that is not active.
    OtherProduct,
    /// Not updated on GitHub for [`DORMANT_AFTER_SECS`].
    Dormant,
}

impl TierReason {
    fn is_full(self) -> bool {
        matches!(self, Self::Blocked | Self::ActiveProduct | Self::MergeInFlight)
    }
}

/// What a pass knows about its candidates when it chooses tiers.
pub(crate) struct TierInputs<'a> {
    /// PR urls of rows that are always probed in full.
    pub(crate) always_full: Vec<&'a str>,
    /// The `in_review` rows.
    pub(crate) in_review: &'a [PendingMergeCheck],
    /// Work item ids of parked rows among `in_review`.
    pub(crate) parked: &'a HashSet<String>,
    /// Work item ids with a merge in flight; `None` when that could not be
    /// read, which counts every row as having one rather than guess.
    pub(crate) merge_in_flight: Option<&'a HashSet<String>>,
    /// The active products: the product selected in the app plus any named
    /// by dispatch focus. `None` when no product is singled out, which makes
    /// every product active.
    pub(crate) active_products: Option<&'a HashSet<String>>,
}

/// The tiers one pass probes.
#[derive(Debug, Default)]
pub(crate) struct TierPlan {
    /// PR urls probed in full this pass.
    pub(crate) full: Vec<String>,
    /// Every PR url watched only for merge or close.
    pub(crate) shallow: Vec<String>,
    /// The part of [`Self::shallow`] due for a probe this pass.
    pub(crate) shallow_due: Vec<String>,
    /// Row counts by [`TierReason`].
    pub(crate) reasons: HashMap<&'static str, usize>,
}

impl TierReason {
    /// The name a [`TierPlan::reasons`] count is filed under.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::ActiveProduct => "active_product",
            Self::MergeInFlight => "merge_in_flight",
            Self::Parked => "parked",
            Self::OtherProduct => "other_product",
            Self::Dormant => "dormant",
        }
    }
}

fn in_review_tier(
    row: &PendingMergeCheck,
    inputs: &TierInputs<'_>,
    memory: &ProbeMemory,
    epoch_now: i64,
) -> TierReason {
    // A merge in flight is detected on every pass even while the row is
    // parked: the parked skip in `sweep_one` still withholds open-PR upkeep.
    let merging =
        inputs.merge_in_flight.is_none_or(|ids| ids.contains(&row.work_item_id)) || memory.is_queued(&row.pr_url);
    if merging {
        return TierReason::MergeInFlight;
    }
    if inputs.parked.contains(&row.work_item_id) {
        return TierReason::Parked;
    }
    if inputs
        .active_products
        .is_some_and(|active| !active.contains(&row.product_id))
    {
        return TierReason::OtherProduct;
    }
    if memory.is_dormant(&row.pr_url, epoch_now) {
        return TierReason::Dormant;
    }
    TierReason::ActiveProduct
}

/// Choose the tier of every tracked PR. A PR any of whose rows needs the full
/// probe is probed in full; otherwise it is shallow, and due when it has not
/// been observed for [`SHALLOW_PROBE_INTERVAL`].
pub(crate) fn plan_tiers(inputs: &TierInputs<'_>, memory: &ProbeMemory, now: Instant, epoch_now: i64) -> TierPlan {
    let mut plan = TierPlan::default();
    let mut full_seen: HashSet<&str> = HashSet::new();
    for url in &inputs.always_full {
        *plan.reasons.entry(TierReason::Blocked.key()).or_default() += 1;
        if full_seen.insert(url) {
            plan.full.push((*url).to_owned());
        }
    }
    let mut shallow_candidates: Vec<&str> = Vec::new();
    for row in inputs.in_review {
        let reason = in_review_tier(row, inputs, memory, epoch_now);
        *plan.reasons.entry(reason.key()).or_default() += 1;
        if reason.is_full() {
            if full_seen.insert(&row.pr_url) {
                plan.full.push(row.pr_url.clone());
            }
        } else {
            shallow_candidates.push(&row.pr_url);
        }
    }
    let mut shallow_seen: HashSet<&str> = HashSet::new();
    for url in shallow_candidates {
        if full_seen.contains(url) || !shallow_seen.insert(url) {
            continue;
        }
        plan.shallow.push(url.to_owned());
        if memory.shallow_due(url, now) {
            plan.shallow_due.push(url.to_owned());
        }
    }
    plan
}

/// What one pass's probes did, for the per-pass log line.
#[derive(bon::Builder)]
pub(crate) struct TierOutcome<'a> {
    pub(crate) plan: &'a TierPlan,
    pub(crate) full_observed: usize,
    pub(crate) shallow_observed: usize,
    pub(crate) full_chunks: crate::ProbeWork,
    pub(crate) shallow_chunks: crate::ProbeWork,
    pub(crate) active_products: Option<&'a HashSet<String>>,
}

/// Log how the pass spent its probes, per tier. One line a pass, at `info`:
/// starvation of the slow tier is a matter of a number growing across lines,
/// and it must be visible without raising the log level.
pub(crate) fn log_tier_outcome(outcome: &TierOutcome<'_>, memory: &ProbeMemory, now: Instant) {
    let oldest = |urls: &[String]| {
        urls.iter()
            .map(|url| memory.unobserved_for(url, now))
            .max()
            .unwrap_or_default()
    };
    let chunk = |work: &crate::ProbeWork| {
        // `deferred` counts every chunk not completed; split it into the ones
        // that were sent and failed, and the ones never sent.
        let failed = work.attempted.saturating_sub(work.completed);
        let not_sent = work.deferred.saturating_sub(failed);
        (work.attempted, failed, not_sent)
    };
    let (full_attempted, full_failed, full_deferred) = chunk(&outcome.full_chunks);
    let (shallow_attempted, shallow_failed, shallow_deferred) = chunk(&outcome.shallow_chunks);
    let count = |reason: TierReason| outcome.plan.reasons.get(reason.key()).copied().unwrap_or(0);
    tracing::info!(
        full_prs = outcome.plan.full.len(),
        full_observed = outcome.full_observed,
        full_chunks_attempted = full_attempted,
        full_chunks_failed = full_failed,
        full_chunks_deferred = full_deferred,
        full_oldest_unobserved_secs = oldest(&outcome.plan.full).as_secs(),
        shallow_prs = outcome.plan.shallow.len(),
        shallow_due = outcome.plan.shallow_due.len(),
        shallow_observed = outcome.shallow_observed,
        shallow_chunks_attempted = shallow_attempted,
        shallow_chunks_failed = shallow_failed,
        shallow_chunks_deferred = shallow_deferred,
        shallow_oldest_unobserved_secs = oldest(&outcome.plan.shallow).as_secs(),
        rows_blocked = count(TierReason::Blocked),
        rows_active_product = count(TierReason::ActiveProduct),
        rows_merge_in_flight = count(TierReason::MergeInFlight),
        rows_parked = count(TierReason::Parked),
        rows_other_product = count(TierReason::OtherProduct),
        rows_dormant = count(TierReason::Dormant),
        active_products = outcome.active_products.map_or(0, HashSet::len),
        every_product_active = outcome.active_products.is_none(),
        "merge poller: probe tiers",
    );
}

/// One pass's probe request: every tracked PR and what the pass knows about
/// the rows behind them.
pub(crate) struct TierRequest<'a> {
    pub(crate) work_db: &'a WorkDb,
    pub(crate) completion_handler: Option<&'a dyn CompletionRequests>,
    pub(crate) state: &'a crate::PassState,
    pub(crate) inputs: TierInputs<'a>,
    /// Every tracked PR url, deduplicated.
    pub(crate) all_urls: &'a [String],
}

/// What [`probe_by_tier`] hands back to the pass.
pub(crate) struct TieredProbe {
    /// What the walks read: every full probe result, plus the shallow
    /// probes that found their PR merged or closed.
    pub(crate) results: HashMap<String, std::result::Result<PrLifecycleProbe, String>>,
    /// Shallow-tier PRs the walks have nothing to do for: still open, not due
    /// this pass, or not answered. A shallow probe says nothing about an open
    /// PR beyond that it is open, so no open-PR upkeep may run from it.
    pub(crate) unwalked: HashSet<String>,
    /// Review-guide source observation sequences, allocated ahead of the full
    /// probe (see [`crate::pass::allocate_source_observation_sequences`]).
    pub(crate) sequences: HashMap<String, i64>,
    /// When the oldest contributing chunk started. The snapshot ages from
    /// this instant, not from when the last chunk finished.
    pub(crate) taken_at: Instant,
}

/// Probe every tracked PR at the depth its tier calls for: the full tier
/// first, then the shallow PRs that are due, so the PRs the poller acts on are
/// never queued behind the ones it only watches.
pub(crate) async fn probe_by_tier(probe: &dyn MergeProbe, request: TierRequest<'_>) -> TieredProbe {
    let TierRequest {
        work_db,
        completion_handler,
        state,
        inputs,
        all_urls,
    } = request;
    let plan = {
        let mut tiering = state.tiering();
        let now = Instant::now();
        tiering
            .memory
            .track(&all_urls.iter().map(String::as_str).collect(), now);
        plan_tiers(
            &inputs,
            &tiering.memory,
            now,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
    };
    let sequences = crate::pass::allocate_source_observation_sequences(work_db, completion_handler, &plan.full);
    state.probe_order.lock().unwrap().observation.clear();
    let trace = &state.trace;
    let full_chunks_before = trace.probe_work(ProbeDepth::Full.chunk_work_kind());
    let shallow_chunks_before = trace.probe_work(ProbeDepth::Shallow.chunk_work_kind());
    let mut results = probe.probe_batch(&plan.full).await;
    let shallow_results = if plan.shallow_due.is_empty() {
        HashMap::new()
    } else {
        probe.probe_shallow_batch(&plan.shallow_due).await
    };
    let observation = state.probe_order.lock().unwrap().observation.clone();

    let mut shallow_observed = 0;
    let mut terminal_shallow = HashSet::new();
    {
        let mut tiering = state.tiering();
        let memory = &mut tiering.memory;
        for (url, result) in &results {
            if let Ok(full) = result {
                memory.observe_full(url, full, observation.observed_at(url));
            }
        }
        for (url, result) in shallow_results {
            match result {
                Ok(ShallowProbe::Open {
                    updated_at,
                    in_merge_queue,
                    auto_merge_enabled,
                }) => {
                    memory.observe_shallow(
                        &url,
                        updated_at.as_deref(),
                        in_merge_queue || auto_merge_enabled,
                        observation.observed_at(&url),
                    );
                    shallow_observed += 1;
                }
                Ok(ShallowProbe::Terminal(terminal)) => {
                    memory.observe_shallow(
                        &url,
                        terminal.updated_at.as_deref(),
                        terminal.in_merge_queue || terminal.auto_merge_enabled,
                        observation.observed_at(&url),
                    );
                    shallow_observed += 1;
                    terminal_shallow.insert(url.clone());
                    results.insert(url, Ok(*terminal));
                }
                // Not observed: it stays due, and is asked again next pass.
                Err(_) => {}
            }
        }
    }
    let unwalked: HashSet<String> = plan
        .shallow
        .iter()
        .filter(|url| !terminal_shallow.contains(*url))
        .cloned()
        .collect();

    let tiering = state.tiering();
    log_tier_outcome(
        &TierOutcome::builder()
            .plan(&plan)
            .full_observed(
                plan.full
                    .iter()
                    .filter(|url| matches!(results.get(*url), Some(Ok(_))))
                    .count(),
            )
            .shallow_observed(shallow_observed)
            .full_chunks(
                trace
                    .probe_work(ProbeDepth::Full.chunk_work_kind())
                    .saturating_sub(&full_chunks_before),
            )
            .shallow_chunks(
                trace
                    .probe_work(ProbeDepth::Shallow.chunk_work_kind())
                    .saturating_sub(&shallow_chunks_before),
            )
            .maybe_active_products(tiering.active_products.as_ref())
            .build(),
        &tiering.memory,
        Instant::now(),
    );
    drop(tiering);
    TieredProbe {
        results,
        unwalked,
        sequences,
        taken_at: observation.taken_at(),
    }
}
