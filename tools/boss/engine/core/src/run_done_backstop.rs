//! The backstop under the `boss propose done` declaration: what the engine
//! does about a run that stops without ever declaring itself finished.
//!
//! ## Why a backstop is mandatory
//!
//! Gating completion on a declaration
//! ([`boss_protocol::ProposalKind::RunDone`]) fixes the failure where the
//! engine *inferred* a run was finished from state that predated it. It
//! cannot fix the opposite failure on its own: a worker can crash, wedge,
//! exhaust its context, or simply forget the verb. A gate that waits
//! unconditionally on a declaration that never arrives hangs those runs
//! forever — strictly worse than the inference it replaced. So the
//! declaration handles the common case and this module handles the rest.
//!
//! ## What this module is NOT
//!
//! It is **not a new liveness detector**, and deliberately not the old
//! completion gate with a longer timer. The engine already has detectors
//! that answer "is this worker dead or wedged?" from liveness signals
//! rather than from PR state, and they are much better at it than any
//! completion-path inference:
//!
//! - [`crate::stale_worker_sweep`] — 30 minutes with no hook event while
//!   `activity == Working` and no tool in flight ⇒ `orphaned`. This is the
//!   detector for the worker that stopped emitting anything at all.
//! - [`crate::dead_pid_sweep`] / [`crate::husk_pane_sweep`] — the process
//!   or its pane is gone.
//! - [`crate::worker_readoption`] — a live worker for an execution the
//!   engine believes is dead is re-adopted, not reaped.
//!
//! Those already ran under the old behaviour; the problem was that the
//! completion path *pre-empted* them by declaring success first. Removing
//! that pre-emption is most of the fix. What is left for this module is the
//! narrow in-band question the sweeps cannot answer: at a Stop boundary,
//! for a run that has not declared, is the worker still working — and if it
//! has genuinely stopped, how do we end the run **visibly** rather than
//! silently as a success?
//!
//! ## The decision, and why it is far less aggressive than what it replaces
//!
//! Today an undeclared Stop either finalizes the run (wrongly, from PR
//! health) or enters the auto-nudge loop, which re-probes on essentially
//! every Stop until the circuit breaker trips. [`decide`] instead:
//!
//! 1. **Holds quietly** while the worker shows any sign of life — live
//!    descendant processes (a subagent still running, the exact state the
//!    md5 incident was in when it was killed) or build-wait narration. No
//!    probe, no breaker consumption, no finalize. The silence clock is
//!    reset, because a working worker is not a silent one.
//! 2. **Holds quietly** for the whole [`declaration_horizon_secs`] window
//!    even without those signals. A worker between turns, thinking, or
//!    reading a large file looks identical to a stalled one at any single
//!    Stop; only elapsed silence distinguishes them. This is where the
//!    aggression goes: what used to be "act on every Stop" becomes "act
//!    once, after the horizon".
//! 3. **Asks, once the horizon elapses** — a probe naming the declaration
//!    verb. Asking is preferred to deciding: the worker is the only party
//!    that knows whether it is finished, and the entire point of the
//!    declaration is to stop the engine guessing on its behalf.
//! 4. **Escalates** when the ask goes unanswered. The existing circuit
//!    breaker bounds the asking and parks the execution for a human. A
//!    parked run is `waiting_human` with an attention item — never
//!    `completed`. That is the defined, visible outcome a missing
//!    declaration must have; it is emphatically not a silent success.
//!
//! Safety property preserved: nothing here reaps a worker. Step 1 is the
//! explicit guard for "alive and working", and steps 3–4 route through
//! `nudge_or_park`, which is not a reap path.

use std::sync::Arc;

use crate::build_wait_tracker::{BuildWaitDecision, BuildWaitTracker};
use boss_protocol::ExecutionKind;

/// How long the engine waits, in continuous silence at Stop boundaries,
/// before asking an undeclared run whether it is finished.
///
/// 20 minutes. The number is chosen against what it is discriminating, not
/// against the nudge breaker's cadence: a worker doing real work emits Stop
/// boundaries every few minutes at most, and each of those either declares
/// (ending the wait) or shows life (resetting the clock). Twenty minutes of
/// neither, from a worker whose process tree is idle, is a real signal.
///
/// It is deliberately shorter than [`crate::stale_worker_sweep`]'s 30-minute
/// wedge threshold so the *ask* — the cheap, recoverable action — always
/// gets a turn before the sweep's `orphaned` verdict, and never the other
/// way round.
pub const DEFAULT_DECLARATION_HORIZON_SECS: i64 = 20 * 60;

/// The ask must always get a turn before [`crate::stale_worker_sweep`]
/// reaches its own verdict, so the cheap, recoverable action precedes the
/// irreversible one. A compile-time assertion rather than a test: if either
/// constant is ever retuned, the ordering between them is the invariant to
/// preserve, and a build failure states that at the moment of the edit.
const _: () = assert!(
    DEFAULT_DECLARATION_HORIZON_SECS < crate::stale_worker_sweep::DEFAULT_STALE_THRESHOLD_SECS,
    "the run-done ask must fire before the stale-worker sweep's `orphaned` verdict",
);

/// The horizon for a `revision_implementation`. 40 minutes — double the
/// default.
///
/// Revisions are where this bites hardest and where patience is cheapest.
/// A revision is dispatched into a PR that is *already* open, mergeable and
/// CI-clean, so it has no observable state of its own to move until the
/// moment it pushes: the engine has strictly less to go on than for a
/// primary implementation, whose branch either has a PR or does not. Both
/// confirmed incidents were revisions mid-investigation, and an
/// investigation that reads code and spawns a research subagent can
/// legitimately go a long time between externally-visible events.
///
/// The asymmetry is safe in the direction it errs: waiting too long on a
/// dead revision costs a held worker slot that [`crate::stale_worker_sweep`]
/// reclaims anyway; asking too early costs an interrupted investigation,
/// which is the failure being fixed.
pub const REVISION_DECLARATION_HORIZON_SECS: i64 = 40 * 60;

/// How long the engine waits for `kind` before asking. See the two
/// constants for the reasoning; every kind other than
/// `revision_implementation` takes the default.
///
/// Exhaustive match rather than a `_` arm: a new execution kind should have
/// to state its patience, since "how long can this legitimately go quiet?"
/// is exactly the question a new kind's author has just answered for
/// themselves.
pub fn declaration_horizon_secs(kind: &ExecutionKind) -> i64 {
    match kind {
        ExecutionKind::RevisionImplementation => REVISION_DECLARATION_HORIZON_SECS,
        ExecutionKind::AnswerAgent
        | ExecutionKind::AutomationTriage
        | ExecutionKind::ChoreImplementation
        | ExecutionKind::CiRemediation
        | ExecutionKind::ConflictResolution
        | ExecutionKind::InvestigationImplementation
        | ExecutionKind::PrReview
        | ExecutionKind::ProductDesign
        | ExecutionKind::ProjectDesign
        | ExecutionKind::TaskImplementation => DEFAULT_DECLARATION_HORIZON_SECS,
    }
}

/// What [`decide`] concluded about an undeclared run at this Stop boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackstopDecision {
    /// The worker is demonstrably still working — live descendant
    /// processes. Hold silently; the silence clock has been reset.
    /// `descendant_count` is what the process-tree probe found.
    HoldingWorkerActive { descendant_count: usize },
    /// No sign of activity, but the silence horizon has not elapsed. Hold
    /// silently: no probe, no breaker consumption, no finalize.
    HoldingWithinHorizon { waited_secs: i64, horizon_secs: i64 },
    /// The horizon has elapsed with no declaration and no sign of life.
    /// Ask the worker whether it is finished, bounded by the caller's
    /// circuit breaker.
    Ask { waited_secs: i64, horizon_secs: i64 },
}

/// Decide what to do about `execution_id`, which has stopped without
/// declaring itself done.
///
/// `descendant_count` is the caller's process-tree reading (0 when the
/// probe found nothing, including on platforms where it cannot look).
/// `now_epoch_secs` is passed in rather than read here so the caller — and
/// tests — control the clock.
///
/// Callers must only reach this for a run with no `run_done` declaration;
/// a declared run never gets here, and calling with one would start a
/// silence clock for a question already answered.
pub fn decide(
    tracker: &BuildWaitTracker,
    execution_id: &str,
    kind: &ExecutionKind,
    descendant_count: usize,
    now_epoch_secs: i64,
) -> BackstopDecision {
    if descendant_count > 0 {
        // A worker waiting on its own subagent is working, not stalled —
        // and the clock restarts, because the next quiet stretch should be
        // measured from when the background work ended, not from the first
        // Stop that happened to precede it.
        tracker.forget(execution_id);
        return BackstopDecision::HoldingWorkerActive { descendant_count };
    }
    let horizon_secs = declaration_horizon_secs(kind);
    match tracker.record(execution_id, now_epoch_secs, horizon_secs) {
        BuildWaitDecision::Suppress { waited_secs } => BackstopDecision::HoldingWithinHorizon {
            waited_secs,
            horizon_secs,
        },
        BuildWaitDecision::Expired { waited_secs } => BackstopDecision::Ask {
            waited_secs,
            horizon_secs,
        },
    }
}

/// Probe text for the ask. Deliberately phrased so a worker that is still
/// working has an honest, cheap answer available — the failure mode being
/// avoided is a probe that reads as an accusation and pushes a mid-
/// investigation worker into wrapping up prematurely, which is the same
/// harm as terminalizing it.
pub const PROBE_DECLARE_DONE: &str = "You have not declared this run finished. If you are done, run \
`boss propose done --outcome <delivered|no-changes-needed|blocked> --summary \"<one line>\"` — that \
declaration, not the PR's state, is what ends the run. If you are still working, say so and carry \
on; nothing is being taken away from you.";

/// Type-erased handle to the backstop's silence tracker, so the completion
/// handler can hold one without depending on the concrete tracker type
/// beyond what it already does for build-wait and background-children.
pub type DeclarationSilenceTracker = Arc<BuildWaitTracker>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_with_live_subagents_is_held_not_asked() {
        let tracker = BuildWaitTracker::new();
        // Long past any horizon: elapsed time must not matter while the
        // process tree says the worker is working. This is the md5 case.
        let decision = decide(
            &tracker,
            "exec_1",
            &ExecutionKind::RevisionImplementation,
            2,
            10_000_000,
        );
        assert_eq!(decision, BackstopDecision::HoldingWorkerActive { descendant_count: 2 });
    }

    #[test]
    fn silence_inside_the_horizon_holds_quietly() {
        let tracker = BuildWaitTracker::new();
        let first = decide(&tracker, "exec_1", &ExecutionKind::ChoreImplementation, 0, 1_000);
        assert_eq!(
            first,
            BackstopDecision::HoldingWithinHorizon {
                waited_secs: 0,
                horizon_secs: DEFAULT_DECLARATION_HORIZON_SECS,
            }
        );
        let later = decide(&tracker, "exec_1", &ExecutionKind::ChoreImplementation, 0, 1_060);
        assert_eq!(
            later,
            BackstopDecision::HoldingWithinHorizon {
                waited_secs: 60,
                horizon_secs: DEFAULT_DECLARATION_HORIZON_SECS,
            }
        );
    }

    #[test]
    fn silence_past_the_horizon_asks() {
        let tracker = BuildWaitTracker::new();
        decide(&tracker, "exec_1", &ExecutionKind::ChoreImplementation, 0, 1_000);
        let decision = decide(
            &tracker,
            "exec_1",
            &ExecutionKind::ChoreImplementation,
            0,
            1_000 + DEFAULT_DECLARATION_HORIZON_SECS,
        );
        assert_eq!(
            decision,
            BackstopDecision::Ask {
                waited_secs: DEFAULT_DECLARATION_HORIZON_SECS,
                horizon_secs: DEFAULT_DECLARATION_HORIZON_SECS,
            }
        );
    }

    /// Background activity does not merely skip one evaluation — it resets
    /// the clock, so a worker that spends an hour on subagent work and then
    /// goes quiet gets the full horizon of quiet before it is asked, not
    /// an immediate ask on the strength of the hour it spent working.
    #[test]
    fn background_activity_restarts_the_silence_clock() {
        let tracker = BuildWaitTracker::new();
        decide(&tracker, "exec_1", &ExecutionKind::ChoreImplementation, 0, 1_000);
        decide(&tracker, "exec_1", &ExecutionKind::ChoreImplementation, 1, 2_000);
        let after = decide(&tracker, "exec_1", &ExecutionKind::ChoreImplementation, 0, 2_100);
        assert_eq!(
            after,
            BackstopDecision::HoldingWithinHorizon {
                waited_secs: 0,
                horizon_secs: DEFAULT_DECLARATION_HORIZON_SECS,
            },
            "the clock must restart from the end of the background work"
        );
    }

    #[test]
    fn a_revision_gets_a_longer_horizon_than_a_primary_implementation() {
        assert!(
            declaration_horizon_secs(&ExecutionKind::RevisionImplementation)
                > declaration_horizon_secs(&ExecutionKind::ChoreImplementation),
            "revisions have less observable state of their own; they get more patience"
        );
    }
}
