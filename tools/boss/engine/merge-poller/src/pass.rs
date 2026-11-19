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

    /// Stop starting more work. `leave_open_walk_reserve` is true for the
    /// open walk while later groups have not run this pass.
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
    pub(crate) fn new(results: HashMap<String, std::result::Result<PrLifecycleProbe, String>>) -> Self {
        Self {
            results,
            taken_at: tokio::time::Instant::now(),
            refreshes_left: MAX_PROBE_REFRESHES_PER_PASS,
            refreshed_last_call: false,
        }
    }

    /// Returns `true` when it is safe to write state from `self.results`.
    ///
    /// Fresh snapshot → `true` immediately (the overwhelmingly common case,
    /// and free — no clock-driven GitHub traffic). Stale snapshot → re-probe
    /// and return `true`. Stale snapshot with the per-pass refresh budget
    /// already spent → `false`, and the caller must stop: writing off a
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
    /// allocates nothing.
    pub(crate) async fn ensure_fresh(
        &mut self,
        probe: &dyn MergeProbe,
        remaining_urls: impl FnOnce() -> Vec<String>,
    ) -> bool {
        self.refreshed_last_call = false;
        let age = self.taken_at.elapsed();
        if age <= PROBE_SNAPSHOT_MAX_AGE {
            return true;
        }
        if self.refreshes_left == 0 {
            tracing::warn!(
                snapshot_age_ms = age.as_millis(),
                max_age_ms = PROBE_SNAPSHOT_MAX_AGE.as_millis(),
                "merge poller: probe snapshot stale and the per-pass re-probe budget is spent; \
                 abandoning the rest of this pass rather than transitioning off a stale snapshot",
            );
            return false;
        }
        self.refreshes_left -= 1;
        let urls = remaining_urls();
        tracing::warn!(
            snapshot_age_ms = age.as_millis(),
            max_age_ms = PROBE_SNAPSHOT_MAX_AGE.as_millis(),
            pr_count = urls.len(),
            "merge poller: probe snapshot went stale mid-pass; re-probing the unprocessed remainder \
             before writing further state",
        );
        self.results = probe.probe_batch(&urls).await;
        self.taken_at = tokio::time::Instant::now();
        self.refreshed_last_call = true;
        true
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
    work_db: &WorkDb,
    parked_ids: &mut HashSet<String>,
    parked_scope: &[&'a PendingMergeCheck],
    remaining: impl IntoIterator<Item = &'a PendingMergeCheck>,
) -> bool {
    let ok = snapshot.ensure_fresh(probe, || remaining_probe_urls(remaining)).await;
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
/// a pass abandoned mid-walk still classified them all. An entry the
/// snapshot no longer holds (a mid-pass re-probe replaces the results map
/// with just the unprocessed remainder) is reported as `None` — no adaptive
/// slot, and the next sweep re-observes it.
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
/// how far it got; nothing it did not reach is lost, because the next
/// pass's rotation starts with exactly those candidates.
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
        .filter(|candidate| seen.insert(candidate.work_item_id.as_str()))
        .collect();
    {
        // Keep the rotation memory the size of the live candidate set.
        let mut rotations = state.rotations();
        rotations.walk.retain(&seen);
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
        pending_pr_recheck = pending_pr_recheck.len(),
        pending_background_nudges = pending_background_nudges.len(),
        stranded_ci_attempts = stranded_ci_attempts.len(),
        stranded_blocked = stranded_blocked.len(),
        late_pr_candidates = late_pr_candidates.len(),
        "merge poller: sweep started",
    );
    let mut outcome = SweepOutcome::default();
    // Batch every PR this pass will probe into one round trip
    // (`MergeProbe::probe_batch`) instead of one `gh pr view` per PR —
    // covers both the `sweep_one` candidates below and `stranded_blocked`,
    // since both loops probe the same way.
    let mut probe_url_seen = std::collections::HashSet::new();
    let probe_urls: Vec<String> = in_review
        .iter()
        .chain(blocked_conflict.iter())
        .chain(blocked_ci.iter())
        .chain(blocked_other.iter())
        .chain(stranded_blocked.iter())
        .map(|candidate| candidate.pr_url.clone())
        .filter(|url| probe_url_seen.insert(url.clone()))
        .collect();
    // One batched GitHub request, so the phase is one item: entering with
    // `probe_urls.len()` and never finishing those items would report every
    // probed URL as unfinished even on a healthy pass.
    trace.enter(PassPhase::Probe, 1);
    let source_observation_sequences = allocate_source_observation_sequences(work_db, completion_handler, &probe_urls);
    let mut snapshot = ProbeSnapshot::new(probe.probe_batch(&probe_urls).await);
    trace.finish_item();

    // The pass's candidate walk, materialised so a mid-pass re-probe can be
    // scoped to the part of it that has not happened yet — see
    // [`ProbeSnapshot::ensure_fresh`]. Terminal facts first, then open-PR
    // upkeep; each half least recently attended first.
    let (terminal, open): (Vec<&PendingMergeCheck>, Vec<&PendingMergeCheck>) = candidates
        .into_iter()
        .partition(|candidate| probed_terminal(&snapshot.results, &candidate.pr_url));
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
    trace.enter(PassPhase::TerminalWalk, terminal_walk.len());
    for idx in 0..terminal_walk.len() {
        let candidate = terminal_walk[idx];
        trace.start_item(&candidate.work_item_id);
        if !ensure_fresh_refreshing_parked(
            &mut snapshot,
            probe,
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

    for group in groups {
        match group {
            PostTerminalGroup::OpenWalk => {
                trace.enter(PassPhase::OpenWalk, open_walk.len());
                for idx in 0..open_walk.len() {
                    // Always leave the reserve: an item started with less
                    // than a quarter of the budget left is dropped by the
                    // outer timeout and would be stamped, starving the tail.
                    if clock.should_yield(true) {
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
                    if !ensure_fresh_refreshing_parked(
                        &mut snapshot,
                        probe,
                        work_db,
                        &mut parked_ids,
                        &parked_scope,
                        open_walk[idx..].iter().copied().chain(stranded_blocked.iter()),
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
            }
            PostTerminalGroup::Upkeep => {
                // Rescue stranded ci_remediations attempts: `pending` rows with no live
                // execution. These arise when two dequeue events land in the same sweep —
                // the first flips the task (consuming the WHERE guard on
                // `mark_chore_blocked_ci_failure`) and the second inserts a ci_remediations
                // row but cannot create an execution.
                trace.enter(PassPhase::StrandedCi, stranded_ci_attempts.len());
                for attempt in &stranded_ci_attempts {
                    if clock.should_yield(false) {
                        break;
                    }
                    trace.start_item(&attempt.work_item_id);
                    if ci_watch::rescue_stranded_ci_remediation_attempt(work_db, publisher, attempt).await {
                        outcome.ci_remediation_redispatched += 1;
                    }
                    trace.finish_item();
                }
                // Reconcile stranded `blocked` parents (NULL scalar reason, empty
                // active-signal set, remediation-owned, bound PR).
                trace.enter(PassPhase::StrandedBlocked, stranded_blocked.len());
                for idx in 0..stranded_blocked.len() {
                    if clock.should_yield(false) {
                        break;
                    }
                    trace.start_item(&stranded_blocked[idx].work_item_id);
                    if !ensure_fresh_refreshing_parked(
                        &mut snapshot,
                        probe,
                        work_db,
                        &mut parked_ids,
                        &parked_scope,
                        stranded_blocked[idx..].iter(),
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
                        &stranded_blocked[idx],
                        &mut outcome,
                    )
                    .await;
                    trace.finish_item();
                }
                // Merge-queue rebounce: one batched GraphQL round trip, then
                // per-candidate checks. Refresh parked rows first so a row
                // parked during the open walk is not bounced.
                trace.enter(PassPhase::QueueRebounce, 0);
                if !clock.should_yield(false) {
                    parked_ids = parked_ids_for_pass(work_db, parked_scope.iter().copied());
                    run_merge_queue_rebounce_pass(
                        work_db,
                        publisher,
                        &in_review,
                        &blocked_ci,
                        Some(&parked_ids),
                        &mut outcome,
                    )
                    .await;
                }
                // Reviewer-fallback: tasks held in `active` (PendingReview)
                // while waiting for an AI reviewer pass that has since finished
                // or timed out. Timeout: 10 minutes.
                trace.enter(PassPhase::StalledReviewer, 0);
                let reviewer_stale_secs: u64 = 10 * 60;
                let stalled_candidates = match work_db.list_tasks_with_stalled_reviewer(reviewer_stale_secs) {
                    Ok(items) => items,
                    Err(err) => {
                        tracing::warn!(?err, "merge poller: failed to list stalled reviewer tasks");
                        Vec::new()
                    }
                };
                trace.set_items_total(stalled_candidates.len());
                for (task_id, product_id, pr_url, review_result_written) in &stalled_candidates {
                    if clock.should_yield(false) {
                        break;
                    }
                    trace.start_item(task_id);
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
                    sample_review_pool_gauges(work_db, &mut outcome);
                }
            }
            PostTerminalGroup::LateFamilies => {
                if let Some(handler) = completion_handler {
                    if clock.should_yield(true) {
                        continue;
                    }
                    // Only a pass that actually has time for this group
                    // advances family rotation, so a family is not charged
                    // for a pass that already spent its budget on the walk.
                    let family_turn = state.next_late_family_turn();
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
                        if clock.should_yield(true) {
                            break;
                        }
                        match phase {
                            LatePhase::PrRecheck => {
                                trace.enter(PassPhase::PrRecheck, rechecks.len());
                                for execution_id in &rechecks {
                                    if clock.should_yield(true) {
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
                                    if clock.should_yield(true) {
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
                                    if clock.should_yield(true) {
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
    trace.end();

    let observations = poll_observations(&probe_urls, &snapshot.results);
    (outcome, observations)
}

/// Classify each probed URL into the tier that should drive its adaptive
/// polling. Shared by the full sweep and the batched adaptive reconcile so
/// both feed [`PrPollSchedule`] through identical rules: a URL whose probe
/// failed, or which the snapshot no longer holds, yields `None` (no adaptive
/// slot — the full sweep re-probes it).
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
