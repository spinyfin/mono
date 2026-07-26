//! Off-detection-path conflict remediation.
//!
//! # Why this module exists
//!
//! The merge poller's job is **detection**: probe every tracked PR, notice
//! merges / conflicts / CI transitions, write the state change, publish the
//! event. `merge_poller::run_one_pass` is awaited one pass at a time on a
//! single task, so anything it awaits inline is time during which *every*
//! other PR's lifecycle goes unobserved.
//!
//! `e64dda13b` ("boss/engine: escalation-ladder harness — rung-1
//! engine-direct rebase on conflict_watch", PR #1968) hung remediation
//! directly off that detection path:
//! `sweep_one` → `conflict_watch::on_conflict_detected` →
//! `conflict_ladder::try_mechanical_rungs`, awaited inline. That ladder
//! leases a cube workspace, runs a real rebase, runs the rung-0 resolvers,
//! and pushes through the checkleft push-gate — minutes of work per
//! conflicting PR, serially, inside a sweep whose declared cadence is 60
//! seconds. One observed window had `boss_engine::merge_poller` emit **zero**
//! trace lines for 32 minutes while seven consecutive ladder runs executed
//! back to back; a PR that merged ten seconds after that pass took its probe
//! snapshot was not moved to `done` for another 33 minutes.
//!
//! So: detection records "conflict observed, remediation needed" and hands
//! the ladder to [`ConflictRemediationQueue`], which runs it on its own
//! tasks. The detection pass returns immediately.
//!
//! # What the queue guarantees
//!
//! - **Bounded concurrency.** Remediation leases cube workspaces, and cube
//!   capacity is shared with every dispatched worker. An unbounded
//!   `tokio::spawn`-per-conflict would stampede it the first time `main`
//!   moves under a dozen open PRs at once. At most
//!   [`MAX_CONCURRENT_CONFLICT_REMEDIATIONS`] ladder runs are in flight.
//! - **Deduplication.** A PR that already has a ladder run in flight is not
//!   enqueued again. Without this, every sweep (and every adaptive per-PR
//!   reconcile, which fires far more often than the full sweep) would pile
//!   another ladder run onto the same still-conflicting PR.
//! - **Back-off, but only where a retry is actually pending.** A job that
//!   deliberately leaves its attempt `pending` for a later tick to retry —
//!   [`LadderOutcome::MechanicalRungsUnavailable`], i.e. a rung-1 lease
//!   failure — holds its slot for [`DEFAULT_REMEDIATION_COOLDOWN`] so the
//!   retry doesn't become a storm. Every other exit
//!   ([`RemediationDisposition::Settled`]) releases the slot immediately: a
//!   *new* conflict on the same PR must never be declined because an
//!   unrelated, already-concluded remediation happened to finish seconds ago.
//! - **No lost signal.** Declining to enqueue is never a terminal decision:
//!   the `conflict_resolutions` attempt row stays `pending` with no
//!   `revision_task_id`, and `conflict_watch`'s blocked re-arm path treats
//!   exactly that shape as re-enqueueable, so the next detection tick
//!   re-enters this path. (Before PR #2367's review that re-entry did *not*
//!   exist: the re-arm path returned "active crz still in flight; no new
//!   dispatch" for a pending attempt with no revision, so a declined enqueue
//!   really was terminal and only the startup-only
//!   `reconcile_orphaned_conflict_ladder_attempts` ever recovered it.)
//! - **Nothing acts on a stale snapshot.** The candidate/attempt/probe a job
//!   is handed were captured at detection time; with a bound of 2 and
//!   multi-minute ladder runs, a job can sit on the semaphore for a long
//!   while. Before it leases anything — and again before it spawns a worker
//!   revision — it re-reads the attempt row and re-checks the PR's live open
//!   state, and aborts if either has moved on (see
//!   [`LadderRemediationJob::abort_reason`]).
//!
//! # Operational consequence
//!
//! A conflict is now *observed* strictly before it is *acted on*. With the
//! default bound of 2 and observed ladder durations of 1.5–8.75 minutes, a
//! conflict detected while the queue is saturated waits roughly one ladder
//! duration per two conflicts ahead of it. That lag is the price of never
//! again blinding merge detection for the length of a rebase.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Semaphore;

use crate::conflict_ladder::{self, LadderOutcome};
use crate::conflict_watch;
use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::merge_poller::PrLifecycleProbe;
use crate::work::{ConflictResolution, GhPrStateChecker, PendingMergeCheck, PrOpenState, PrStateChecker, WorkDb};

/// How many conflict remediations may run at once.
///
/// **Two.** The number is bounded by cube, not by CPU: every ladder run
/// leases a real workspace for the whole rung-0/rung-1 sequence (observed
/// 1.5–8.75 minutes each), and those leases come out of the same pool the
/// dispatcher hands to workers. One would be strictly serial — a single
/// slow rebase would again queue every other conflict behind it, which is
/// the failure mode this module exists to remove, just moved one hop off
/// the detection path. Anything much larger trades away worker capacity to
/// speed up a background repair that nothing is waiting on synchronously.
/// Two gives conflicts an independent lane each while holding at most two
/// remediation-owned leases at any instant, leaving the overwhelming
/// majority of cube capacity for dispatched work.
pub const MAX_CONCURRENT_CONFLICT_REMEDIATIONS: usize = 2;

/// How long a PR is held out of the queue after a ladder run that
/// **deliberately left its attempt `pending` for a later tick to retry** —
/// [`LadderOutcome::MechanicalRungsUnavailable`], the rung-1 lease failure.
/// Without it, every adaptive per-PR reconcile — 40 s apart on a hot PR —
/// would queue another full ladder run against a PR whose last one failed
/// seconds ago.
///
/// Deliberately **not** applied to any other exit. A run that concluded —
/// retired the conflict, halted for sign-off, spawned a worker revision, or
/// aborted because the attempt/PR moved on — has nothing outstanding, so
/// holding its PR for another three minutes only delays remediation of a
/// genuinely *new* conflict (a fresh attempt row, a new base SHA) that
/// happens to arrive inside the window. See [`RemediationDisposition`].
pub const DEFAULT_REMEDIATION_COOLDOWN: Duration = Duration::from_secs(180);

/// What [`ConflictRemediationQueue::try_enqueue`] decided. Reported by the
/// caller into the trace so "why did no ladder run for this conflict?" is
/// answerable without reading the queue's internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// A remediation task was spawned (it may still be waiting on a permit).
    Enqueued,
    /// A ladder run for this PR is already in flight; this tick adds nothing.
    AlreadyInFlight,
    /// A ladder run for this PR finished within the cooldown window.
    Cooldown,
}

/// What a finished job leaves behind, and therefore what the queue should do
/// with the PR's dedup slot.
///
/// The distinction that matters is **whether a retry is still owed**. Only a
/// job that walked away from a live attempt expecting a later tick to pick it
/// up needs the storm guard; everything else has concluded, and holding the
/// slot then just delays a genuinely new conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemediationDisposition {
    /// The job reached a conclusion: the conflict was retired, the resolution
    /// was halted for operator sign-off, a worker revision was spawned (or
    /// its creation failed and the attempt was abandoned), or the job aborted
    /// because the attempt/PR was no longer live. Nothing is waiting on
    /// another ladder run — release the slot immediately.
    Settled,
    /// The job deliberately left the attempt `pending` with no
    /// `revision_task_id` for a later detection tick to retry
    /// ([`LadderOutcome::MechanicalRungsUnavailable`]). This is the retry
    /// storm [`DEFAULT_REMEDIATION_COOLDOWN`] exists for, and the only
    /// outcome that pays it.
    RetryAfterCooldown,
}

/// The work one enqueued conflict actually performs. A trait rather than a
/// hardcoded call so tests can drive the queue's concurrency, dedup, and
/// off-path guarantees with a job that blocks on a signal instead of
/// standing up a cube double and a real rebase.
#[async_trait]
pub trait ConflictRemediationJob: Send + Sync {
    async fn run(
        &self,
        candidate: PendingMergeCheck,
        attempt: ConflictResolution,
        probe: PrLifecycleProbe,
    ) -> RemediationDisposition;
}

/// Production job: the escalation ladder, plus the worker-revision spawn the
/// ladder falls through to.
pub struct LadderRemediationJob {
    work_db: Arc<WorkDb>,
    publisher: Arc<dyn ExecutionPublisher>,
    cube_client: Arc<dyn CubeClient>,
    /// Checked immediately before the fall-through revision spawn.
    /// Deliberately the **live** checker rather than the detection pass's
    /// `StaticPrStateChecker(Open)` snapshot: minutes elapse between the
    /// probe that observed the conflict and this spawn, and the PR may have
    /// merged or closed in between. Spawning a conflict-resolution revision
    /// against a PR that is no longer open is precisely the "transition off a
    /// stale snapshot" mistake.
    pr_checker: Arc<dyn PrStateChecker>,
}

impl LadderRemediationJob {
    pub fn new(work_db: Arc<WorkDb>, publisher: Arc<dyn ExecutionPublisher>, cube_client: Arc<dyn CubeClient>) -> Self {
        Self {
            work_db,
            publisher,
            cube_client,
            pr_checker: Arc::new(GhPrStateChecker),
        }
    }

    /// Swap the live `gh` checker for a double. Test seam only — production
    /// must re-check against real GitHub state, which is the whole point of
    /// [`Self::abort_reason`].
    #[cfg(test)]
    pub fn with_pr_checker(mut self, pr_checker: Arc<dyn PrStateChecker>) -> Self {
        self.pr_checker = pr_checker;
        self
    }

    /// `Some(reason)` when the snapshot this job was enqueued with no longer
    /// describes reality and it must not act.
    ///
    /// `candidate` / `attempt` / `probe` are captured at **detection** time.
    /// With a concurrency bound of 2 and observed ladder durations of
    /// 1.5–8.75 minutes, a job can sit on the semaphore for many minutes
    /// before it starts, and the ladder itself then runs for minutes more. In
    /// that window the merge poller can retire the attempt (`on_resolved`),
    /// supersede it (`supersede_if_stale`), or observe the PR merged/closed.
    /// Acting anyway means leasing a cube workspace and pushing an
    /// engine-direct rebase at a PR that is already done with — the exact
    /// "transition off a stale snapshot" mistake this PR exists to stop.
    ///
    /// Checked twice: once before the mechanical rungs (so no lease is ever
    /// taken for a dead attempt) and once before the fall-through worker
    /// spawn (the ladder itself is the longer of the two windows).
    ///
    /// Deliberately conservative on errors: a transient DB or `gh` failure
    /// returns `None` (proceed). Remediation that is merely *possibly* stale
    /// is better than remediation dropped on a blip — the ladder's own guards
    /// (`insert_conflict_resolution`'s UNIQUE key, `create_revision`'s
    /// `assert_parent_revisable` gate) remain in force behind this one.
    ///
    /// Note there is no explicit "is the PR still conflicting?" probe: when
    /// the poller observes a conflict clear it calls `on_resolved`, which
    /// marks this very attempt `succeeded` — so the attempt-status check
    /// below already covers that transition, without a second GitHub round
    /// trip on a path that is bounded by cube, not by quota.
    fn abort_reason(&self, candidate: &PendingMergeCheck, attempt: &ConflictResolution) -> Option<String> {
        match self.work_db.get_conflict_resolution(&attempt.id) {
            Ok(Some(current)) => {
                if !matches!(current.status.as_str(), "pending" | "running") {
                    return Some(format!("attempt is now `{}`, no longer live", current.status));
                }
                if current.revision_task_id.is_some() {
                    return Some("attempt already has a revision task; another vehicle owns the fix".to_owned());
                }
            }
            Ok(None) => return Some("attempt row no longer exists".to_owned()),
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?err,
                    "conflict_remediation: could not re-read attempt for the liveness re-check; proceeding",
                );
            }
        }
        match self.pr_checker.check(&candidate.pr_url) {
            Ok(PrOpenState::Open) => None,
            Ok(other) => Some(format!("PR is no longer open ({other:?})")),
            Err(err) => {
                tracing::warn!(
                    pr_url = %candidate.pr_url,
                    error = %format!("{err:#}"),
                    "conflict_remediation: could not re-check PR open state; proceeding",
                );
                None
            }
        }
    }
}

#[async_trait]
impl ConflictRemediationJob for LadderRemediationJob {
    async fn run(
        &self,
        candidate: PendingMergeCheck,
        attempt: ConflictResolution,
        probe: PrLifecycleProbe,
    ) -> RemediationDisposition {
        let started = Instant::now();
        // Before the first thing that costs anything (`try_mechanical_rungs`
        // leases a cube workspace): is this still worth doing?
        if let Some(reason) = self.abort_reason(&candidate, &attempt) {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                attempt_id = %attempt.id,
                reason = %reason,
                "conflict_remediation: queued remediation is stale by the time its turn came; \
                 aborting before the mechanical rungs (no cube lease, no worker spawned)",
            );
            return RemediationDisposition::Settled;
        }
        let outcome = conflict_ladder::try_mechanical_rungs(
            self.work_db.as_ref(),
            self.publisher.as_ref(),
            self.cube_client.as_ref(),
            &candidate,
            &attempt,
        )
        .await;
        match outcome {
            LadderOutcome::Retired => {
                tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    attempt_id = %attempt.id,
                    elapsed_ms = started.elapsed().as_millis(),
                    "conflict_remediation: conflict auto-resolved by engine-direct rebase (rung 1); no worker spawned",
                );
                RemediationDisposition::Settled
            }
            LadderOutcome::HaltedForSignoff => {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    attempt_id = %attempt.id,
                    elapsed_ms = started.elapsed().as_millis(),
                    "conflict_remediation: mechanical rung's resolution rejected by the deletion tripwire; \
                     halted for operator sign-off, no worker spawned",
                );
                RemediationDisposition::Settled
            }
            LadderOutcome::MechanicalRungsUnavailable => {
                // Same contract as the inline path: spawn nothing, leave the
                // attempt `pending` with no `revision_task_id` so a later
                // detection tick re-enters the ladder from scratch. This is
                // the one outcome that owes a retry, so it is the one that
                // holds its slot for the cooldown.
                tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    attempt_id = %attempt.id,
                    elapsed_ms = started.elapsed().as_millis(),
                    "conflict_remediation: mechanical rungs unavailable this attempt (rung-1 lease); \
                     no worker spawned, ladder will retry on a later tick",
                );
                RemediationDisposition::RetryAfterCooldown
            }
            LadderOutcome::FellThrough {
                residual_conflict_files,
                ..
            } => {
                // Second checkpoint. The ladder just held a cube lease for
                // minutes; `on_resolved` may have retired this attempt, or
                // the PR may have merged, while it ran. `create_revision`'s
                // own gate would refuse a merged parent, but it would do so
                // by *abandoning* an attempt that another path has already
                // legitimately closed — better to notice here and leave the
                // row exactly as we found it.
                if let Some(reason) = self.abort_reason(&candidate, &attempt) {
                    tracing::info!(
                        work_item_id = %candidate.work_item_id,
                        pr_url = %candidate.pr_url,
                        attempt_id = %attempt.id,
                        reason = %reason,
                        elapsed_ms = started.elapsed().as_millis(),
                        "conflict_remediation: mechanical rungs fell through, but the attempt went stale \
                         while they ran; not spawning a worker revision",
                    );
                    return RemediationDisposition::Settled;
                }
                let use_small_agent_profile = conflict_ladder::rung2_eligible(residual_conflict_files);
                let spawned = conflict_watch::spawn_conflict_revision_after_ladder(
                    self.work_db.as_ref(),
                    self.publisher.as_ref(),
                    self.pr_checker.as_ref(),
                    &candidate,
                    &probe,
                    &attempt,
                    use_small_agent_profile,
                )
                .await;
                tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    attempt_id = %attempt.id,
                    spawned,
                    use_small_agent_profile,
                    elapsed_ms = started.elapsed().as_millis(),
                    "conflict_remediation: mechanical rungs fell through; worker revision spawn attempted",
                );
                // Either a revision now owns the fix, or the attempt was
                // abandoned (`revision_create_failed`) and the parent rests
                // `blocked` for a human. Neither is waiting on this queue.
                RemediationDisposition::Settled
            }
        }
    }
}

/// Per-PR slot state backing the dedup guarantee.
#[derive(Debug, Clone, Copy)]
enum SlotState {
    InFlight,
    CompletedAt(Instant),
}

/// Bounded, deduplicating runner for conflict remediation. Cheap to clone-
/// share via `Arc`; `try_enqueue` takes `&self` and never blocks.
pub struct ConflictRemediationQueue {
    job: Arc<dyn ConflictRemediationJob>,
    permits: Arc<Semaphore>,
    slots: Arc<Mutex<HashMap<String, SlotState>>>,
    cooldown: Duration,
}

impl ConflictRemediationQueue {
    /// Queue with the production bound and cooldown.
    pub fn new(job: Arc<dyn ConflictRemediationJob>) -> Self {
        Self::with_limits(job, MAX_CONCURRENT_CONFLICT_REMEDIATIONS, DEFAULT_REMEDIATION_COOLDOWN)
    }

    /// Queue with explicit limits — tests use this to make the bound and the
    /// cooldown observable in a few milliseconds instead of minutes.
    pub fn with_limits(job: Arc<dyn ConflictRemediationJob>, max_concurrent: usize, cooldown: Duration) -> Self {
        Self {
            job,
            permits: Arc::new(Semaphore::new(max_concurrent.max(1))),
            slots: Arc::new(Mutex::new(HashMap::new())),
            cooldown,
        }
    }

    /// Hand one freshly-detected conflict to the background remediator.
    ///
    /// Returns without awaiting any part of the ladder — that is the whole
    /// point. Never blocks: when the concurrency bound is saturated the
    /// spawned task waits for a permit, not the caller.
    pub fn try_enqueue(
        &self,
        candidate: &PendingMergeCheck,
        attempt: &ConflictResolution,
        probe: &PrLifecycleProbe,
    ) -> EnqueueOutcome {
        let key = candidate.pr_url.clone();
        {
            let mut slots = match self.slots.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            // Opportunistic prune so a long-lived engine doesn't accumulate
            // one map entry per PR it has ever remediated.
            let cooldown = self.cooldown;
            slots.retain(|_, state| match state {
                SlotState::InFlight => true,
                SlotState::CompletedAt(at) => at.elapsed() < cooldown,
            });
            match slots.get(&key) {
                Some(SlotState::InFlight) => return EnqueueOutcome::AlreadyInFlight,
                // Any surviving `CompletedAt` is inside the window by the
                // `retain` above.
                Some(SlotState::CompletedAt(_)) => return EnqueueOutcome::Cooldown,
                None => {}
            }
            slots.insert(key.clone(), SlotState::InFlight);
        }

        let job = self.job.clone();
        let permits = self.permits.clone();
        let slots = self.slots.clone();
        let candidate = candidate.clone();
        let attempt = attempt.clone();
        let probe = probe.clone();
        tokio::spawn(async move {
            // The guard releases the slot on *every* exit path, including a
            // panic inside the job or the runtime dropping the task — a
            // leaked `InFlight` entry would silently wedge remediation for
            // that PR for the life of the process.
            let mut guard = SlotGuard {
                slots,
                key: key.clone(),
                // Only meaningful once `started` is set. Left at
                // `RetryAfterCooldown` so an unwind or a mid-run cancellation
                // — the cases where we have no idea what state the ladder
                // left behind — backs off rather than re-running immediately.
                disposition: RemediationDisposition::RetryAfterCooldown,
                started: false,
            };
            let _permit = match permits.acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    // `started` is still false, so the guard removes the slot
                    // outright: no ladder attempt happened, so there is
                    // nothing to back off from and the next detection tick
                    // should be free to try again at once.
                    tracing::warn!(pr_url = %key, "conflict_remediation: semaphore closed; dropping remediation");
                    return;
                }
            };
            guard.started = true;
            guard.disposition = job.run(candidate, attempt, probe).await;
        });
        EnqueueOutcome::Enqueued
    }

    /// Number of PRs currently holding a slot (in flight or cooling down).
    /// Test/diagnostic accessor.
    pub fn tracked_len(&self) -> usize {
        match self.slots.lock() {
            Ok(g) => g.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Close the concurrency semaphore so every task still waiting for a
    /// permit fails its `acquire_owned`. Test seam for the
    /// "guard dropped without the job ever starting" path.
    #[cfg(test)]
    fn close_permits(&self) {
        self.permits.close();
    }
}

/// Releases a PR's dedup slot when its remediation task ends, however it
/// ends.
///
/// The slot is claimed by `try_enqueue` *before* the task waits on the
/// concurrency semaphore, so a PR queued behind the bound is `InFlight` from
/// the moment it is enqueued — which is what makes dedup work at all (a PR
/// waiting for a permit must not be enqueued a second time by the next
/// sweep). The cost of claiming that early is that the guard has to
/// distinguish "the job ran" from "the task died while still queued":
///
/// - **Never started** (`started == false`: the semaphore closed, or the
///   task was cancelled while waiting for a permit). No ladder ran, nothing
///   was leased, nothing is owed — remove the slot so the next detection tick
///   can enqueue immediately.
/// - **Ran and settled.** Same: remove the slot. Whatever happened, no retry
///   is pending on this queue.
/// - **Ran and owes a retry** ([`RemediationDisposition::RetryAfterCooldown`],
///   also the default if the job panicked or was dropped mid-run) — hold the
///   slot for the cooldown so the retry doesn't become a storm.
struct SlotGuard {
    slots: Arc<Mutex<HashMap<String, SlotState>>>,
    key: String,
    disposition: RemediationDisposition,
    started: bool,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut slots = match self.slots.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match (self.started, self.disposition) {
            (false, _) | (true, RemediationDisposition::Settled) => {
                slots.remove(&self.key);
            }
            (true, RemediationDisposition::RetryAfterCooldown) => {
                slots.insert(self.key.clone(), SlotState::CompletedAt(Instant::now()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState, PrReviewState};

    fn candidate(pr_url: &str) -> PendingMergeCheck {
        PendingMergeCheck {
            work_item_id: format!("task-for-{pr_url}"),
            product_id: "prod-1".to_owned(),
            pr_url: pr_url.to_owned(),
        }
    }

    fn attempt(id: &str, pr_url: &str) -> ConflictResolution {
        ConflictResolution::builder()
            .id(id)
            .product_id("prod-1")
            .work_item_id(format!("task-for-{pr_url}"))
            .pr_url(pr_url)
            .pr_number(1_i64)
            .head_branch("feature")
            .base_branch("main")
            .status("pending")
            .created_at("2026-07-25T00:00:00Z")
            .build()
    }

    fn probe(pr_url: &str) -> PrLifecycleProbe {
        PrLifecycleProbe::builder()
            .url(pr_url.to_owned())
            .state(PrLifecycleState::Open(OpenPrStatus::conflict_only()))
            .labels(Vec::new())
            .review(PrReviewState::Unknown)
            .build()
    }

    /// Job that parks until the test releases it, standing in for the
    /// escalation ladder's cube lease + rebase + push. Records what it was
    /// handed and the peak number of concurrent runs.
    struct ParkingJob {
        seen: Arc<Mutex<Vec<String>>>,
        running: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
        release: Arc<Semaphore>,
        disposition: RemediationDisposition,
    }

    /// A [`ParkingJob`] plus the handles the test observes it through.
    struct Parked {
        job: Arc<ParkingJob>,
        seen: Arc<Mutex<Vec<String>>>,
        peak: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
        release: Arc<Semaphore>,
    }

    impl ParkingJob {
        /// Parked job that reports it left a retry outstanding — the only
        /// disposition that pays the cooldown.
        fn parked_owing_retry() -> Parked {
            Self::parked_with(RemediationDisposition::RetryAfterCooldown)
        }

        /// Parked job that reports it concluded, i.e. nothing is waiting on
        /// the queue for this PR.
        fn parked_settled() -> Parked {
            Self::parked_with(RemediationDisposition::Settled)
        }

        fn parked_with(disposition: RemediationDisposition) -> Parked {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let running = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let finished = Arc::new(AtomicUsize::new(0));
            // Zero permits: every job parks until the test hands one out.
            let release = Arc::new(Semaphore::new(0));
            let job = Arc::new(Self {
                seen: seen.clone(),
                running,
                peak: peak.clone(),
                finished: finished.clone(),
                release: release.clone(),
                disposition,
            });
            Parked {
                job,
                seen,
                peak,
                finished,
                release,
            }
        }
    }

    #[async_trait]
    impl ConflictRemediationJob for ParkingJob {
        async fn run(
            &self,
            candidate: PendingMergeCheck,
            _attempt: ConflictResolution,
            _probe: PrLifecycleProbe,
        ) -> RemediationDisposition {
            match self.seen.lock() {
                Ok(mut g) => g.push(candidate.pr_url.clone()),
                Err(p) => p.into_inner().push(candidate.pr_url.clone()),
            }
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            if let Ok(permit) = self.release.acquire().await {
                permit.forget();
            }
            self.running.fetch_sub(1, Ordering::SeqCst);
            self.finished.fetch_add(1, Ordering::SeqCst);
            self.disposition
        }
    }

    /// Spin until `pred` holds or the deadline passes. Cheaper and less
    /// flaky than a fixed sleep for "the spawned task has got going yet".
    async fn wait_until(mut pred: impl FnMut() -> bool) -> bool {
        for _ in 0..2000 {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        pred()
    }

    #[tokio::test]
    async fn try_enqueue_returns_without_waiting_for_the_job() {
        let Parked {
            job,
            seen,
            finished,
            release,
            ..
        } = ParkingJob::parked_settled();
        let queue = ConflictRemediationQueue::new(job);
        let pr = "https://github.com/foo/bar/pull/1";

        let started = Instant::now();
        let outcome = queue.try_enqueue(&candidate(pr), &attempt("att-1", pr), &probe(pr));
        let elapsed = started.elapsed();

        assert_eq!(outcome, EnqueueOutcome::Enqueued);
        assert!(
            elapsed < Duration::from_millis(200),
            "enqueue must not await the job; took {elapsed:?}",
        );
        assert!(
            wait_until(|| !seen.lock().unwrap().is_empty()).await,
            "the job must actually run in the background",
        );
        assert_eq!(finished.load(Ordering::SeqCst), 0, "job is still parked");
        release.add_permits(1);
        assert!(wait_until(|| finished.load(Ordering::SeqCst) == 1).await);
    }

    #[tokio::test]
    async fn a_second_enqueue_for_an_in_flight_pr_is_declined() {
        let Parked { job, seen, release, .. } = ParkingJob::parked_settled();
        let queue = ConflictRemediationQueue::new(job);
        let pr = "https://github.com/foo/bar/pull/1";

        assert_eq!(
            queue.try_enqueue(&candidate(pr), &attempt("att-1", pr), &probe(pr)),
            EnqueueOutcome::Enqueued,
        );
        assert!(wait_until(|| !seen.lock().unwrap().is_empty()).await);
        assert_eq!(
            queue.try_enqueue(&candidate(pr), &attempt("att-2", pr), &probe(pr)),
            EnqueueOutcome::AlreadyInFlight,
            "a PR with a ladder run in flight must not get a second one",
        );
        release.add_permits(2);
        // Give the (non-existent) second job every chance to show up.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "exactly one ladder run for this PR");
    }

    #[tokio::test]
    async fn a_run_that_owes_a_retry_is_declined_until_the_cooldown_elapses() {
        let Parked {
            job,
            seen,
            finished,
            release,
            ..
        } = ParkingJob::parked_owing_retry();
        let queue = ConflictRemediationQueue::with_limits(job, 2, Duration::from_millis(150));
        let pr = "https://github.com/foo/bar/pull/1";

        assert_eq!(
            queue.try_enqueue(&candidate(pr), &attempt("att-1", pr), &probe(pr)),
            EnqueueOutcome::Enqueued,
        );
        release.add_permits(1);
        assert!(wait_until(|| finished.load(Ordering::SeqCst) == 1).await);

        assert_eq!(
            queue.try_enqueue(&candidate(pr), &attempt("att-2", pr), &probe(pr)),
            EnqueueOutcome::Cooldown,
            "a PR whose ladder run just failed its rung-1 lease must not be immediately re-run",
        );

        tokio::time::sleep(Duration::from_millis(200)).await;
        release.add_permits(1);
        assert_eq!(
            queue.try_enqueue(&candidate(pr), &attempt("att-3", pr), &probe(pr)),
            EnqueueOutcome::Enqueued,
            "past the cooldown the PR is eligible again — a declined enqueue is never terminal",
        );
        assert!(wait_until(|| seen.lock().unwrap().len() == 2).await);
    }

    /// The cooldown must not outlive the failure mode it guards. A run that
    /// *concluded* (retired the conflict, spawned a revision, aborted as
    /// stale) leaves nothing outstanding, so a genuinely new conflict on the
    /// same PR — new base SHA, new attempt row — has to be actionable
    /// immediately. Cooling that down strands the new attempt: detection has
    /// already flipped the parent to `blocked: merge_conflict`, and with no
    /// job spawned nothing else is coming.
    #[tokio::test]
    async fn a_settled_run_leaves_its_pr_immediately_eligible_again() {
        let Parked {
            job,
            seen,
            finished,
            release,
            ..
        } = ParkingJob::parked_settled();
        // A cooldown far longer than the test could ever wait out: if the
        // second enqueue succeeds it can only be because a settled run
        // released its slot rather than starting a cooldown.
        let queue = ConflictRemediationQueue::with_limits(job, 2, Duration::from_secs(3600));
        let pr = "https://github.com/foo/bar/pull/1";

        assert_eq!(
            queue.try_enqueue(&candidate(pr), &attempt("att-1", pr), &probe(pr)),
            EnqueueOutcome::Enqueued,
        );
        release.add_permits(1);
        assert!(wait_until(|| finished.load(Ordering::SeqCst) == 1).await);
        assert!(
            wait_until(|| queue.tracked_len() == 0).await,
            "a settled run must release its dedup slot, not park it in a cooldown",
        );

        assert_eq!(
            queue.try_enqueue(&candidate(pr), &attempt("att-2", pr), &probe(pr)),
            EnqueueOutcome::Enqueued,
            "a fresh conflict on a PR whose previous remediation concluded must not be declined",
        );
        release.add_permits(1);
        assert!(wait_until(|| seen.lock().unwrap().len() == 2).await);
    }

    /// A task that never got a permit never leased anything and never ran a
    /// rung, so it has nothing to back off from. Dropping it must clear the
    /// slot outright rather than starting a cooldown on a PR no ladder ever
    /// touched.
    #[tokio::test]
    async fn a_job_that_never_started_does_not_cool_down_its_pr() {
        let Parked { job, seen, release, .. } = ParkingJob::parked_settled();
        // Bound of 1: the second PR sits on the semaphore behind the first.
        let queue = ConflictRemediationQueue::with_limits(job, 1, Duration::from_secs(3600));
        let running_pr = "https://github.com/foo/bar/pull/1";
        let queued_pr = "https://github.com/foo/bar/pull/2";

        assert_eq!(
            queue.try_enqueue(
                &candidate(running_pr),
                &attempt("att-1", running_pr),
                &probe(running_pr)
            ),
            EnqueueOutcome::Enqueued,
        );
        assert!(wait_until(|| !seen.lock().unwrap().is_empty()).await);
        assert_eq!(
            queue.try_enqueue(&candidate(queued_pr), &attempt("att-2", queued_pr), &probe(queued_pr)),
            EnqueueOutcome::Enqueued,
        );

        // Kill the queued task before it ever acquires a permit.
        queue.close_permits();
        assert!(
            wait_until(|| queue.tracked_len() == 1).await,
            "the queued PR's slot must be removed outright, leaving only the running PR tracked; \
             tracked_len was {}",
            queue.tracked_len(),
        );
        assert_eq!(
            queue.try_enqueue(&candidate(queued_pr), &attempt("att-3", queued_pr), &probe(queued_pr)),
            EnqueueOutcome::Enqueued,
            "a PR whose job never started must not be parked in a cooldown",
        );

        release.add_permits(2);
    }

    #[tokio::test]
    async fn concurrency_is_capped_at_the_configured_bound() {
        let Parked {
            job,
            seen,
            peak,
            release,
            ..
        } = ParkingJob::parked_settled();
        let queue = ConflictRemediationQueue::with_limits(job, 2, Duration::from_secs(60));

        for n in 1..=5 {
            let pr = format!("https://github.com/foo/bar/pull/{n}");
            assert_eq!(
                queue.try_enqueue(&candidate(&pr), &attempt(&format!("att-{n}"), &pr), &probe(&pr)),
                EnqueueOutcome::Enqueued,
            );
        }
        // Two get going; the other three sit on the semaphore.
        assert!(wait_until(|| seen.lock().unwrap().len() == 2).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "cube-leasing remediation must never exceed its concurrency bound",
        );

        release.add_permits(5);
        assert!(wait_until(|| seen.lock().unwrap().len() == 5).await);
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "the bound holds for the whole drain, not just the first wave",
        );
    }

    #[tokio::test]
    async fn a_panicking_job_does_not_wedge_its_pr_forever() {
        struct PanickingJob;
        #[async_trait]
        impl ConflictRemediationJob for PanickingJob {
            async fn run(
                &self,
                _: PendingMergeCheck,
                _: ConflictResolution,
                _: PrLifecycleProbe,
            ) -> RemediationDisposition {
                panic!("ladder blew up");
            }
        }
        let queue = ConflictRemediationQueue::with_limits(Arc::new(PanickingJob), 2, Duration::from_millis(1));
        let pr = "https://github.com/foo/bar/pull/1";
        assert_eq!(
            queue.try_enqueue(&candidate(pr), &attempt("att-1", pr), &probe(pr)),
            EnqueueOutcome::Enqueued,
        );
        // The slot guard runs on unwind, so the PR becomes eligible again
        // once the (1 ms) cooldown lapses rather than staying `InFlight`.
        assert!(
            wait_until(
                || queue.try_enqueue(&candidate(pr), &attempt("att-2", pr), &probe(pr)) == EnqueueOutcome::Enqueued
            )
            .await,
            "a panicking remediation must not permanently block its PR",
        );
    }

    /// The queue-wait race: a job carries the candidate/attempt/probe it was
    /// handed at **detection** time, and with a bound of 2 and multi-minute
    /// ladder runs it can sit on the semaphore long enough for the merge
    /// poller to retire the attempt or observe the PR merged. Acting anyway
    /// means leasing a cube workspace and pushing an engine-direct rebase at
    /// a PR nobody is waiting on. See [`LadderRemediationJob::abort_reason`].
    mod staleness {
        use std::sync::atomic::AtomicUsize;

        use super::*;
        use crate::coordinator::CubeRepoHandle;
        use crate::test_support::{RecordingPublisher, create_test_chore_manual, create_test_product_with_repo};
        use crate::work::{ConflictResolutionInsertInput, StaticPrStateChecker, WorkItemPatch};

        /// Cube double that records every `ensure_repo` origin the ladder
        /// asks for. `ensure_repo` is the ladder's **first** cube call, so
        /// "this repo never appears" is a faithful proxy for "no workspace
        /// was ever leased for this PR".
        ///
        /// It then fails, which the ladder classifies as `FellThrough` — the
        /// arm whose *second* staleness checkpoint (before the worker-revision
        /// spawn) these tests also need to reach.
        struct RecordingCube {
            origins: Arc<Mutex<Vec<String>>>,
            /// Fired inside `ensure_repo`, i.e. while the ladder is running.
            /// The seam for "the poller retired the attempt mid-ladder".
            during_ladder: Box<dyn Fn() + Send + Sync>,
        }

        impl RecordingCube {
            fn new() -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
                Self::with_hook(Box::new(|| {}))
            }

            fn with_hook(during_ladder: Box<dyn Fn() + Send + Sync>) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
                let origins = Arc::new(Mutex::new(Vec::new()));
                (
                    Arc::new(Self {
                        origins: origins.clone(),
                        during_ladder,
                    }),
                    origins,
                )
            }

            fn leased(origins: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
                match origins.lock() {
                    Ok(g) => g.clone(),
                    Err(p) => p.into_inner().clone(),
                }
            }
        }

        crate::stub_cube_client! { RecordingCube {
            async fn ensure_repo(&self, origin: &str) -> ::anyhow::Result<CubeRepoHandle> {
                match self.origins.lock() {
                    Ok(mut g) => g.push(origin.to_owned()),
                    Err(p) => p.into_inner().push(origin.to_owned()),
                }
                (self.during_ladder)();
                ::core::result::Result::Err(::anyhow::anyhow!("no cube in tests"))
            }
        } }

        /// Job wrapper that parks before delegating — stands in for a task
        /// that reached the front of the queue only after a long wait.
        struct Gated {
            inner: Arc<dyn ConflictRemediationJob>,
            entered: Arc<AtomicUsize>,
            gate: Arc<Semaphore>,
        }

        #[async_trait]
        impl ConflictRemediationJob for Gated {
            async fn run(
                &self,
                candidate: PendingMergeCheck,
                attempt: ConflictResolution,
                probe: PrLifecycleProbe,
            ) -> RemediationDisposition {
                self.entered.fetch_add(1, Ordering::SeqCst);
                if let Ok(permit) = self.gate.acquire().await {
                    permit.forget();
                }
                self.inner.run(candidate, attempt, probe).await
            }
        }

        struct Fixture {
            candidate: PendingMergeCheck,
            attempt: ConflictResolution,
            repo: String,
        }

        /// One `in_review` chore with a conflicting PR and the `pending`
        /// attempt row detection would have written for it.
        fn fixture(db: &Arc<WorkDb>, name: &str, pr_url: &str, pr_number: i64) -> Fixture {
            let repo = format!("git@github.com:foo/{name}.git");
            let product = create_test_product_with_repo(db, &format!("Product-{name}"), Some(&repo));
            let chore = create_test_chore_manual(db, product.id.clone(), name);
            db.update_work_item(
                &chore.id,
                WorkItemPatch {
                    status: Some("in_review".into()),
                    pr_url: Some(pr_url.into()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
            let attempt = db
                .insert_conflict_resolution(ConflictResolutionInsertInput {
                    product_id: product.id.clone(),
                    work_item_id: chore.id.clone(),
                    pr_url: pr_url.to_owned(),
                    pr_number,
                    head_branch: "feature".to_owned(),
                    base_branch: "main".to_owned(),
                    base_sha_at_trigger: Some("base-1".to_owned()),
                    head_sha_before: Some("head-1".to_owned()),
                })
                .unwrap()
                .expect("fresh attempt row");
            Fixture {
                candidate: PendingMergeCheck {
                    work_item_id: chore.id,
                    product_id: product.id,
                    pr_url: pr_url.to_owned(),
                },
                attempt,
                repo,
            }
        }

        fn memory_db() -> Arc<WorkDb> {
            Arc::new(WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap())
        }

        fn ladder_job(db: &Arc<WorkDb>, cube: Arc<dyn CubeClient>, pr_state: PrOpenState) -> Arc<LadderRemediationJob> {
            Arc::new(
                LadderRemediationJob::new(db.clone(), Arc::new(RecordingPublisher::default()), cube)
                    .with_pr_checker(Arc::new(StaticPrStateChecker(pr_state))),
            )
        }

        fn revision_task_id(db: &WorkDb, attempt_id: &str) -> Option<String> {
            db.get_conflict_resolution(attempt_id)
                .unwrap()
                .expect("attempt row still exists")
                .revision_task_id
        }

        /// The reviewer's scenario end to end: two conflicts, a bound of one,
        /// and the queued one's attempt retired by the poller (`on_resolved`)
        /// while it waits for a permit. It must lease nothing and spawn
        /// nothing when its turn finally comes.
        #[tokio::test]
        async fn a_job_retired_while_it_waited_for_a_permit_leases_and_spawns_nothing() {
            let db = memory_db();
            let blocker = fixture(&db, "blocker", "https://github.com/foo/bar/pull/1", 1);
            let queued = fixture(&db, "queued", "https://github.com/foo/bar/pull/2", 2);

            let (cube, origins) = RecordingCube::new();
            let gate = Arc::new(Semaphore::new(0));
            let entered = Arc::new(AtomicUsize::new(0));
            let job = Arc::new(Gated {
                inner: ladder_job(&db, cube, PrOpenState::Open),
                entered: entered.clone(),
                gate: gate.clone(),
            });
            // Bound of one: `queued` genuinely sits on the semaphore behind
            // `blocker`, which is the window this test is about.
            let queue = ConflictRemediationQueue::with_limits(job, 1, Duration::from_millis(1));

            assert_eq!(
                queue.try_enqueue(&blocker.candidate, &blocker.attempt, &probe(&blocker.candidate.pr_url)),
                EnqueueOutcome::Enqueued,
            );
            assert!(
                wait_until(|| entered.load(Ordering::SeqCst) == 1).await,
                "the blocker must hold the only permit before the second enqueue",
            );
            assert_eq!(
                queue.try_enqueue(&queued.candidate, &queued.attempt, &probe(&queued.candidate.pr_url)),
                EnqueueOutcome::Enqueued,
            );

            // While it waits: the merge poller sees the PR go clean and
            // retires the attempt.
            db.mark_conflict_resolution_succeeded(&queued.attempt.id, Some("head-2"))
                .unwrap()
                .expect("attempt retired");

            gate.add_permits(2);
            assert!(
                wait_until(|| entered.load(Ordering::SeqCst) == 2).await,
                "the queued job must eventually get its permit",
            );
            assert!(
                wait_until(|| RecordingCube::leased(&origins).contains(&blocker.repo)).await,
                "sanity: the blocker's ladder really did run and reach cube",
            );
            // Give the queued job every chance to misbehave before asserting
            // the negative.
            tokio::time::sleep(Duration::from_millis(50)).await;

            assert!(
                !RecordingCube::leased(&origins).contains(&queued.repo),
                "a retired attempt must not lease a cube workspace when its turn comes; leased: {:?}",
                RecordingCube::leased(&origins),
            );
            assert_eq!(
                revision_task_id(&db, &queued.attempt.id),
                None,
                "a retired attempt must not get a worker revision spawned against it",
            );
        }

        /// Same guard, driven at the level of the job itself: the PR merged
        /// between detection and this job's turn.
        #[tokio::test]
        async fn a_merged_pr_aborts_before_the_mechanical_rungs() {
            let db = memory_db();
            let f = fixture(&db, "merged-meanwhile", "https://github.com/foo/bar/pull/3", 3);
            let (cube, origins) = RecordingCube::new();
            let job = ladder_job(&db, cube, PrOpenState::Merged);

            let disposition = job
                .run(f.candidate.clone(), f.attempt.clone(), probe(&f.candidate.pr_url))
                .await;

            assert_eq!(
                RecordingCube::leased(&origins),
                Vec::<String>::new(),
                "a PR that merged while the job was queued must not be leased for",
            );
            assert_eq!(revision_task_id(&db, &f.attempt.id), None);
            assert_eq!(
                disposition,
                RemediationDisposition::Settled,
                "an aborted job owes no retry, so it must not hold its PR in a cooldown",
            );
        }

        /// The second checkpoint. The ladder holds a cube lease for minutes;
        /// the attempt can be retired *while it runs*, and the fall-through
        /// must not then spawn a worker revision against it.
        #[tokio::test]
        async fn an_attempt_retired_during_the_ladder_does_not_get_a_revision() {
            let db = memory_db();
            let f = fixture(&db, "retired-mid-ladder", "https://github.com/foo/bar/pull/4", 4);
            let retire_db = db.clone();
            let attempt_id = f.attempt.id.clone();
            let (cube, origins) = RecordingCube::with_hook(Box::new(move || {
                retire_db
                    .mark_conflict_resolution_succeeded(&attempt_id, Some("head-2"))
                    .unwrap()
                    .expect("attempt retired mid-ladder");
            }));
            let job = ladder_job(&db, cube, PrOpenState::Open);

            let disposition = job
                .run(f.candidate.clone(), f.attempt.clone(), probe(&f.candidate.pr_url))
                .await;

            assert!(
                RecordingCube::leased(&origins).contains(&f.repo),
                "precondition: the ladder ran (and only then was the attempt retired)",
            );
            assert_eq!(
                revision_task_id(&db, &f.attempt.id),
                None,
                "an attempt retired while the ladder ran must not get a worker revision after it",
            );
            assert_eq!(disposition, RemediationDisposition::Settled);
        }
    }
}
