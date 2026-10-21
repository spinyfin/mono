//! Keep `work_executions.status` in agreement with the pane a worker is
//! actually running in.
//!
//! # The disagreement this closes
//!
//! Boss has two surfaces that answer "what is this worker doing":
//!
//! - **The pane** — [`crate::live_worker_state::LiveWorkerStateRegistry`],
//!   driven event-by-event off the worker's own hook stream. `bossctl
//!   agents list` reads it.
//! - **The row** — `work_executions.status`. Dispatch admission, every
//!   liveness sweep, the reaper, attention surfacing, the kanban and every
//!   CLI/JSON reader read *that*.
//!
//! The pane is the authority on moment-to-moment activity: it is fed by the
//! worker's own process, in-band, and cannot be forged by stale bookkeeping.
//! The row is the durable projection every other consumer reasons about. So
//! the row does not get to *disagree* with the pane — it has to be written
//! from it.
//!
//! Until mono#2673 it wasn't. `PaneSpawnRunner` stamped `waiting_human` on
//! the row the instant the pane came up and nothing ever cleared it, so a
//! worker mid-`Bash`-call — `activity: working`, `last_tool_end` seconds
//! ago — stored `waiting_human` for its entire life. Two live surfaces,
//! flatly contradicting each other, with the wrong one being the one
//! everything downstream reads.
//!
//! The spawn-time write is gone (`RunWaitState::WorkerPaneAlive` → the row
//! stays `running` while the agent works). This module supplies the other
//! half: the genuine wait, and the resumption out of it.
//!
//! # The signal
//!
//! [`WorkerActivity::WaitingForInput`] is not inferred and not a heuristic
//! over recent tool activity. It is the driver's own awaiting-input
//! notification — Claude's `Notification` hook — and the registry only
//! honours it for a driver that declared
//! `Capability::AwaitingInputSignal`, refusing to guess for one that did
//! not (see `LiveWorkerStateRegistry::apply_event`). It clears on the next
//! `PreToolUse`/`UserPromptSubmit`, i.e. when the worker demonstrably
//! resumed.
//!
//! That signal was already arriving; it just stopped at the in-memory
//! registry and never reached the durable row. [`mirror_awaiting_input`]
//! runs on the same dispatch path, immediately after the live-state transition (before any transcript I/O), and
//! writes the transition through.
//!
//! # What is deliberately NOT mirrored
//!
//! A pane going [`WorkerActivity::Terminated`] or
//! [`WorkerActivity::Errored`] is a *death* signal, not a wait signal.
//! Death detection has its own owners (`dead_pane_sweep`,
//! `husk_pane_sweep`, `lost_workspace_sweep`, `spawn_ack_sweep`), each
//! with its own corroboration rules — see
//! `tools/boss/docs/worker-liveness-contract.md`. Writing a live status
//! from here on the way out of a wait would race them, so this module
//! leaves a dying pane's row exactly where it is and lets the reaper own
//! it.

use std::sync::Arc;

use boss_protocol::{ExecutionKind, WorkExecution, WorkerActivity};

use crate::coordinator::ExecutionPublisher;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;

/// Reason string stamped on the `executions.<id>` topic invalidation when
/// a worker parks on a human.
const REASON_AWAITING_INPUT: &str = "worker_awaiting_human_input";

/// Reason string stamped when the worker resumes and the row goes back to
/// `running`.
const REASON_RESUMED: &str = "worker_resumed_from_human_input";

/// Whether an activity transition crosses the awaiting-input boundary, and
/// in which direction. `Some(true)` means "park the row on a human",
/// `Some(false)` means "the worker resumed", `None` means the row must not
/// be touched.
///
/// Pure so the policy is pinned by unit tests without a DB, a registry, or
/// a `ServerState`.
pub fn awaiting_input_transition(prior: Option<WorkerActivity>, new: Option<WorkerActivity>) -> Option<bool> {
    let new = new?;
    let was_waiting = prior == Some(WorkerActivity::WaitingForInput);
    let is_waiting = new == WorkerActivity::WaitingForInput;
    match (was_waiting, is_waiting) {
        // Entered the wait. `prior == None` (first event on a freshly
        // registered slot) counts as entering: the row is `running` from
        // the spawn, and this is the first evidence otherwise.
        (false, true) => Some(true),
        // Left the wait — but only for another *live* activity. A pane
        // that went `Terminated`/`Errored` belongs to the death sweeps;
        // see the module docs.
        (true, false) if !new.is_terminal() => Some(false),
        _ => None,
    }
}

/// The activity a hook event moved a slot between, as that handler saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityTransition {
    pub prior: Option<WorkerActivity>,
    pub new: Option<WorkerActivity>,
}

/// Mirror one activity transition onto `execution_id`'s stored status.
///
/// A no-op unless the transition crosses the awaiting-input boundary
/// ([`awaiting_input_transition`]) *and* the row is in the matching
/// `running`/`waiting_human` state — [`WorkDb::set_execution_awaiting_human`]
/// applies that guard in SQL, so a row the completion path has already
/// moved on (terminal, `waiting_review`, `waiting_merge`) is never dragged
/// back into a live status by a late hook.
///
/// `is_pr_review` excludes `pr_review` executions: a reviewer's own kanban
/// "AI reviewing" badge is keyed off `status = running` for the duration of
/// the review (see `runner::RunWaitState::WorkerPaneAlive`'s doc), and a
/// reviewer session runs with tool calls denied — exactly the shape that
/// produces a `Notification` permission-prompt hook. Mirroring that onto
/// the row would flip a healthy, still-working reviewer to `waiting_human`
/// and drop the badge mid-review for no genuine wait. This matches the
/// exclusion `completion::worker_owns_turn_loop` already applies to
/// reviewers.
///
/// Best-effort: a failed write is logged and dropped. This runs inline on
/// the worker-event dispatch path and must never block or fail it — the
/// worst case of a dropped write is the pre-fix behaviour for one
/// transition, and the next one re-converges.
pub async fn mirror_awaiting_input(
    work_db: &Arc<WorkDb>,
    publisher: &Arc<dyn ExecutionPublisher>,
    registry: &LiveWorkerStateRegistry,
    slot_id: u8,
    execution_id: &str,
    is_pr_review: bool,
    transition: ActivityTransition,
) {
    if is_pr_review {
        return;
    }
    // The before/after pair only decides whether this event is worth a look.
    if awaiting_input_transition(transition.prior, transition.new).is_none() {
        return;
    }
    // What gets written is decided from the pane's activity *at write time*,
    // inside a gate that serializes every hook's write. Two hooks for one
    // worker run on separate tasks, so a `Notification` (-> waiting) and the
    // `PreToolUse` that follows it (-> working) can reach this point in
    // either order; deciding from the pair each saw would let the older one
    // write last and leave the row disagreeing with the pane until the next
    // transition. Deciding from the current activity makes the last write
    // always agree with the latest pane state.
    let updated = {
        let _gate = MIRROR_WRITE_GATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(current) = registry.get(slot_id).map(|s| s.activity) else {
            return;
        };
        // A pane that died belongs to the death sweeps; see the module docs.
        if current.is_terminal() {
            return;
        }
        let awaiting = current == WorkerActivity::WaitingForInput;
        match store_awaiting_status(work_db, execution_id, awaiting) {
            Some(updated) => (updated, awaiting),
            None => return,
        }
    };
    publish_awaiting_status(publisher, execution_id, &updated.0, updated.1).await;
}

/// Serializes the decide-and-write step of [`mirror_awaiting_input`] across
/// hook tasks. Held only across the synchronous status write, never an await.
static MIRROR_WRITE_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Mirror `LiveWorkerStateRegistry::mark_stalled_spawns`'s promotion onto
/// the row.
///
/// That sweep is a second, independent producer of
/// [`WorkerActivity::WaitingForInput`] — the directory-trust-prompt stall,
/// which by construction has `last_event_at == None` (no hook has ever
/// arrived) and never will, so [`mirror_awaiting_input`] above — which only
/// runs off the hook-dispatch path — can never observe or mirror it. Left
/// unmirrored, that is the one wait an unattended run most needs surfaced:
/// the row stays `running` forever, disagreeing with a pane that is
/// genuinely, permanently blocked on a human.
///
/// Always enters the wait (`awaiting = true`); the sweep never produces the
/// resumption edge — a worker that gets past the trust prompt does so by
/// emitting a hook, which resumes it through [`mirror_awaiting_input`]
/// instead. [`WorkDb::set_execution_awaiting_human`]'s SQL guard makes a
/// redundant or late call here a safe no-op.
///
/// `is_pr_review` mirrors [`mirror_awaiting_input`]'s exclusion: a reviewer
/// stalled on the directory-trust prompt has no hook-dispatch path to ever
/// resume it (that edge lives solely in `mirror_awaiting_input`, which
/// returns early for reviewers), so mirroring the wait here would strand
/// the row in `waiting_human` for the rest of the review with nothing able
/// to clear it.
pub async fn mirror_stalled_spawn_wait(
    work_db: &Arc<WorkDb>,
    publisher: &Arc<dyn ExecutionPublisher>,
    execution_id: &str,
    is_pr_review: bool,
) {
    if is_pr_review {
        return;
    }
    write_awaiting_status(work_db, publisher, execution_id, true).await;
}

/// Resolve each stalled slot to its run id and kind, and mirror the
/// promotion onto the row via [`mirror_stalled_spawn_wait`].
///
/// `mark_stalled_spawns` returns every slot it moved out of `Spawning`,
/// including capability-less drivers promoted to [`WorkerActivity::Idle`]
/// on attach evidence. Only [`WorkerActivity::WaitingForInput`] is a wait
/// the row should absorb: `Idle` is "alive, no specific claim", and
/// [`awaiting_input_transition`] only clears `waiting_human` on a
/// transition *out of* `WaitingForInput`. Mirroring an Idle promotion
/// would stamp `waiting_human` with nothing able to repair it. The Idle
/// slots still belong in `stalled` so the timer can broadcast the pane
/// update; they just must not reach the row write.
///
/// This is the sole call site both `app/server.rs`'s stalled-spawn timer and
/// its tests should use — pulling the slot resolution, the activity gate,
/// and the `pr_review` skip out of the timer body means the two can never
/// diverge, and a test that calls this function is actually pinning the
/// production wiring rather than a re-typed copy of it.
pub async fn mirror_stalled_spawn_waits(
    registry: &LiveWorkerStateRegistry,
    work_db: &Arc<WorkDb>,
    publisher: &Arc<dyn ExecutionPublisher>,
    stalled: &[u8],
) {
    for slot_id in stalled {
        let Some(state) = registry.get(*slot_id) else {
            continue;
        };
        if state.activity != WorkerActivity::WaitingForInput {
            continue;
        }
        let is_pr_review = state.kind.as_deref() == Some(ExecutionKind::PrReview.as_str());
        mirror_stalled_spawn_wait(work_db, publisher, &state.run_id, is_pr_review).await;
    }
}

/// One periodic declared-wait pass: reclassify parked workers against the wait
/// registry, keep each header subtitle on the new state, and mirror departures
/// from `WaitingForInput` onto the rows. Returns the slots that changed so the
/// caller can broadcast.
pub async fn sweep_declared_waits(
    registry: &LiveWorkerStateRegistry,
    work_db: &Arc<WorkDb>,
    publisher: &Arc<dyn ExecutionPublisher>,
    live_status_manager: &crate::live_status_loop::LiveStatusManager,
    now_epoch_secs: i64,
) -> Vec<u8> {
    let changed = registry.reconcile_declared_waits(now_epoch_secs);
    if changed.is_empty() {
        return Vec::new();
    }
    let slots: Vec<u8> = changed.iter().map(|(slot, ..)| *slot).collect();
    tracing::info!(
        slots = ?slots,
        "declared-wait sweep: reclassified parked workers \
         (WaitingOnWork while a wait is live, WaitOverdue once it expired)",
    );
    for (slot_id, _, new) in &changed {
        if *slot_id < crate::worker_registry::REMOTE_SLOT_BASE {
            live_status_manager.notify(*slot_id, crate::live_status_loop::Trigger::ActivityChanged(*new));
        }
    }
    mirror_declared_wait_reclassifications(registry, work_db, publisher, &changed).await;
    slots
}

/// Mirror the periodic declared-wait pass's reclassifications onto the rows.
///
/// A worker parked as `WaitingForInput` (and mirrored to `waiting_human`)
/// that then declares a wait is reclassified off the hook-dispatch path, so
/// `mirror_awaiting_input` never sees the departure. Each `(slot, prior, new)`
/// goes through the same [`awaiting_input_transition`] policy, so the row can
/// only ever be cleared, never newly parked, by this pass.
pub async fn mirror_declared_wait_reclassifications(
    registry: &LiveWorkerStateRegistry,
    work_db: &Arc<WorkDb>,
    publisher: &Arc<dyn ExecutionPublisher>,
    changed: &[(u8, WorkerActivity, WorkerActivity)],
) {
    for (slot_id, prior, new) in changed {
        let Some(state) = registry.get(*slot_id) else {
            continue;
        };
        let is_pr_review = state.kind.as_deref() == Some(ExecutionKind::PrReview.as_str());
        mirror_awaiting_input(
            work_db,
            publisher,
            &state.run_id,
            is_pr_review,
            Some(*prior),
            Some(*new),
        )
        .await;
    }
}

async fn write_awaiting_status(
    work_db: &Arc<WorkDb>,
    publisher: &Arc<dyn ExecutionPublisher>,
    execution_id: &str,
    awaiting: bool,
) {
    if let Some(updated) = store_awaiting_status(work_db, execution_id, awaiting) {
        publish_awaiting_status(publisher, execution_id, &updated, awaiting).await;
    }
}

/// The durable half: move the row, or report why nothing moved.
fn store_awaiting_status(work_db: &Arc<WorkDb>, execution_id: &str, awaiting: bool) -> Option<WorkExecution> {
    match work_db.set_execution_awaiting_human(execution_id, awaiting) {
        Ok(Some(execution)) => Some(execution),
        Ok(None) => {
            tracing::debug!(
                execution_id,
                awaiting,
                "awaiting-input mirror: no row moved (already in the target state, or the row is \
                 no longer a live pane-hosted worker)",
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                awaiting,
                ?err,
                "awaiting-input mirror: failed to write the execution status; the stored row now \
                 disagrees with the pane until the next transition re-converges it",
            );
            None
        }
    }
}

async fn publish_awaiting_status(
    publisher: &Arc<dyn ExecutionPublisher>,
    execution_id: &str,
    updated: &WorkExecution,
    awaiting: bool,
) {
    let reason = if awaiting {
        REASON_AWAITING_INPUT
    } else {
        REASON_RESUMED
    };
    tracing::info!(
        execution_id,
        work_item_id = %updated.work_item_id,
        status = %updated.status,
        "awaiting-input mirror: stored execution status now agrees with the pane",
    );
    publisher
        .publish(execution_id, &updated.work_item_id, updated.status.as_str(), reason)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::ExecutionStatus;

    #[test]
    fn entering_the_wait_parks_the_row() {
        assert_eq!(
            awaiting_input_transition(Some(WorkerActivity::Working), Some(WorkerActivity::WaitingForInput)),
            Some(true),
        );
        // A slot whose very first observed activity is the wait (no prior
        // snapshot) still parks: the row is `running` from the spawn.
        assert_eq!(
            awaiting_input_transition(None, Some(WorkerActivity::WaitingForInput)),
            Some(true),
        );
    }

    #[test]
    fn resuming_returns_the_row_to_running() {
        for resumed in [WorkerActivity::Working, WorkerActivity::Idle, WorkerActivity::Spawning] {
            assert_eq!(
                awaiting_input_transition(Some(WorkerActivity::WaitingForInput), Some(resumed)),
                Some(false),
                "{resumed:?} is a live activity and must clear the wait",
            );
        }
    }

    /// A pane that died while parked is the death sweeps' business — see
    /// the module docs. Writing `running` back from here would race them.
    #[test]
    fn a_dying_pane_is_left_for_the_death_sweeps() {
        for dead in [WorkerActivity::Terminated, WorkerActivity::Errored] {
            assert!(dead.is_terminal(), "{dead:?} is expected to be a terminal activity",);
            assert_eq!(
                awaiting_input_transition(Some(WorkerActivity::WaitingForInput), Some(dead)),
                None,
                "{dead:?} must not drive a status write",
            );
        }
    }

    /// Every event a working worker emits (`PreToolUse`, `PostToolUse`,
    /// `Stop` with no pending notification, …) lands here, so the common
    /// case must decide "write nothing" without touching the DB.
    #[test]
    fn transitions_that_do_not_cross_the_boundary_write_nothing() {
        assert_eq!(
            awaiting_input_transition(Some(WorkerActivity::Working), Some(WorkerActivity::Idle)),
            None,
        );
        assert_eq!(
            awaiting_input_transition(Some(WorkerActivity::Spawning), Some(WorkerActivity::Working)),
            None,
        );
        // Capability-less Idle promotion, then a real event: must not be
        // mistaken for leaving a wait, because the slot never entered one.
        assert_eq!(
            awaiting_input_transition(Some(WorkerActivity::Idle), Some(WorkerActivity::Working)),
            None,
        );
        // Re-asserting the wait (a second Notification, or a Stop that
        // preserves the pending flag) is idempotent.
        assert_eq!(
            awaiting_input_transition(
                Some(WorkerActivity::WaitingForInput),
                Some(WorkerActivity::WaitingForInput)
            ),
            None,
        );
        // No new activity at all (slot released between the read and the
        // re-read) must never write.
        assert_eq!(awaiting_input_transition(Some(WorkerActivity::Working), None), None);
    }

    /// The two statuses this module moves between are exactly the two
    /// `ExecutionStatus::is_live` covers, so mirroring never changes
    /// whether a row counts as live to a sweep — only *why* it is live.
    #[test]
    fn mirroring_never_changes_liveness() {
        assert!(ExecutionStatus::Running.is_live());
        assert!(ExecutionStatus::WaitingHuman.is_live());
    }
}
