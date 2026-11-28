//! Skip-the-queue dispatch of one named `ready` execution: the claim, the
//! spawn, and the two explicit start gestures built on them.
//!
//! `bossctl agents launch` and a card moved to Doing are the same request:
//! start this item now. Both create or reuse the item's execution through
//! [`ExecutionCoordinator::request_execution_via_db`] and hand it to
//! [`ExecutionCoordinator::claim_forced_dispatch`], which bypasses the focus
//! set and the interactive concurrency cap and grows the execution's home
//! pool up to its hard cap. They differ in what they wait for and in what
//! they admit past a dispatch pause:
//!
//! - [`ExecutionCoordinator::force_dispatch`] awaits the spawn, so the CLI
//!   learns whether the worker started. It names
//!   [`DispatchAdmission::OperatorForced`], which overrides any pause.
//! - [`ExecutionCoordinator::launch_from_board_move`] answers once the worker
//!   slot and durable execution claim are held and launch intent is persisted.
//!   It runs the same spawn in the background. The frontend
//!   connection handles one client's requests in order, so a board gesture
//!   that awaited a cube lease would stall every request the app sent behind
//!   it. It never overrides a pause the owner has not confirmed: a paused
//!   engine refuses the gesture with the pause's reason.
use boss_engine_storage_values::dispatch_pause::exempts_work;
use boss_protocol::RequestExecutionInput;

use super::lease_limiter::LeaseAttemptGuard;
use super::scheduler::DrainDispatchClaim;
use super::*;
use crate::dispatch_inflight::DispatchReservation;
use crate::work::CancelExecutionOpts;

/// Why a forced dispatch could not claim a worker for a `ready` execution.
///
/// Carried in the `anyhow` chain. `Display` is the sentence `bossctl agents
/// launch` prints; [`Self::plain_reason`] is the same refusal for a card on
/// the board, without execution ids.
#[derive(Debug)]
pub enum ForcedDispatchRefusal {
    /// Another dispatch holds an incompatible reservation for this item.
    ReservationConflict { execution_id: String },
    /// The drain already owns the durable claim; this launch lost the race.
    AlreadyStarting,
    /// The execution changed state before the durable claim could be taken.
    ClaimRejected { execution_id: String },
    /// A startup preflight is still blocking local dispatch.
    DispatchUnavailable(String),
    /// A dispatch pause the caller's admission does not override.
    Paused {
        /// Operator-facing sentence naming the pause's origin, scope and reason.
        detail: String,
        /// The reason the pause was raised with.
        reason: String,
    },
    /// The per-repo bound on concurrent cube workspace leases is reached.
    RepoLeaseBound {
        bound: usize,
        repo: String,
        execution_id: String,
    },
    /// Every slot of the execution's home pool is busy and the pool cannot grow.
    PoolAtHardCap {
        pool: &'static str,
        hard_cap: usize,
        execution_id: String,
    },
}

impl ForcedDispatchRefusal {
    /// The refusal as a clause for someone looking at the board.
    pub fn plain_reason(&self) -> String {
        match self {
            Self::ReservationConflict { .. } => {
                "another worker is being set up for this item; try again when setup finishes".to_owned()
            }
            Self::AlreadyStarting => "a worker is already being started for this item".to_owned(),
            Self::ClaimRejected { .. } => {
                "the item is no longer ready to start; try again after its dependencies clear".to_owned()
            }
            Self::DispatchUnavailable(reason) => format!("the engine cannot start workers yet ({reason})"),
            Self::Paused { reason, .. } => format!("dispatch is paused ({reason})"),
            Self::RepoLeaseBound { bound, .. } => {
                format!("{bound} workspaces are already being set up for its repository; try again in a moment")
            }
            Self::PoolAtHardCap { pool, hard_cap, .. } => {
                format!("all {hard_cap} {pool} worker slots are in use; try again when one finishes")
            }
        }
    }
}

impl std::fmt::Display for ForcedDispatchRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReservationConflict { execution_id } => write!(
                f,
                "execution {execution_id} (or another execution for its work item) is already being dispatched — cannot force-dispatch"
            ),
            Self::AlreadyStarting => f.write_str("a worker is already being started for this item"),
            Self::ClaimRejected { execution_id } => {
                write!(f, "execution {execution_id} could not be claimed for dispatch")
            }
            Self::DispatchUnavailable(reason) => write!(f, "local dispatch is unavailable: {reason}"),
            Self::Paused { detail, .. } => f.write_str(detail),
            Self::RepoLeaseBound {
                bound,
                repo,
                execution_id,
            } => write!(
                f,
                "{bound} cube workspace leases are already in flight for {repo}; \
                 cannot force-dispatch {execution_id} until one finishes"
            ),
            Self::PoolAtHardCap {
                pool,
                hard_cap,
                execution_id,
            } => f.write_str(&boss_protocol::pool_hard_cap_refusal_message(
                pool,
                *hard_cap,
                execution_id,
            )),
        }
    }
}

impl std::error::Error for ForcedDispatchRefusal {}

/// A `ready` execution holding everything a forced dispatch takes before it
/// spawns: the in-flight reservation, the per-repo lease permit and a worker
/// slot. Dropping it releases the permit and the reservation; the slot is
/// returned by [`ExecutionCoordinator::finish_forced_dispatch`].
struct ForcedDispatchClaim {
    execution: WorkExecution,
    worker_id: String,
    // Declared so the permit is released before the reservation.
    _lease_attempt: Option<LeaseAttemptGuard>,
    _reservation: DispatchReservation,
}

/// What a card moved to Doing did.
#[derive(Debug)]
pub struct BoardLaunchOutcome {
    /// The execution the request settled on.
    pub execution: WorkExecution,
    /// Whether this request claimed a worker for it. `false` when the item
    /// already had a worker, or one being started, and nothing was launched.
    pub launched: bool,
}

impl ExecutionCoordinator {
    fn record_live_board_launch(&self, work_item_id: &str) {
        if let Err(err) = self.work_db.record_explicit_launch(work_item_id) {
            tracing::warn!(%work_item_id, ?err, "failed to record launch intent for an active worker");
        }
    }

    /// Hold a dispatch at the handoff boundary in integration fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn reserve_board_dispatch_for_test(&self, execution: &WorkExecution) -> DispatchReservation {
        self.inflight_dispatches
            .try_reserve(execution, &self.work_db)
            .expect("reserve fixture dispatch")
    }

    /// Skip-the-queue dispatch for `bossctl agents launch` and the
    /// spawn-capability breaker's half-open recovery probe
    /// (`spawn_health::maybe_admit_recovery_probe`). Looks the execution up
    /// directly, classifies which pool it belongs to exactly as
    /// `drain_ready_queue` does (via [`Self::pool_for_execution`]), claims a
    /// worker from THAT pool via `WorkerPool::claim_worker_force` (which
    /// grows the pool by one slot up to ITS OWN hard cap when every
    /// configured slot is busy), and runs the same `schedule_execution` path
    /// the auto-dispatcher uses. Returns the worker id we landed on so
    /// callers can echo it back to the human.
    ///
    /// "Force" bypasses the interactive-pool concurrency cap outright, and
    /// the dispatch pause only as far as the caller's [`DispatchAdmission`]
    /// allows — it is NOT a blanket pause bypass. `bossctl agents launch`
    /// passes [`DispatchAdmission::OperatorForced`] and does override the
    /// pause; the breaker's recovery canary passes
    /// [`DispatchAdmission::BreakerRecoveryProbe`], which
    /// [`Self::dispatch_hold_for`] admits only under a Breaker-origin pause
    /// and never for a `pr_review` row. Passing the admission down rather
    /// than assuming one is what stopped this function from being a hole in
    /// the pause. It never bypasses *pool
    /// correctness*: a `pr_review` still claims a `review-` worker id (so it
    /// resolves the pinned Opus reviewer policy via
    /// [`pool_dispatch_policy_for_worker_id`] and never counts against the
    /// interactive pool's `busy_count`), and an automation-sourced row still
    /// claims an `auto-worker-` id, exactly as the normal path would.
    ///
    /// When the execution's home pool has no free slot, force-dispatch grows
    /// THAT pool by one slot (bounded by its own hard cap) rather than
    /// falling back to a different pool — the same policy the main pool
    /// already used for `bossctl agents launch`, now applied uniformly so
    /// the forced worker always keeps its pool's dispatch policy.
    ///
    /// Errors when the execution is not in `ready` (already claimed by
    /// the auto-dispatcher in a race, terminal, or unknown), when the
    /// active pause does not admit `admission`, or when the target pool is
    /// already at its hard cap with no idle slot.
    /// On admission, `try_reserve` precedes the first `.await` (`claim_worker_force`);
    /// the operator-answer first poll of [`Self::launch_answered_work`] depends on it.
    pub async fn force_dispatch(self: &Arc<Self>, execution_id: &str, admission: DispatchAdmission) -> Result<String> {
        let claim = self.claim_forced_dispatch(execution_id, admission).await?;
        self.finish_forced_dispatch(claim, admission).await
    }

    /// The decision half of a forced dispatch: everything that can refuse it
    /// before a cube lease is taken. Successful claims reserve before yielding.
    async fn claim_forced_dispatch(
        self: &Arc<Self>,
        execution_id: &str,
        admission: DispatchAdmission,
    ) -> Result<ForcedDispatchClaim> {
        if let Some(reason) = self.dispatch_preflight_block_reason() {
            return Err(ForcedDispatchRefusal::DispatchUnavailable(reason).into());
        }
        let execution = self
            .work_db
            .get_execution(execution_id)
            .with_context(|| format!("failed to look up execution {execution_id}"))?;
        if execution.status != ExecutionStatus::Ready {
            return Err(anyhow!(
                "execution {execution_id} is in status {status:?}, not ready — cannot force-dispatch",
                status = execution.status,
            ));
        }
        // Refuse before claiming a slot, so a held force-dispatch never has
        // to hand a worker back. `schedule_execution` re-checks — it is the
        // chokepoint and owns the guarantee — but answering here keeps the
        // error the caller sees precise and costs nothing.
        if let Some(hold) = self.dispatch_hold_for(&execution, admission) {
            return Err(ForcedDispatchRefusal::Paused {
                detail: hold.to_string(),
                reason: hold.pause.reason.to_string(),
            }
            .into());
        }
        // Take the durable claim before the reservation. If the drain won,
        // it must remain free to reserve the item and finish its dispatch.
        match self.work_db.claim_execution_for_dispatch(&execution.id) {
            Ok(DispatchClaimOutcome::Won) => {}
            Ok(DispatchClaimOutcome::AlreadyHeld) => return Err(ForcedDispatchRefusal::AlreadyStarting.into()),
            Ok(DispatchClaimOutcome::Rejected) => {
                return Err(ForcedDispatchRefusal::ClaimRejected {
                    execution_id: execution_id.to_owned(),
                }
                .into());
            }
            Err(err) => {
                self.fail_execution_if_allocation_refused(&execution, &err).await;
                return Err(err);
            }
        }
        let mut durable_claim = DrainDispatchClaim::already_held(Arc::clone(&self.work_db), execution.id.clone());
        let Some(reservation) = self.inflight_dispatches.try_reserve(&execution, &self.work_db) else {
            return Err(ForcedDispatchRefusal::ReservationConflict {
                execution_id: execution_id.to_owned(),
            }
            .into());
        };
        // A forced launch overrides the interactive cap, not the per-repo
        // lease bound: extra concurrent leases on one repo only queue on
        // cube's per-repo lock, so a forced launch during a lease storm
        // would add timeouts rather than start sooner. A spoke lease the
        // host has not answered is a retry of a stored command, not a new
        // cube lease, and must not occupy that bound.
        let lease_attempt = if self.spoke_lease_retry_skips_repo_bound(&execution) {
            None
        } else {
            let Some(guard) = self
                .repo_lease_limiter
                .try_begin(&execution.repo_remote_url, &execution.id)
                .map(|guard| guard.with_kick(Arc::clone(self)))
            else {
                return Err(ForcedDispatchRefusal::RepoLeaseBound {
                    bound: self.repo_lease_limiter_bound(),
                    repo: execution.repo_remote_url.clone(),
                    execution_id: execution_id.to_owned(),
                }
                .into());
            };
            Some(guard)
        };
        let pool = self.pool_for_execution(&execution);
        let worker_id = pool
            .claim_worker_force(&execution.id, execution.preferred_workspace_id.as_deref())
            .await
            .ok_or_else(|| ForcedDispatchRefusal::PoolAtHardCap {
                pool: self.attributed_pool_label(&execution),
                hard_cap: pool.hard_cap(),
                execution_id: execution_id.to_owned(),
            })?;
        durable_claim.disarm();
        Ok(ForcedDispatchClaim {
            execution,
            worker_id,
            _lease_attempt: lease_attempt,
            _reservation: reservation,
        })
    }

    /// The spawn half of a forced dispatch: take the claimed execution to a
    /// running worker, or hand the slot back and record the failed start.
    async fn finish_forced_dispatch(
        self: &Arc<Self>,
        claim: ForcedDispatchClaim,
        admission: DispatchAdmission,
    ) -> Result<String> {
        let execution = &claim.execution;
        let worker_id = &claim.worker_id;
        let mut spawn_claim = DrainDispatchClaim::already_held(Arc::clone(&self.work_db), execution.id.clone());
        if let Err(err) = self.schedule_execution(execution, worker_id, admission).await {
            self.pool_for_execution(execution)
                .release_worker_if_execution(worker_id, &execution.id, execution.preferred_workspace_id.as_deref())
                .await;
            self.recover_failed_dispatch(execution, worker_id, &err).await;
            return Err(err);
        }
        spawn_claim.disarm();
        self.consume_operator_forced_if_admitted(&execution.id, admission);
        Ok(worker_id.clone())
    }

    /// [`Self::force_dispatch`] that answers once the worker slot, durable
    /// execution claim and explicit launch intent are held.
    /// The spawn runs in the background with the same failure handling: a
    /// start that then fails is retried or reported by the ordinary pre-start
    /// machinery, exactly as it is for a claim the drain made.
    async fn force_dispatch_at_claim(
        self: &Arc<Self>,
        execution_id: &str,
        admission: DispatchAdmission,
    ) -> Result<String> {
        let claim = self.claim_forced_dispatch(execution_id, admission).await?;
        // Persistence is part of admission, before the spawn is detached.
        if let Err(err) = self.work_db.record_explicit_launch(&claim.execution.work_item_id) {
            let release = self.work_db.release_dispatch_claim(&claim.execution.id);
            self.pool_for_execution(&claim.execution)
                .release_worker_if_execution(
                    &claim.worker_id,
                    &claim.execution.id,
                    claim.execution.preferred_workspace_id.as_deref(),
                )
                .await;
            release?;
            return Err(err.context("failed to record board launch intent"));
        }
        let worker_id = claim.worker_id.clone();
        let coordinator = Arc::clone(self);
        boss_task_scope::spawn(async move {
            let execution_id = claim.execution.id.clone();
            let work_item_id = claim.execution.work_item_id.clone();
            let worker_id = claim.worker_id.clone();
            match coordinator.finish_forced_dispatch(claim, admission).await {
                Ok(_) => tracing::info!(
                    %execution_id,
                    %work_item_id,
                    %worker_id,
                    "spawn_attempt status=ready -> spawned (launched from the board)"
                ),
                Err(err) => tracing::error!(
                    ?err,
                    %execution_id,
                    %work_item_id,
                    %worker_id,
                    "spawn_attempt status=ready -> failed reason=schedule_execution_error (launched from the board)"
                ),
            }
        });
        Ok(worker_id)
    }

    /// Start `input.work_item_id` now for a card the owner moved to Doing,
    /// the way `bossctl agents launch` starts it: the item's execution is
    /// created or reused by the same request, claimed past the focus set and
    /// the interactive concurrency cap (growing its pool up to the hard cap),
    /// and the launch is recorded on the item so the review, CI-fix and
    /// merge-conflict work the engine mints for it stays admitted while focus
    /// mode excludes it.
    ///
    /// The gesture is not a pause override. While a pause holds this item the
    /// request is refused before anything is written; the confirmed override
    /// is [`Self::dispatch_with_pause_bypass`]. A `critical` item keeps the
    /// exemption the drain gives it from a default-scope operator pause.
    ///
    /// A refusal records no launch intent. A newly created execution is
    /// cancelled unless allocation refusal already failed it visibly. A
    /// reused queued execution retains its prior queue state. The caller
    /// owns reverting the status move. For an already-active worker, an
    /// intent-write failure is logged without refusing the move.
    pub async fn launch_from_board_move(
        self: &Arc<Self>,
        input: RequestExecutionInput,
        live_states: Arc<dyn LiveWorkerLiveness>,
    ) -> Result<BoardLaunchOutcome> {
        let facts = self.work_db.dispatch_admission_facts(&input.work_item_id)?;
        let work_item_id = facts.resolved_work_item_id.clone();
        let critical_main = facts.targets_main_pool && facts.is_critical;
        if let Some(pause) = self.dispatch_pause()
            && !exempts_work(
                pause.accepts_pause_bypass(),
                facts.exempt_from_operator_pause,
                critical_main,
            )
        {
            return Err(ForcedDispatchRefusal::Paused {
                detail: format!(
                    "dispatch is paused ({origin}, scope {scope}): {reason}",
                    origin = pause.origin.as_metadata_str(),
                    scope = pause.scope.as_str(),
                    reason = pause.reason,
                ),
                reason: pause.reason.to_string(),
            }
            .into());
        }
        let admission = if critical_main {
            DispatchAdmission::Critical
        } else {
            DispatchAdmission::Queued
        };

        // Retain prior statuses so refusal can undo a reused waiting row's
        // promotion as well as cancel a newly created execution.
        let pre_existing: HashMap<String, ExecutionStatus> = self
            .work_db
            .list_executions(Some(&work_item_id))?
            .into_iter()
            .filter(|execution| !execution.status.is_terminal())
            .map(|execution| (execution.id, execution.status))
            .collect();
        let execution = self.request_execution_via_db(input, live_states)?;
        if execution.status != ExecutionStatus::Ready {
            // The item already has a worker, or one being started: there is
            // no second worker to launch. The owner still asked for it by
            // name, so the launch is recorded.
            self.record_live_board_launch(&work_item_id);
            return Ok(BoardLaunchOutcome {
                execution,
                launched: false,
            });
        }
        match self.force_dispatch_at_claim(&execution.id, admission).await {
            Ok(_worker_id) => Ok(BoardLaunchOutcome {
                execution: self.work_db.get_execution(&execution.id).unwrap_or(execution),
                launched: true,
            }),
            Err(err) => {
                // A drain that claimed the row first is not a refusal: the
                // item is being started, which is what was asked for.
                let refreshed = self.work_db.get_execution(&execution.id).unwrap_or(execution);
                let dispatching = matches!(
                    refreshed.status,
                    ExecutionStatus::Claimed | ExecutionStatus::Running | ExecutionStatus::WaitingHuman
                ) || self.inflight_dispatches.try_is_execution_dispatching(&refreshed.id)
                    == Some(true);
                if dispatching
                    || matches!(
                        err.downcast_ref::<ForcedDispatchRefusal>(),
                        Some(ForcedDispatchRefusal::AlreadyStarting)
                    )
                {
                    self.record_live_board_launch(&work_item_id);
                    return Ok(BoardLaunchOutcome {
                        execution: refreshed,
                        launched: false,
                    });
                }
                if pre_existing.get(&refreshed.id) == Some(&ExecutionStatus::WaitingDependency) {
                    self.work_db.restore_ready_to_waiting_dependency(&refreshed.id)?;
                } else if !pre_existing.contains_key(&refreshed.id) && !refreshed.status.is_terminal() {
                    self.work_db.cancel_execution_with(
                        &refreshed.id,
                        CancelExecutionOpts {
                            reason: Some(format!("start from the board refused: {err:#}")),
                            queued_only: true,
                            record_failure_reason: false,
                        },
                    )?;
                }
                Err(err)
            }
        }
    }
}
