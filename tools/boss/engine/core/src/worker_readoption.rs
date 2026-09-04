//! The convergence rule for a live worker the engine believes is dead.
//!
//! ## The contradiction
//!
//! An execution row can be terminal while its worker process keeps running.
//! The engine already *detects* this — `note_hook_for_terminal_execution` has
//! logged `[engine-reconcile] live hook event arrived for a TERMINAL
//! execution` since the 2026-07-14 incident, and `bossctl dispatch diagnose`
//! reports it as `SIG-4b`. What it did not do was resolve it. The comment on
//! that function said so explicitly: *"This deliberately does NOT resurrect
//! the row: reconciliation of the actual liveness state belongs to the
//! sweeps."*
//!
//! The sweeps could not resolve it either, because every one of them can only
//! move in one direction — reap. [`crate::husk_pane_sweep`] is the general
//! backstop for "the app hosts a pane the engine forgot", and its only verb is
//! to SIGTERM it; since the 2026-07-26 mass-retirement incident it (correctly)
//! refuses to do that to a demonstrably live worker, and its
//! `MAX_RETIREMENTS_PER_PASS` breaker (correctly) declines outright when many
//! panes go husk at once. Both refusals are right. Together they mean the case
//! *"the engine was wrong, not the worker"* had no resolution at all: the run
//! stayed alive and untracked indefinitely.
//!
//! Untracked is not a cosmetic state. It means:
//!
//! - the agent indicator paints from a `LiveWorkerState` that no longer
//!   exists, so an actively-working row shows no agent or a stale wait;
//! - `bossctl agents stop <exec-id>` reports `no live worker matches`, so the
//!   operator cannot reap it either;
//! - the pool claim is gone, so the work item is eligible for re-dispatch, and
//!   a second worker is spawned onto a row a first worker is still editing.
//!
//! On 2026-07-28 that produced three concurrent workers on one chore and two
//! on another, with eight `claude` processes alive against two the engine
//! could see.
//!
//! ## The rule
//!
//! Give convergence its missing second direction. When a worker proves itself
//! alive for an execution the engine has terminalized, exactly one of two
//! things must happen, promptly and observably:
//!
//! - **Re-adopt** when the terminal status was an *inference* (`orphaned`)
//!   and the worker has supplied positive liveness (an alive pid, a live
//!   tmux pane pid, or a hosted slot). The engine guessed the worker was
//!   gone from the absence of a signal; the worker has now supplied one, so
//!   the guess is withdrawn (see
//!   [`crate::work::WorkDb::readopt_inferred_terminal_execution`]).
//!   `abandoned` is **not** an inference: the idle-park / nudge-breaker path
//!   writes it as a deliberate teardown, then kills the pane, and the dying
//!   process's `session_end` must not resurrect the row.
//! - **Reap** otherwise. Either the terminal status was a *decision*
//!   (`abandoned` / `cancelled` / `completed` / `failed`), the trigger is
//!   `session_end` (the worker announcing its exit, never proof of life),
//!   there is no positive liveness at all, the row already has a different
//!   live execution, or its work item has since closed. In each case the
//!   surviving process must not keep running: the authoritative record is
//!   right and the process is wrong.
//!
//! The asymmetry is deliberate and is the whole safety argument. Re-adoption
//! rewrites a record and is cheap to get wrong in the recoverable direction (a
//! row briefly reads live and the ordinary sweeps re-reap it). Reaping
//! destroys in-flight work and is not recoverable. So re-adoption is reserved
//! for a genuine death-inferred-from-silence that a still-alive worker has
//! now disproven, and reaping is the default everywhere else.
//!
//! ## What this module is not
//!
//! It does not decide *whether* a worker is alive — that is
//! [`crate::durable_liveness`] (durable pid probe) and the hook stream. It
//! takes liveness as an input and answers only "given that it is alive, what
//! should happen to it".

use boss_protocol::ExecutionStatus;

/// The engine's ability to act on this module's verdict, abstracted so
/// detectors can trigger convergence without depending on `ServerState` (and
/// so their tests can assert the trigger fired without standing up an app
/// session). Mirrors [`crate::spawn_ack_sweep::SpawnAckReaper`].
///
/// Implemented by `ServerState` in `app::readoption`, which owns the
/// collaborators the action needs — the worker pool, the live-state registry,
/// and the app session that knows which slot hosts the pane.
#[async_trait::async_trait]
pub trait LiveWorkerConvergence: Send + Sync {
    /// Re-adopt or reap the worker behind `execution_id`, whose execution is
    /// terminal but whose process has been observed alive. `trigger` names the
    /// detector that observed it, and is carried onto the dispatch event.
    async fn converge_live_worker(&self, execution_id: &str, trigger: &str);
}

/// Convergence disabled — for tests and any wiring with no `ServerState`.
///
/// A detector configured with this still enforces its own guard (the durable-pid
/// check in [`crate::orphan_sweep`] blocks duplicate dispatch regardless of who
/// resolves the contradiction); it simply never resolves the underlying state.
pub struct NoopLiveWorkerConvergence;

#[async_trait::async_trait]
impl LiveWorkerConvergence for NoopLiveWorkerConvergence {
    async fn converge_live_worker(&self, _execution_id: &str, _trigger: &str) {}
}

/// What to do about a live worker whose execution the engine has terminalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContradictionVerdict {
    /// Withdraw the inferred death and put the run back under tracking.
    Readopt,
    /// The record stands; the surviving process must be torn down.
    Reap { reason: ReapReason },
    /// Nothing to resolve — the execution is not terminal, so the engine and
    /// the worker already agree.
    NoContradiction,
}

/// Why a surviving worker is being reaped rather than re-adopted. Folded into
/// the dispatch event and the trace line so an operator can tell "you stopped
/// this" from "something newer already owns the row".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapReason {
    /// `abandoned` / `cancelled` / `completed` / `failed`: a deliberate
    /// outcome, not a guess. Reversing it would resurrect a run a human, the
    /// idle-park path, or the worker itself ended. Also used when the trigger
    /// is `session_end`: that event is the process announcing its exit, not
    /// proof of life.
    TerminalByDecision,
    /// The execution status was inferred, but the owning work item has since
    /// reached a terminal state. Re-adopting would resurrect a worker for work
    /// that is already closed.
    WorkItemTerminal,
    /// A different execution for the same work item is already live. Only one
    /// worker may own a row; the newer one wins because it holds the current
    /// workspace lease and the older one's record is already terminal.
    SupersededByLiveExecution,
    /// Status was inferred, but nothing is actually alive: no live shell pid,
    /// no tmux-observed pane pid, no registry slot, no pane the app hosts.
    /// Re-adopting would write `status=running` with no lease, no workspace,
    /// and no registry entry — a row every death sweep is structurally blind
    /// to.
    NoPositiveLiveness,
}

impl ReapReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ReapReason::TerminalByDecision => "terminal_by_decision",
            ReapReason::WorkItemTerminal => "work_item_terminal",
            ReapReason::SupersededByLiveExecution => "superseded_by_live_execution",
            ReapReason::NoPositiveLiveness => "no_positive_liveness",
        }
    }
}

/// Hook kind for the worker announcing its own exit. Never liveness evidence:
/// [`crate::live_worker_state`] sets `activity = Terminated` on it, and the
/// events socket calls it a process boundary, not a turn boundary.
pub const SESSION_END_TRIGGER: &str = "session_end";

/// Triggers that observe only the shell hosting a pane (or a scheduled scan)
/// and say nothing about the driver having actually run a turn. Named
/// explicitly on an allow-list, rather than inferred, so an unrecognized
/// trigger added later defaults to withholding driver evidence and denying
/// positive liveness, not asserting either. Shared by
/// [`crate::app::readoption`]'s driver-evidence classification and its
/// positive-liveness gate — both decisions must agree about the same
/// trigger, so there is exactly one list.
pub const NON_HOOK_TRIGGERS: [&str; 3] = [
    "redispatch_guard",
    "durable_state_scan",
    crate::tmux_adoption::TERMINAL_HANDOFF_TRIGGER,
];

impl ContradictionVerdict {
    /// Stable identifier for dispatch-event details and trace fields.
    pub fn as_str(&self) -> &'static str {
        match self {
            ContradictionVerdict::Readopt => "readopt",
            ContradictionVerdict::Reap { .. } => "reap",
            ContradictionVerdict::NoContradiction => "no_contradiction",
        }
    }
}

/// Decide what to do about a worker that has proven itself alive.
///
/// `status` is the execution's current DB status. `work_item_terminal` says an
/// authoritative owner has already closed the bound work. `other_live_execution`
/// is the id of any OTHER `running`/`waiting_human` execution on the same work
/// item, if one exists. `trigger` names the detector (a hook kind, or a sweep
/// name). `positive_liveness` is true when something besides the trigger
/// itself shows a live process — an alive durable pid, a tmux-observed pane
/// pid, a registry slot, or a pane the app currently hosts.
///
/// Pure, so the policy is unit-testable without a DB, an app session, or a
/// real process — and so the policy lives in exactly one place rather than
/// being re-derived at each call site the way the pre-existing liveness checks
/// were.
pub fn classify_contradiction(
    status: &ExecutionStatus,
    work_item_terminal: bool,
    other_live_execution: Option<&str>,
    trigger: &str,
    positive_liveness: bool,
) -> ContradictionVerdict {
    if !status.is_terminal() {
        return ContradictionVerdict::NoContradiction;
    }
    // A newer live execution outranks the survivor regardless of how the
    // survivor's own row went terminal: two workers on one row is the state
    // this whole change exists to prevent, and the live one holds the
    // workspace lease.
    if other_live_execution.is_some() {
        return ContradictionVerdict::Reap {
            reason: ReapReason::SupersededByLiveExecution,
        };
    }
    if work_item_terminal {
        return ContradictionVerdict::Reap {
            reason: ReapReason::WorkItemTerminal,
        };
    }
    // SessionEnd is the worker announcing its exit — a process boundary, not
    // a turn boundary. Treating it as DriverHook-class liveness is what
    // resurrected idle-parked executions into unreapable `running` phantoms.
    if trigger == SESSION_END_TRIGGER {
        return ContradictionVerdict::Reap {
            reason: ReapReason::TerminalByDecision,
        };
    }
    match status {
        // Only `orphaned` is a genuine death-inferred-from-silence. The
        // idle-park path writes `abandoned` as a deliberate teardown.
        ExecutionStatus::Orphaned if positive_liveness => ContradictionVerdict::Readopt,
        ExecutionStatus::Orphaned => ContradictionVerdict::Reap {
            reason: ReapReason::NoPositiveLiveness,
        },
        // Deliberate outcomes. The record is authoritative; the process is not.
        _ => ContradictionVerdict::Reap {
            reason: ReapReason::TerminalByDecision,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(status: &ExecutionStatus, work_item_terminal: bool, other: Option<&str>) -> ContradictionVerdict {
        classify_contradiction(status, work_item_terminal, other, "stop", true)
    }

    #[test]
    fn a_non_terminal_execution_has_nothing_to_resolve() {
        for status in [
            ExecutionStatus::Running,
            ExecutionStatus::WaitingHuman,
            ExecutionStatus::Ready,
            ExecutionStatus::Claimed,
        ] {
            assert_eq!(
                classify(&status, false, None),
                ContradictionVerdict::NoContradiction,
                "{status} is live; engine and worker already agree",
            );
        }
    }

    /// The core rule: a status the engine *guessed* is withdrawn when the
    /// worker disproves it with positive liveness. Only `orphaned` is that
    /// guess; `abandoned` is a deliberate idle-park decision.
    #[test]
    fn inferred_terminals_are_readopted() {
        assert_eq!(
            classify(&ExecutionStatus::Orphaned, false, None),
            ContradictionVerdict::Readopt,
            "orphaned is an inference from missing signal, not a decision",
        );
    }

    /// A status somebody (or the worker, or idle-park) actually decided is
    /// never rewritten — the surviving process is what is wrong.
    #[test]
    fn deliberate_terminals_reap_the_survivor() {
        for status in [
            ExecutionStatus::Abandoned,
            ExecutionStatus::Cancelled,
            ExecutionStatus::Completed,
            ExecutionStatus::Failed,
        ] {
            assert_eq!(
                classify(&status, false, None),
                ContradictionVerdict::Reap {
                    reason: ReapReason::TerminalByDecision
                },
                "{status} is a deliberate outcome and must not be resurrected",
            );
        }
    }

    /// Anti-duplication invariant: once a replacement worker is live, the
    /// survivor loses — even though its own row went terminal by inference and
    /// would otherwise have been re-adopted. Re-adopting here is precisely how
    /// two workers end up on one row.
    #[test]
    fn a_live_replacement_outranks_an_otherwise_readoptable_survivor() {
        assert_eq!(
            classify(&ExecutionStatus::Orphaned, false, Some("exec_newer")),
            ContradictionVerdict::Reap {
                reason: ReapReason::SupersededByLiveExecution
            },
        );
    }

    #[test]
    fn a_closed_work_item_outranks_an_inferred_terminal_status() {
        assert_eq!(
            classify(&ExecutionStatus::Orphaned, true, None),
            ContradictionVerdict::Reap {
                reason: ReapReason::WorkItemTerminal,
            },
            "closed work must not be resurrected merely because the execution's own status was inferred",
        );
    }

    #[test]
    fn session_end_never_readopts_even_an_orphaned_row_with_positive_liveness() {
        assert_eq!(
            classify_contradiction(&ExecutionStatus::Orphaned, false, None, SESSION_END_TRIGGER, true,),
            ContradictionVerdict::Reap {
                reason: ReapReason::TerminalByDecision
            },
            "SessionEnd is the worker announcing exit, not proof of life",
        );
        assert_eq!(
            classify_contradiction(&ExecutionStatus::Abandoned, false, None, SESSION_END_TRIGGER, true,),
            ContradictionVerdict::Reap {
                reason: ReapReason::TerminalByDecision
            },
            "the idle-park teardown echo must not resurrect the row it just abandoned",
        );
    }

    #[test]
    fn orphaned_without_positive_liveness_is_reaped_not_readopted() {
        assert_eq!(
            classify_contradiction(&ExecutionStatus::Orphaned, false, None, "stop", false),
            ContradictionVerdict::Reap {
                reason: ReapReason::NoPositiveLiveness
            },
            "a hook with no live pid, no tmux pid, and no slot must not produce an unreapable running row",
        );
    }
}
