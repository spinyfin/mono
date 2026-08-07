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
//! runs on the same dispatch path, immediately after `apply_event`, and
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

use boss_protocol::{ExecutionKind, WorkerActivity};

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
    execution_id: &str,
    is_pr_review: bool,
    prior: Option<WorkerActivity>,
    new: Option<WorkerActivity>,
) {
    if is_pr_review {
        return;
    }
    let Some(awaiting) = awaiting_input_transition(prior, new) else {
        return;
    };
    write_awaiting_status(work_db, publisher, execution_id, awaiting).await;
}

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
/// This is the sole call site both `app/server.rs`'s stalled-spawn timer and
/// its tests should use — pulling the slot resolution and the `pr_review`
/// skip out of the timer body means the two can never diverge, and a test
/// that calls this function is actually pinning the production wiring
/// rather than a re-typed copy of it.
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
        let is_pr_review = state.kind.as_deref() == Some(ExecutionKind::PrReview.as_str());
        mirror_stalled_spawn_wait(work_db, publisher, &state.run_id, is_pr_review).await;
    }
}

async fn write_awaiting_status(
    work_db: &Arc<WorkDb>,
    publisher: &Arc<dyn ExecutionPublisher>,
    execution_id: &str,
    awaiting: bool,
) {
    let updated = match work_db.set_execution_awaiting_human(execution_id, awaiting) {
        Ok(Some(execution)) => execution,
        Ok(None) => {
            tracing::debug!(
                execution_id,
                awaiting,
                "awaiting-input mirror: no row moved (already in the target state, or the row is \
                 no longer a live pane-hosted worker)",
            );
            return;
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                awaiting,
                ?err,
                "awaiting-input mirror: failed to write the execution status; the stored row now \
                 disagrees with the pane until the next transition re-converges it",
            );
            return;
        }
    };
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
