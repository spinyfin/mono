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
//! - **Deduplication.** A PR that already has a ladder run in flight, or that
//!   finished one within [`DEFAULT_REMEDIATION_COOLDOWN`], is not enqueued
//!   again. Without this, every sweep (and every adaptive per-PR reconcile,
//!   which fires far more often than the full sweep) would pile another
//!   ladder run onto the same still-conflicting PR.
//! - **No lost signal.** Declining to enqueue is never a terminal decision:
//!   the `conflict_resolutions` attempt row stays `pending` with no
//!   `revision_task_id`, so the next detection tick re-enters this path —
//!   exactly the retry contract the pre-existing
//!   [`LadderOutcome::MechanicalRungsUnavailable`] path already relied on.
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
use crate::work::{ConflictResolution, GhPrStateChecker, PendingMergeCheck, PrStateChecker, WorkDb};

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

/// How long after a ladder run finishes before the same PR may be enqueued
/// again. Guards the retry loop for the outcomes that deliberately leave the
/// attempt `pending` (rung-1 lease failure, transport errors): without it,
/// every adaptive per-PR reconcile — 40 s apart on a hot PR — would queue
/// another full ladder run against a PR whose last one failed seconds ago.
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

/// The work one enqueued conflict actually performs. A trait rather than a
/// hardcoded call so tests can drive the queue's concurrency, dedup, and
/// off-path guarantees with a job that blocks on a signal instead of
/// standing up a cube double and a real rebase.
#[async_trait]
pub trait ConflictRemediationJob: Send + Sync {
    async fn run(&self, candidate: PendingMergeCheck, attempt: ConflictResolution, probe: PrLifecycleProbe);
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
}

#[async_trait]
impl ConflictRemediationJob for LadderRemediationJob {
    async fn run(&self, candidate: PendingMergeCheck, attempt: ConflictResolution, probe: PrLifecycleProbe) {
        let started = Instant::now();
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
            }
            LadderOutcome::MechanicalRungsUnavailable => {
                // Same contract as the inline path: spawn nothing, leave the
                // attempt `pending` with no `revision_task_id` so the next
                // detection tick re-enters the ladder from scratch (subject
                // to the cooldown above).
                tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    attempt_id = %attempt.id,
                    elapsed_ms = started.elapsed().as_millis(),
                    "conflict_remediation: mechanical rungs unavailable this attempt (rung-1 lease); \
                     no worker spawned, ladder will retry on a later tick",
                );
            }
            LadderOutcome::FellThrough {
                residual_conflict_files,
                ..
            } => {
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
            // The guard marks the slot complete on *every* exit path,
            // including a panic inside the job or the runtime dropping the
            // task — a leaked `InFlight` entry would silently wedge
            // remediation for that PR for the life of the process.
            let _guard = SlotGuard {
                slots,
                key: key.clone(),
            };
            let _permit = match permits.acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(pr_url = %key, "conflict_remediation: semaphore closed; dropping remediation");
                    return;
                }
            };
            job.run(candidate, attempt, probe).await;
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
}

struct SlotGuard {
    slots: Arc<Mutex<HashMap<String, SlotState>>>,
    key: String,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut slots = match self.slots.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        slots.insert(self.key.clone(), SlotState::CompletedAt(Instant::now()));
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
        fn parked() -> Parked {
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
        async fn run(&self, candidate: PendingMergeCheck, _attempt: ConflictResolution, _probe: PrLifecycleProbe) {
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
        } = ParkingJob::parked();
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
        let Parked { job, seen, release, .. } = ParkingJob::parked();
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
    async fn a_completed_pr_is_declined_until_the_cooldown_elapses() {
        let Parked {
            job,
            seen,
            finished,
            release,
            ..
        } = ParkingJob::parked();
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
            "a PR whose ladder run just finished must not be immediately re-run",
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

    #[tokio::test]
    async fn concurrency_is_capped_at_the_configured_bound() {
        let Parked {
            job,
            seen,
            peak,
            release,
            ..
        } = ParkingJob::parked();
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
            async fn run(&self, _: PendingMergeCheck, _: ConflictResolution, _: PrLifecycleProbe) {
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
}
