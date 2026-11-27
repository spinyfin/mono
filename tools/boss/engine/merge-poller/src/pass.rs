//! One full detection pass: list every candidate, probe them in one batch,
//! record what the probe says, then run the upkeep that rides the same
//! cadence. See [`run_one_pass_tracked`] for the order and why it matters.

use std::collections::HashSet;

use super::deferred_review_admission::sweep_deferred_review_admission_with;
use super::host::SweepHost;
use super::pass_trace::PassPhase;
use super::walk_order::PassState;
use super::*;

/// How long a batched probe snapshot may be used to write lifecycle
/// transitions before it must be re-taken.
///
/// A pass takes ONE batched probe up front and then walks every candidate
/// against it. That is fine when the pass takes a second; it is a
/// correctness problem when it doesn't. In the incident this constant
/// exists for, PR #2346 merged ten seconds after a pass took its snapshot,
/// and the pass — stuck behind 32 minutes of inline conflict remediation —
/// went on writing state from that half-hour-old snapshot. Sized to the
/// nominal full-sweep interval: a snapshot older than one whole sweep
/// cadence is by definition something the next sweep should have replaced.
pub(crate) const PROBE_SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(60);

/// Re-probes allowed within a single pass before the pass gives up on its
/// remaining candidates. Bounded so a pathologically slow pass can't turn
/// into an unbounded GitHub-quota amplifier; abandoning the remainder is
/// safe because every candidate list is rebuilt from the DB next pass.
pub(crate) const MAX_PROBE_REFRESHES_PER_PASS: u8 = 2;

/// Share of the pass budget the open walk leaves for everything after it
/// when later groups have not yet run this pass. Without this, a walk that
/// costs more than the budget never yields, and stranded recovery, reviewer
/// upkeep, PR recheck and late-PR recovery never run in that pass.
pub(crate) const OPEN_WALK_RESERVE_DIVISOR: u32 = 4;

/// How much earlier the in-pass clock expires than the outer timeout, so
/// `remaining().is_zero()` can fire and the pass can finish instead of
/// being dropped mid-item. Sized for bookkeeping, not another GitHub-bound
/// candidate.
pub(crate) const PASS_YIELD_MARGIN: Duration = Duration::from_secs(2);

/// Wall-clock of one pass, used to stop a walk before it consumes the
/// reserve later phases need, and to yield before the outer timeout. Both
/// deadlines are `None` when the caller did not bound the pass
/// (`Duration::MAX`), so self-limiting is off.
struct PassClock {
    /// When the outer `tokio::time::timeout` fires.
    timeout_at: Option<tokio::time::Instant>,
    /// `timeout_at` minus [`PASS_YIELD_MARGIN`]: `remaining()` hits zero here.
    yield_at: Option<tokio::time::Instant>,
    budget: Duration,
}

impl PassClock {
    fn start(budget: Duration) -> Self {
        let now = tokio::time::Instant::now();
        let timeout_at = now.checked_add(budget);
        let yield_at = timeout_at.and_then(|deadline| deadline.checked_sub(PASS_YIELD_MARGIN));
        Self {
            timeout_at,
            yield_at,
            budget,
        }
    }

    fn remaining(&self) -> Duration {
        match self.yield_at {
            Some(deadline) => deadline.saturating_duration_since(tokio::time::Instant::now()),
            None => Duration::MAX,
        }
    }

    /// True when continuing this walk would leave later phases less than
    /// `budget / divisor` before the outer timeout.
    fn past_reserve(&self, divisor: u32) -> bool {
        let Some(timeout_at) = self.timeout_at else {
            return false;
        };
        timeout_at.saturating_duration_since(tokio::time::Instant::now()) <= self.budget / divisor
    }

    /// Stop starting more work. `leave_open_walk_reserve` is true for a
    /// rotated group while a later group has still to run this pass; the
    /// last group is bounded by the yield margin alone.
    fn should_yield(&self, leave_open_walk_reserve: bool) -> bool {
        (leave_open_walk_reserve && self.past_reserve(OPEN_WALK_RESERVE_DIVISOR)) || self.remaining().is_zero()
    }
}

#[derive(Clone, Copy)]
enum LatePhase {
    PrRecheck,
    BackgroundNudges,
    LatePr,
}

/// The local upkeep subphases, in the order a pass with turn 0 runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum UpkeepPhase {
    StrandedCi,
    StrandedBlocked,
    QueueRebounce,
    StalledReviewer,
    ReviewAdmission,
    ReviewBatchUpkeep,
}

/// The upkeep subphases in the order the pass with `turn` runs them: the
/// starting subphase advances every pass, so a slow early subphase cannot
/// keep the later ones from running on every pass.
pub(crate) fn upkeep_order(turn: u64) -> [UpkeepPhase; 6] {
    let mut subphases = [
        UpkeepPhase::StrandedCi,
        UpkeepPhase::StrandedBlocked,
        UpkeepPhase::QueueRebounce,
        UpkeepPhase::StalledReviewer,
        UpkeepPhase::ReviewAdmission,
        UpkeepPhase::ReviewBatchUpkeep,
    ];
    let len = subphases.len() as u64;
    subphases.rotate_left((turn % len) as usize);
    subphases
}

/// Groups after the terminal walk. Terminal merge observation always runs
/// first; these three rotate so a slow open walk or slow upkeep cannot
/// starve recovery forever.
#[derive(Clone, Copy)]
enum PostTerminalGroup {
    OpenWalk,
    Upkeep,
    LateFamilies,
}

/// The batched probe result for one pass, plus the age bookkeeping that
/// keeps a long pass from transitioning work items off a stale view of
/// GitHub. See [`PROBE_SNAPSHOT_MAX_AGE`].
pub(crate) struct ProbeSnapshot {
    pub(crate) results: HashMap<String, std::result::Result<PrLifecycleProbe, String>>,
    /// `tokio::time::Instant`, not `std::time::Instant`, so the ageing logic
    /// is drivable from tests under a paused clock (`tokio::time::advance`)
    /// instead of only by a real 60-second wall-clock stall. Identical to the
    /// std clock outside a paused runtime.
    taken_at: tokio::time::Instant,
    refreshes_left: u8,
    /// Set by the most recent [`Self::ensure_fresh`] call: true only when
    /// that call actually re-probed.
    refreshed_last_call: bool,
}

impl ProbeSnapshot {
    #[cfg(test)]
    pub(crate) fn new(results: HashMap<String, std::result::Result<PrLifecycleProbe, String>>) -> Self {
        Self::at(results, tokio::time::Instant::now())
    }

    /// `taken_at` is when the oldest contributing chunk started, not when
    /// the last chunk finished: a later slow chunk must not make an early
    /// result look fresh.
    pub(crate) fn at(
        results: HashMap<String, std::result::Result<PrLifecycleProbe, String>>,
        taken_at: tokio::time::Instant,
    ) -> Self {
        Self {
            results,
            taken_at,
            refreshes_left: MAX_PROBE_REFRESHES_PER_PASS,
            refreshed_last_call: false,
        }
    }

    /// Returns `true` when it is safe to write state from `self.results`.
    ///
    /// Fresh snapshot → `true` immediately (the overwhelmingly common case,
    /// and free — no clock-driven GitHub traffic). Stale snapshot → re-probe
    /// the unprocessed remainder and return `true` only if that refresh is
    /// itself still within [`PROBE_SNAPSHOT_MAX_AGE`]. A refresh ages from
    /// this pass's oldest contributing chunk start, so a slow later chunk
    /// cannot hide an early result. If that age already exceeds the max,
    /// another refresh runs while [`MAX_PROBE_REFRESHES_PER_PASS`] remains;
    /// once the budget is spent → `false`, and the caller must stop. Never
    /// authorize a write from a snapshot that is already stale: writing off a
    /// snapshot that old is worse than deferring to the next pass.
    ///
    /// `remaining_urls` is called **only** on the re-probe path and must
    /// yield just the candidates the pass has not walked yet. Re-probing the
    /// pass's whole original URL set would re-fetch every candidate already
    /// transitioned earlier in the pass, multiplying GitHub batch cost on
    /// precisely the path that is already slow enough to have gone stale.
    /// Nothing is lost by dropping the processed prefix: those entries are
    /// never read again, and every candidate list is rebuilt from the DB next
    /// pass. It is a closure rather than a slice so the common (fresh) case
    /// allocates nothing. The closure runs at most once: further refreshes
    /// in the same call reuse that remainder.
    pub(crate) async fn ensure_fresh(
        &mut self,
        probe: &dyn MergeProbe,
        state: &PassState,
        remaining_urls: impl FnOnce() -> Vec<String>,
    ) -> bool {
        self.refreshed_last_call = false;
        if self.taken_at.elapsed() <= PROBE_SNAPSHOT_MAX_AGE {
            return true;
        }
        if self.refreshes_left == 0 {
            tracing::warn!(
                snapshot_age_ms = self.taken_at.elapsed().as_millis(),
                max_age_ms = PROBE_SNAPSHOT_MAX_AGE.as_millis(),
                "merge poller: probe snapshot stale and the per-pass re-probe budget is spent; \
                 abandoning the rest of this pass rather than transitioning off a stale snapshot",
            );
            return false;
        }
        let urls = remaining_urls();
        loop {
            let age = self.taken_at.elapsed();
            self.refreshes_left -= 1;
            tracing::warn!(
                snapshot_age_ms = age.as_millis(),
                max_age_ms = PROBE_SNAPSHOT_MAX_AGE.as_millis(),
                pr_count = urls.len(),
                "merge poller: probe snapshot went stale mid-pass; re-probing the unprocessed remainder \
                 before writing further state",
            );
            state.probe_order.lock().unwrap().observation.clear();
            // Merge rather than replace: entries for candidates already walked
            // stay readable for the pass's adaptive-poll observations.
            // Age from when the oldest refresh chunk started, matching the
            // initial snapshot: a slow later chunk must not hide an early result.
            self.results.extend(probe.probe_batch(&urls).await);
            self.taken_at = state.probe_order.lock().unwrap().observation.taken_at();
            self.refreshed_last_call = true;
            let refresh_age = self.taken_at.elapsed();
            if refresh_age <= PROBE_SNAPSHOT_MAX_AGE {
                return true;
            }
            if self.refreshes_left == 0 {
                tracing::warn!(
                    snapshot_age_ms = refresh_age.as_millis(),
                    max_age_ms = PROBE_SNAPSHOT_MAX_AGE.as_millis(),
                    "merge poller: probe snapshot stale and the per-pass re-probe budget is spent; \
                     abandoning the rest of this pass rather than transitioning off a stale snapshot",
                );
                return false;
            }
        }
    }
}

/// Parked rows for every candidate this pass will walk or rebounce, in one
/// query. A failed read treats none as parked, matching
/// [`WorkDb::is_parked_or_assume_not`].
fn parked_ids_for_pass<'a>(
    work_db: &WorkDb,
    candidates: impl IntoIterator<Item = &'a PendingMergeCheck>,
) -> HashSet<String> {
    let ids: Vec<String> = candidates
        .into_iter()
        .map(|candidate| candidate.work_item_id.clone())
        .collect();
    match work_db.parked_work_item_ids(&ids) {
        Ok(parked) => parked,
        Err(err) => {
            tracing::warn!(
                ?err,
                "merge poller: failed to batch-load parked rows; treating none as parked",
            );
            HashSet::new()
        }
    }
}

/// Re-take the parked set when [`ProbeSnapshot::ensure_fresh`] actually
/// re-probed: a row parked during a slow walk must not keep the snapshot
/// from the start of the pass.
async fn ensure_fresh_refreshing_parked<'a>(
    snapshot: &mut ProbeSnapshot,
    probe: &dyn MergeProbe,
    state: &PassState,
    work_db: &WorkDb,
    parked_ids: &mut HashSet<String>,
    parked_scope: &[&'a PendingMergeCheck],
    remaining: impl IntoIterator<Item = &'a PendingMergeCheck>,
) -> bool {
    let ok = snapshot
        .ensure_fresh(probe, state, || remaining_probe_urls(remaining))
        .await;
    if ok && snapshot.refreshed_last_call {
        *parked_ids = parked_ids_for_pass(work_db, parked_scope.iter().copied());
    }
    ok
}

/// The still-unprocessed candidates' PR urls, de-duplicated and in walk
/// order — what [`ProbeSnapshot::ensure_fresh`] re-probes when it must.
fn remaining_probe_urls<'a>(remaining: impl IntoIterator<Item = &'a PendingMergeCheck>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    remaining
        .into_iter()
        .map(|c| c.pr_url.clone())
        .filter(|url| seen.insert(url.clone()))
        .collect()
}

/// Allocate source-capture observation sequences before a batched GitHub
/// probe starts. A slower, older response can therefore never outrank a
/// newer probe merely because it arrived later. The disabled rollout pays
/// no write cost and leaves the existing poller behavior unchanged.
///
/// Sequence allocation is deliberately adjacent to the actual GitHub
/// request rather than the later candidate walk: response timing must not
/// determine which comparison becomes the desired one.
pub(crate) fn allocate_source_observation_sequences(
    work_db: &WorkDb,
    completion_handler: Option<&dyn CompletionRequests>,
    probe_urls: &[String],
) -> HashMap<String, i64> {
    completion_handler
        .filter(|handler| handler.review_guide_source_capture_enabled())
        .map(
            // One block for the whole batch, in probe order: this runs
            // ahead of the probe on every pass, so its lock acquisitions
            // are paid before a single merge can be observed.
            |_| match work_db.allocate_pr_review_guide_source_observation_sequences(probe_urls.len()) {
                Ok(sequences) => probe_urls.iter().cloned().zip(sequences).collect(),
                Err(error) => {
                    tracing::warn!(
                        pr_count = probe_urls.len(),
                        ?error,
                        "merge poller: could not allocate review-guide source observation sequences",
                    );
                    HashMap::new()
                }
            },
        )
        .unwrap_or_default()
}

/// Run one full lifecycle sweep over every chore and project_task
/// the poller cares about (in_review with a PR, plus rows currently
/// blocked on merge_conflict so we can detect resolution, plus
/// `waiting_human` executions whose chore is still missing a
/// `pr_url`). Returns per-bucket counters so callers can log a
/// one-line summary.
///
/// `cube_client` is threaded into the conflict-watch retire path so
/// `on_resolved` can release the cube workspace lease the resolution
/// worker held (design Q5). Pass `None` for sweeps that don't need to
/// drive lease release — pre-Phase-3 wiring, tests, etc.
///
/// `completion_handler` is threaded in so the pending-PR-detection
/// recheck can reuse the on-Stop transition path
/// (`record_worker_pr_completion` + cube release + pane teardown + event
/// publish). Pass `None` for pre-`completion_handler` wiring and tests that
/// exercise only the in-review and conflict paths.
///
/// `host` is the engine-host seam ([`SweepHost`]) that carries out conflict
/// handling, design-doc pointer maintenance and project population. The host
/// owns the bounded background runner conflict remediation is handed to — it is
/// what keeps this function a *detection* pass: observing a conflict records
/// the attempt, publishes, and enqueues; it never awaits the escalation
/// ladder's cube lease + rebase + push. Pass `None` to run the sweep with none
/// of those engine services wired (a pass that only observes and promotes).
pub async fn run_one_pass(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    completion_handler: Option<&dyn CompletionRequests>,
    host: Option<&dyn SweepHost>,
) -> SweepOutcome {
    run_one_pass_observed(work_db, probe, publisher, cube_client, completion_handler, host)
        .await
        .0
}

/// [`run_one_pass`], plus the per-PR [`PollTier`] its probe implies for
/// every candidate the pass considered.
///
/// The tiers are free: the pass already probed every one of these PRs, and
/// throwing the classification away is what forced the adaptive layer to
/// spend a probe of its own just to *discover* that a PR is cold. Handing
/// them to [`PrPollSchedule::apply_sweep_observations`] lets the schedule be
/// derived from the sweep rather than guessed and corrected.
///
/// The observation list covers every URL the pass *listed*, not only the
/// ones it walked: the batched probe is taken up front for the whole set, so
/// a pass abandoned mid-walk still classified them all. A mid-pass
/// re-probe merges the remainder into the snapshot, so every listed URL keeps
/// a result; one whose probe failed is reported as `None` — no adaptive slot,
/// and the next sweep re-observes it.
///
/// Runs with no memory of earlier passes; the poller loop calls
/// [`run_one_pass_tracked`] with the [`PassState`] it keeps.
pub async fn run_one_pass_observed(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    completion_handler: Option<&dyn CompletionRequests>,
    host: Option<&dyn SweepHost>,
) -> (SweepOutcome, Vec<PollObservation>) {
    run_one_pass_tracked(
        work_db,
        probe,
        publisher,
        SweepHandlers {
            cube_client,
            completion_handler,
            host,
            parked_ids: None,
        },
        &PassState::default(),
        Duration::MAX,
    )
    .await
}

/// The held rows that get no open-PR upkeep: those on no upkeep list. The
/// lists are read one after another, so a row released to `in_review` between
/// two reads can be on both; it then keeps the upkeep its new status calls for.
fn terminal_watch_only_ids<'a>(
    held_active: &'a [PendingMergeCheck],
    upkeep: impl Iterator<Item = &'a PendingMergeCheck>,
) -> std::collections::HashSet<&'a str> {
    let upkeep: std::collections::HashSet<&str> = upkeep.map(|candidate| candidate.work_item_id.as_str()).collect();
    held_active
        .iter()
        .map(|candidate| candidate.work_item_id.as_str())
        .filter(|id| !upkeep.contains(id))
        .collect()
}

/// Whether the snapshot reports `pr_url` merged or closed — a terminal fact
/// the pass records before any open-PR upkeep.
fn probed_terminal(results: &HashMap<String, std::result::Result<PrLifecycleProbe, String>>, pr_url: &str) -> bool {
    matches!(
        results.get(pr_url),
        Some(Ok(probe)) if !matches!(probe.state, PrLifecycleState::Open(_))
    )
}

/// [`run_one_pass_observed`] for a caller that keeps a [`PassState`] across
/// passes: the phase timing is written to `state` as the pass runs, and the
/// candidate walks resume where the previous pass stopped.
///
/// The pass is ordered so that what it must not miss comes first and what
/// can be slow comes last:
///
/// 1. **List and probe.** Local queries, then one batched GitHub round trip.
///    Between the two, any pending auto-merge disarm is drained; that list
///    is empty unless conflict resolution has just stopped an armed merge.
/// 2. **Terminal walk.** Every candidate the probe reports merged or closed
///    is recorded before any open-PR upkeep runs. A merge costs the list
///    queries, the probe and its own transition — never the upkeep of the
///    candidates listed ahead of it.
/// 3. **Open walk, local upkeep, per-execution families.** These three
///    groups rotate their starting order across passes so a slow open walk
///    or slow upkeep cannot starve recovery forever. Terminal observation
///    always stays first. The open walk also stops when a quarter of the
///    budget remains *if later groups have not run this pass*; unwalked
///    candidates are not stamped. The in-pass clock expires a small margin
///    before the outer timeout so later groups can yield instead of being
///    dropped. Per-execution families (PR recheck, background nudges,
///    late PR) rotate among themselves the same way.
///
/// `budget` is the same cap the loop applies as a timeout. The in-pass
/// clock uses a shorter deadline so groups can yield; `Duration::MAX`
/// disables that. A pass dropped at its budget leaves `state` describing
/// how far it got. Unfinished observations remain retryable; subsequent
/// passes prioritize candidates least recently attended.
pub async fn run_one_pass_tracked(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    handlers: SweepHandlers<'_>,
    state: &PassState,
    budget: Duration,
) -> (SweepOutcome, Vec<PollObservation>) {
    let SweepHandlers {
        cube_client,
        completion_handler,
        host,
        parked_ids: _,
    } = handlers;
    let trace = &state.trace;
    trace.begin();
    let clock = PassClock::start(budget);
    let pass_probe = crate::probe_budget::PassProbe {
        inner: probe,
        state,
        pass_end: clock.yield_at,
    };
    let probe: &dyn MergeProbe = &pass_probe;
    trace.enter(PassPhase::ListCandidates, 0);
    let in_review = match work_db.list_chores_pending_merge_check() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list pending merge checks");
            Vec::new()
        }
    };
    let blocked_conflict = match work_db.list_chores_blocked_on_merge_conflict() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list chores blocked on merge_conflict",);
            Vec::new()
        }
    };
    let blocked_ci = match work_db.list_chores_blocked_on_ci_failure() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list chores blocked on ci_failure",);
            Vec::new()
        }
    };
    let blocked_other = match work_db.list_chores_blocked_other_with_pr() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(
                ?err,
                "merge poller: failed to list chores blocked with a PR for another reason",
            );
            Vec::new()
        }
    };
    // Rows on none of the lists above whose PR nothing else observes: watched
    // for a merge or a close only, never given open-PR upkeep.
    let held_active = match work_db.list_chores_held_active_with_pr() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list rows held active with a PR");
            Vec::new()
        }
    };
    let orphaned_chains = match work_db.list_orphaned_in_review_revision_chains() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list orphaned in-review revision chains");
            Vec::new()
        }
    };
    let pending_pr_recheck = match work_db.list_executions_pending_pr_detection() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list executions pending PR detection",);
            Vec::new()
        }
    };
    // Stop-boundary nudges held for delegated background work must be
    // revisited by a recurring path: the worker may never emit another Stop.
    // The handler snapshots the exact suppressed intents so this also covers
    // revision executions with an already-bound PR, not just the no-PR query
    // above.
    let pending_background_nudges = completion_handler
        .map(|handler| handler.pending_background_nudge_execution_ids())
        .unwrap_or_default();
    let stranded_ci_attempts = match work_db.list_stranded_ci_remediation_attempts() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list stranded ci remediation attempts",);
            Vec::new()
        }
    };
    let stranded_blocked = match work_db.list_chores_stranded_blocked_remediation() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(
                ?err,
                "merge poller: failed to list stranded blocked remediation parents",
            );
            Vec::new()
        }
    };
    // Late-PR candidates (Bug B recovery): terminal executions within
    // the last 60 min whose task is still `active` with no `pr_url`.
    // These arise from the double-spawn race where the orphan sweep
    // abandons exec_A while its pane is still running. The normal
    // `pending_pr_recheck` sweep (which only watches `waiting_human`)
    // cannot recover them; this sweep fills the gap.
    let late_pr_candidates: Vec<LatePrCandidate> = if completion_handler.is_some() {
        match work_db.list_recently_terminal_executions_pending_pr_detection(3600) {
            Ok(items) => items,
            Err(err) => {
                tracing::warn!(?err, "merge poller: failed to list late PR candidates",);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    // Runs on every pass, candidates or not: a pending disarm is driven from
    // its intent, and its card may be on none of the lists above. Its own
    // phase, because it probes and calls GitHub per pending disarm ahead of
    // the batched probe; the list is empty unless conflict resolution has
    // just stopped an armed merge.
    trace.enter(PassPhase::MergeDisarm, 0);
    if let Some(host) = host {
        super::disarm_drain::drain_pending_merge_disarms(work_db, probe, publisher, host).await;
    }
    let total = in_review.len()
        + blocked_conflict.len()
        + blocked_ci.len()
        + blocked_other.len()
        + held_active.len()
        + orphaned_chains.len()
        + pending_pr_recheck.len()
        + pending_background_nudges.len()
        + stranded_ci_attempts.len()
        + stranded_blocked.len()
        + late_pr_candidates.len();
    // De-duplicate by work_item_id: a chore that's both pending and
    // blocked-on-CI (shouldn't happen but defensive) only gets one
    // probe per sweep.
    let mut seen = std::collections::HashSet::new();
    let candidates: Vec<&PendingMergeCheck> = in_review
        .iter()
        .chain(blocked_conflict.iter())
        .chain(blocked_ci.iter())
        .chain(blocked_other.iter())
        .chain(held_active.iter())
        .filter(|candidate| seen.insert(candidate.work_item_id.as_str()))
        .collect();
    let terminal_watch_only = terminal_watch_only_ids(
        &held_active,
        in_review
            .iter()
            .chain(blocked_conflict.iter())
            .chain(blocked_ci.iter())
            .chain(blocked_other.iter()),
    );
    {
        // Keep the rotation memory the size of the live candidate set.
        let mut rotations = state.rotations();
        rotations.walk.retain(&seen);
        rotations
            .upkeep
            .stranded_ci
            .retain(&stranded_ci_attempts.iter().map(|a| a.work_item_id.as_str()).collect());
        rotations
            .upkeep
            .stranded_blocked
            .retain(&stranded_blocked.iter().map(|c| c.work_item_id.as_str()).collect());
        rotations
            .recheck
            .retain(&pending_pr_recheck.iter().map(String::as_str).collect());
        rotations
            .background_nudge
            .retain(&pending_background_nudges.iter().map(String::as_str).collect());
        rotations.late_pr.retain(
            &late_pr_candidates
                .iter()
                .map(|candidate| candidate.execution_id.as_str())
                .collect(),
        );
    }
    if total == 0 && !super::deferred_review_admission::has_pending_review_admission(work_db, completion_handler) {
        // No candidates at all: an empty observation set, which retires every
        // remaining adaptive slot rather than leaving orphans behind.
        trace.end();
        return (SweepOutcome::default(), Vec::new());
    }
    tracing::debug!(
        in_review = in_review.len(),
        blocked_conflict = blocked_conflict.len(),
        blocked_ci = blocked_ci.len(),
        blocked_other = blocked_other.len(),
        held_active = held_active.len(),
        orphaned_revision_chains = orphaned_chains.len(),
        pending_pr_recheck = pending_pr_recheck.len(),
        pending_background_nudges = pending_background_nudges.len(),
        stranded_ci_attempts = stranded_ci_attempts.len(),
        stranded_blocked = stranded_blocked.len(),
        late_pr_candidates = late_pr_candidates.len(),
        "merge poller: sweep started",
    );
    let mut outcome = SweepOutcome::default();
    // Every PR this pass tracks, probed by tier (see `probe_tier`): rows the
    // poller acts on in full, in bounded chunks, first; everything else only
    // for merge or close, and only when due. Covers both the `sweep_one`
    // candidates below and `stranded_blocked`, since both loops probe the
    // same way.
    let mut probe_url_seen = std::collections::HashSet::new();
    let probe_urls: Vec<String> = in_review
        .iter()
        .chain(blocked_conflict.iter())
        .chain(blocked_ci.iter())
        .chain(blocked_other.iter())
        .chain(stranded_blocked.iter())
        .chain(held_active.iter())
        .map(|candidate| candidate.pr_url.clone())
        .chain(orphaned_chains.iter().map(|chain| chain.pr_url.clone()))
        .filter(|url| probe_url_seen.insert(url.clone()))
        .collect();
    state.probe_order.lock().unwrap().retain(&probe_urls);
    // The `Probe` phase is two batched steps, one per tier, reported as one item:
    // entering with `probe_urls.len()` and never finishing those items would
    // report every probed URL as unfinished even on a healthy pass.
    trace.enter(PassPhase::Probe, 1);
    let in_review_parked = parked_ids_for_pass(work_db, in_review.iter());
    let merge_in_flight = match work_db.list_work_item_ids_with_pending_merge() {
        Ok(mut ids) => {
            match work_db.list_work_item_ids_in_merge_queue() {
                Ok(queued) => ids.extend(queued),
                Err(err) => tracing::warn!(
                    ?err,
                    "merge poller: could not read durable merge-queue membership; using merge requests only",
                ),
            }
            Some(ids)
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                "merge poller: could not read merges in flight; probing every in-review PR in full",
            );
            None
        }
    };
    let active_products = state.tiering().active_products.clone();
    let always_full: Vec<&str> = blocked_conflict
        .iter()
        .chain(blocked_ci.iter())
        .chain(blocked_other.iter())
        .chain(stranded_blocked.iter())
        .map(|candidate| candidate.pr_url.as_str())
        .collect();
    let crate::probe_tier::TieredProbe {
        results,
        unwalked,
        sequences: source_observation_sequences,
        taken_at,
    } = crate::probe_tier::probe_by_tier(
        probe,
        crate::probe_tier::TierRequest {
            work_db,
            completion_handler,
            state,
            inputs: crate::probe_tier::TierInputs::builder()
                .always_full(always_full)
                .in_review(&in_review)
                .parked(&in_review_parked)
                .maybe_merge_in_flight(merge_in_flight.as_ref())
                .maybe_active_products(active_products.as_ref())
                .terminal_watch(
                    held_active
                        .iter()
                        .map(|candidate| candidate.pr_url.as_str())
                        .chain(orphaned_chains.iter().map(|chain| chain.pr_url.as_str()))
                        .collect(),
                )
                .build(),
            all_urls: &probe_urls,
        },
    )
    .await;
    let mut snapshot = ProbeSnapshot::at(results, taken_at);
    trace.finish_item();

    // The pass's candidate walk, materialised so a mid-pass re-probe can be
    // scoped to the part of it that has not happened yet — see
    // [`ProbeSnapshot::ensure_fresh`]. Terminal facts first, then open-PR
    // upkeep; each half least recently attended first.
    // A PR watched only for merge or close has nothing to walk unless that
    // probe found it merged or closed. The same holds for a row watched only
    // for those facts whose PR was probed in full for another row: the full
    // result does not make open-PR upkeep apply to it.
    let (terminal, open): (Vec<&PendingMergeCheck>, Vec<&PendingMergeCheck>) = candidates
        .into_iter()
        .filter(|candidate| !unwalked.contains(&candidate.pr_url))
        .filter(|candidate| {
            probed_terminal(&snapshot.results, &candidate.pr_url)
                || !terminal_watch_only.contains(candidate.work_item_id.as_str())
        })
        .partition(|candidate| probed_terminal(&snapshot.results, &candidate.pr_url));
    // Taken from the first probe: a merge or a close does not go stale, and
    // a mid-pass re-probe covers only the candidate walks.
    let orphaned_terminal: Vec<(&crate::work::OrphanedRevisionChain, PrLifecycleProbe)> = orphaned_chains
        .iter()
        .filter_map(|chain| match snapshot.results.get(&chain.pr_url) {
            Some(Ok(probe)) if !matches!(probe.state, PrLifecycleState::Open(_)) => Some((chain, probe.clone())),
            _ => None,
        })
        .collect();
    let terminal_walk: Vec<&PendingMergeCheck> = {
        let rotations = state.rotations();
        rotations
            .walk
            .order(terminal, |candidate| candidate.work_item_id.as_str())
    };
    let open_walk: Vec<&PendingMergeCheck> = {
        let rotations = state.rotations();
        rotations.walk.order(open, |candidate| candidate.work_item_id.as_str())
    };
    let parked_scope: Vec<&PendingMergeCheck> = terminal_walk
        .iter()
        .copied()
        .chain(open_walk.iter().copied())
        .chain(stranded_blocked.iter())
        .collect();
    let mut parked_ids = parked_ids_for_pass(work_db, parked_scope.iter().copied());
    // Terminal merge observation always runs first and is never rotated.
    trace.enter(PassPhase::TerminalWalk, terminal_walk.len() + orphaned_terminal.len());
    for idx in 0..terminal_walk.len() {
        let candidate = terminal_walk[idx];
        trace.start_item(&candidate.work_item_id);
        if !ensure_fresh_refreshing_parked(
            &mut snapshot,
            probe,
            state,
            work_db,
            &mut parked_ids,
            &parked_scope,
            terminal_walk[idx..]
                .iter()
                .copied()
                .chain(open_walk.iter().copied())
                .chain(stranded_blocked.iter()),
        )
        .await
        {
            break;
        }
        state.rotations().walk.stamp(&candidate.work_item_id);
        sweep_one(
            work_db,
            &snapshot.results,
            &source_observation_sequences,
            (publisher, "full_sweep"),
            SweepHandlers {
                cube_client,
                completion_handler,
                host,
                parked_ids: Some(&parked_ids),
            },
            candidate,
            &mut outcome,
        )
        .await;
        trace.finish_item();
    }
    for (chain, terminal_probe) in &orphaned_terminal {
        trace.start_item(&chain.chain_root_id);
        settle_orphaned_revision_chain(
            work_db,
            publisher,
            completion_handler,
            chain,
            terminal_probe,
            &mut outcome,
        )
        .await;
        trace.finish_item();
    }

    // Advance before any rotated group runs so a pass dropped in the open
    // walk still rotates: next pass starts with a later group.
    let turn = state.next_phase_turn();
    let mut groups = [
        PostTerminalGroup::OpenWalk,
        PostTerminalGroup::Upkeep,
        PostTerminalGroup::LateFamilies,
    ];
    groups.rotate_left((turn % 3) as usize);
    let skip_unbatched_review_holds = completion_handler
        .map(|handler| handler.review_batch_fanout_enabled())
        .unwrap_or(false);

    // Groups that still read the probe snapshot and have not started: a
    // re-probe must cover them, or it would replace results they are about
    // to use.
    let mut open_walk_started = false;
    let mut stranded_blocked_started = false;

    for (position, group) in groups.into_iter().enumerate() {
        // The reserve for later groups only matters while one follows.
        let reserve = position + 1 < groups.len();
        match group {
            PostTerminalGroup::OpenWalk => {
                open_walk_started = true;
                trace.enter(PassPhase::OpenWalk, open_walk.len());
                for idx in 0..open_walk.len() {
                    // Leave the reserve while a later group still has to run:
                    // an item started with less than a quarter of the budget
                    // left is dropped by the outer timeout and would be
                    // stamped, starving the tail. The last group is bounded
                    // by the yield margin alone.
                    if clock.should_yield(reserve) {
                        tracing::warn!(
                            deferred = open_walk.len() - idx,
                            walked = idx,
                            open_total = open_walk.len(),
                            "merge poller: open walk stopped to leave time for later phases; \
                             deferred open candidates are first next pass",
                        );
                        break;
                    }
                    let candidate = open_walk[idx];
                    trace.start_item(&candidate.work_item_id);
                    let pending_stranded: &[PendingMergeCheck] = if stranded_blocked_started {
                        &[]
                    } else {
                        &stranded_blocked
                    };
                    if !ensure_fresh_refreshing_parked(
                        &mut snapshot,
                        probe,
                        state,
                        work_db,
                        &mut parked_ids,
                        &parked_scope,
                        open_walk[idx..].iter().copied().chain(pending_stranded.iter()),
                    )
                    .await
                    {
                        break;
                    }
                    // No probe result means nothing was observed: leave the
                    // candidate unstamped and unfinished so the pass reports
                    // an incomplete walk and it is first next pass.
                    if !snapshot.results.contains_key(&candidate.pr_url) {
                        tracing::warn!(
                            work_item_id = %candidate.work_item_id,
                            pr_url = %candidate.pr_url,
                            "merge poller: open walk found no probe result for this PR; will retry next pass",
                        );
                        continue;
                    }
                    state.rotations().walk.stamp(&candidate.work_item_id);
                    sweep_one(
                        work_db,
                        &snapshot.results,
                        &source_observation_sequences,
                        (publisher, "full_sweep"),
                        SweepHandlers {
                            cube_client,
                            completion_handler,
                            host,
                            parked_ids: Some(&parked_ids),
                        },
                        candidate,
                        &mut outcome,
                    )
                    .await;
                    trace.finish_item();
                }
            }
            PostTerminalGroup::Upkeep => {
                if clock.should_yield(false) {
                    continue;
                }
                // The subphases rotate their starting point, advanced before
                // any awaited work, so a slow early subphase cannot keep the
                // later ones from ever running.
                let upkeep_turn = state.next_upkeep_turn();
                let subphases = upkeep_order(upkeep_turn);
                for subphase in subphases {
                    match subphase {
                        // Rescue stranded ci_remediations attempts: `pending` rows with no live
                        // execution. These arise when two dequeue events land in the same sweep —
                        // the first flips the task (consuming the WHERE guard on
                        // `mark_chore_blocked_ci_failure`) and the second inserts a ci_remediations
                        // row but cannot create an execution.
                        UpkeepPhase::StrandedCi => {
                            trace.enter(PassPhase::StrandedCi, stranded_ci_attempts.len());
                            let attempts = state
                                .rotations()
                                .upkeep
                                .stranded_ci
                                .order(stranded_ci_attempts.iter().collect(), |attempt| {
                                    attempt.work_item_id.as_str()
                                });
                            for attempt in attempts {
                                if clock.should_yield(false) {
                                    break;
                                }
                                trace.start_item(&attempt.work_item_id);
                                state.rotations().upkeep.stranded_ci.stamp(&attempt.work_item_id);
                                if ci_watch::rescue_stranded_ci_remediation_attempt(work_db, publisher, attempt).await {
                                    outcome.ci_remediation_redispatched += 1;
                                }
                                trace.finish_item();
                            }
                        }
                        // Reconcile stranded `blocked` parents (NULL scalar reason, empty
                        // active-signal set, remediation-owned, bound PR).
                        UpkeepPhase::StrandedBlocked => {
                            stranded_blocked_started = true;
                            trace.enter(PassPhase::StrandedBlocked, stranded_blocked.len());
                            let stranded_walk = state
                                .rotations()
                                .upkeep
                                .stranded_blocked
                                .order(stranded_blocked.iter().collect(), |candidate| {
                                    candidate.work_item_id.as_str()
                                });
                            for idx in 0..stranded_walk.len() {
                                if clock.should_yield(false) {
                                    break;
                                }
                                let candidate = stranded_walk[idx];
                                trace.start_item(&candidate.work_item_id);
                                state.rotations().upkeep.stranded_blocked.stamp(&candidate.work_item_id);
                                let pending_open: &[&PendingMergeCheck] =
                                    if open_walk_started { &[] } else { &open_walk };
                                if !ensure_fresh_refreshing_parked(
                                    &mut snapshot,
                                    probe,
                                    state,
                                    work_db,
                                    &mut parked_ids,
                                    &parked_scope,
                                    stranded_walk[idx..].iter().copied().chain(pending_open.iter().copied()),
                                )
                                .await
                                {
                                    break;
                                }
                                sweep_stranded_blocked_remediation(
                                    work_db,
                                    &snapshot.results,
                                    publisher,
                                    host,
                                    candidate,
                                    &mut outcome,
                                )
                                .await;
                                trace.finish_item();
                            }
                        }
                        // Merge-queue rebounce: one batched GraphQL round trip, then
                        // per-candidate checks. Refresh parked rows first so a row
                        // parked during the open walk is not bounced.
                        UpkeepPhase::QueueRebounce => {
                            trace.enter(PassPhase::QueueRebounce, 0);
                            if !clock.should_yield(false) {
                                parked_ids = parked_ids_for_pass(work_db, parked_scope.iter().copied());
                                // Rebounce acts on queued PRs, which are all in the full
                                // tier; a PR watched only for merge or close has no
                                // queue entry this poller is responsible for.
                                let rebounce_in_review: Vec<PendingMergeCheck> = in_review
                                    .iter()
                                    .filter(|candidate| !unwalked.contains(&candidate.pr_url))
                                    .cloned()
                                    .collect();
                                run_merge_queue_rebounce_pass(
                                    work_db,
                                    publisher,
                                    host,
                                    &rebounce_in_review,
                                    &blocked_ci,
                                    Some(&parked_ids),
                                    &mut outcome,
                                )
                                .await;
                            }
                        }
                        // Reviewer-fallback: tasks held in `active` (PendingReview)
                        // while waiting for an AI reviewer pass that has since finished
                        // or timed out. Timeout: 10 minutes.
                        UpkeepPhase::StalledReviewer => {
                            trace.enter(PassPhase::StalledReviewer, 0);
                            let reviewer_stale_secs: u64 = 10 * 60;
                            let stalled_candidates = match work_db.list_tasks_with_stalled_reviewer(reviewer_stale_secs)
                            {
                                Ok(items) => items,
                                Err(err) => {
                                    tracing::warn!(?err, "merge poller: failed to list stalled reviewer tasks");
                                    Vec::new()
                                }
                            };
                            let stalled_candidates = {
                                let mut rotations = state.rotations();
                                rotations
                                    .upkeep
                                    .stalled_reviewer
                                    .retain(&stalled_candidates.iter().map(|item| item.0.as_str()).collect());
                                rotations
                                    .upkeep
                                    .stalled_reviewer
                                    .order(stalled_candidates.iter().collect(), |item| item.0.as_str())
                            };
                            trace.set_items_total(stalled_candidates.len());
                            for (task_id, product_id, pr_url, review_result_written) in stalled_candidates {
                                if clock.should_yield(false) {
                                    break;
                                }
                                trace.start_item(task_id);
                                state.rotations().upkeep.stalled_reviewer.stamp(task_id);
                                sweep_stalled_reviewer(
                                    work_db,
                                    publisher,
                                    StalledReview {
                                        task_id,
                                        product_id,
                                        pr_url,
                                        review_result_written: *review_result_written,
                                        skip_unbatched_review_holds,
                                    },
                                    &GhPrStateChecker,
                                    &mut outcome,
                                )
                                .await;
                                trace.finish_item();
                            }
                        }
                        UpkeepPhase::ReviewAdmission => {
                            trace.enter(PassPhase::ReviewAdmission, 0);
                            if !clock.should_yield(false)
                                && skip_unbatched_review_holds
                                && let Some(handler) = completion_handler
                            {
                                let pool_size = handler.review_pool_size();
                                sweep_deferred_review_admission_with(
                                    work_db,
                                    publisher,
                                    handler.review_batch_enqueuer(),
                                    None,
                                    pool_size,
                                    &mut outcome,
                                )
                                .await;
                            }
                        }
                        UpkeepPhase::ReviewBatchUpkeep => {
                            trace.enter(PassPhase::ReviewBatchUpkeep, 0);
                            if !clock.should_yield(false) {
                                if let Err(err) = work_db.sweep_reported_live_review_batch_members() {
                                    tracing::warn!(?err, "merge poller: failed to sweep reported live batch members");
                                }
                                match work_db.reap_inert_review_batches(crate::work::REVIEW_BATCH_STALE_SECS) {
                                    Ok(reaped) => {
                                        outcome.review_batches_reaped = reaped.len();
                                        if !reaped.is_empty() {
                                            tracing::info!(
                                                reaped = reaped.len(),
                                                "merge poller: reaped inert review batches to release pool reservations"
                                            );
                                        }
                                    }
                                    Err(err) => {
                                        tracing::warn!(?err, "merge poller: failed to reap inert review batches");
                                    }
                                }
                            }
                        }
                    }
                }
            }
            PostTerminalGroup::LateFamilies => {
                if let Some(handler) = completion_handler {
                    if clock.should_yield(reserve) {
                        continue;
                    }
                    // Only a pass with comfortable time for this group
                    // advances family rotation, so a family is not charged
                    // for a pass that reached it with the reserve already
                    // spent. Items are still stamped before they run.
                    let family_turn = if clock.should_yield(true) {
                        state.late_family_turn()
                    } else {
                        state.next_late_family_turn()
                    };
                    let rechecks = state
                        .rotations()
                        .recheck
                        .order(pending_pr_recheck.iter().collect(), |execution_id| {
                            execution_id.as_str()
                        });
                    let nudges = state
                        .rotations()
                        .background_nudge
                        .order(pending_background_nudges.iter().collect(), |execution_id| {
                            execution_id.as_str()
                        });
                    let late = state
                        .rotations()
                        .late_pr
                        .order(late_pr_candidates.iter().collect(), |candidate| {
                            candidate.execution_id.as_str()
                        });
                    let mut late_phases = [LatePhase::PrRecheck, LatePhase::BackgroundNudges, LatePhase::LatePr];
                    late_phases.rotate_left((family_turn % 3) as usize);
                    for phase in late_phases {
                        if clock.should_yield(reserve) {
                            break;
                        }
                        match phase {
                            LatePhase::PrRecheck => {
                                trace.enter(PassPhase::PrRecheck, rechecks.len());
                                for execution_id in &rechecks {
                                    if clock.should_yield(reserve) {
                                        break;
                                    }
                                    trace.start_item(execution_id);
                                    state.rotations().recheck.stamp(execution_id);
                                    sweep_pending_pr(handler, execution_id, &mut outcome).await;
                                    trace.finish_item();
                                }
                            }
                            LatePhase::BackgroundNudges => {
                                trace.enter(PassPhase::BackgroundNudges, nudges.len());
                                for execution_id in &nudges {
                                    if clock.should_yield(reserve) {
                                        break;
                                    }
                                    trace.start_item(execution_id);
                                    state.rotations().background_nudge.stamp(execution_id);
                                    if let Some(recheck_outcome) = handler.recheck_background_nudge(execution_id).await
                                    {
                                        tracing::info!(
                                            execution_id,
                                            outcome = ?recheck_outcome,
                                            "merge poller: recurring background-child nudge recheck completed",
                                        );
                                    }
                                    trace.finish_item();
                                }
                            }
                            LatePhase::LatePr => {
                                trace.enter(PassPhase::LatePr, late.len());
                                for candidate in &late {
                                    if clock.should_yield(reserve) {
                                        break;
                                    }
                                    trace.start_item(&candidate.execution_id);
                                    state.rotations().late_pr.stamp(&candidate.execution_id);
                                    sweep_late_pr(handler, candidate, &mut outcome).await;
                                    trace.finish_item();
                                }
                            }
                        }
                    }
                } else if !pending_pr_recheck.is_empty() {
                    tracing::debug!(
                        count = pending_pr_recheck.len(),
                        "merge poller: pending PR-detection candidates skipped (no completion_handler wired)",
                    );
                }
            }
        }
    }
    // Two cheap reads, taken whether or not the upkeep group had time to run,
    // so a pass under load does not publish the gauges as 0.
    sample_review_pool_gauges(work_db, &mut outcome);
    trace.end();

    let observations = poll_observations(&probe_urls, &snapshot.results);
    (outcome, observations)
}

/// Classify each probed URL into the tier that should drive its adaptive
/// polling. Shared by the full sweep and the batched adaptive reconcile so
/// both feed [`PrPollSchedule`] through identical rules: a URL whose probe
/// failed yields `None` (no adaptive slot — the full sweep re-probes it).
pub(crate) fn poll_observations(
    pr_urls: &[String],
    results: &HashMap<String, std::result::Result<PrLifecycleProbe, String>>,
) -> Vec<PollObservation> {
    pr_urls
        .iter()
        .map(|url| {
            let tier = results
                .get(url)
                .and_then(|r| r.as_ref().ok())
                .and_then(poll_tier_for_probe);
            (url.clone(), tier)
        })
        .collect()
}
