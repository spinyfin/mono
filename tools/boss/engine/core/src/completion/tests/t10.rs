//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).
//!
//! **Theme: a worker that finishes must terminalize, even when it
//! finishes with nothing to show.**
//!
//! These pin the fix for the 2026-08-12 stall in which a worker reached a
//! correct terminal conclusion ("CI is already green; no code change
//! needed"), said so twice — the second time in direct reply to the
//! engine's own re-prompt — and then held an interactive slot for 2h40m
//! with the execution never leaving `running`.
//!
//! Two independent defects had to line up:
//!
//! 1. **The engine asked in a language it cannot read.** The re-prompt
//!    (`probe_push_to_existing_pr`) ended *"there is nothing left to do,
//!    say so — explain your status"*, inviting prose; the engine's only
//!    terminal for that state is an own-line
//!    [`NO_CHANGES_NEEDED`](crate::no_op_signal::NO_CHANGES_NEEDED_MARKER)
//!    match, which explicitly rejects prose. Compliance was unparseable.
//! 2. **The nudge ladder had an absorbing state.** The reply landed inside
//!    [`crate::nudge_breaker::MIN_RENUDGE_INTERVAL`], so the boundary
//!    produced `NudgeDebounced` — a decision to wait for "the next Stop".
//!    But the only producer of a next Stop is a probe, and the debounce is
//!    the choice not to send one. The one record that could have re-driven
//!    the ladder (the tracked nudge intent) was then retired by the
//!    background-children watermark rule on the next sweep, leaving
//!    nothing anywhere scheduled to look at the execution again.

use super::*;

/// Manually-advanced fake clock for the debounce window. Unlike
/// [`super::auto_advancing_clock`] (which steps forward on every read, so
/// the breaker never debounces) this holds still until a test explicitly
/// advances it — the only way to exercise a ladder that must survive a
/// debounce and then proceed once the interval genuinely elapses.
#[derive(Clone)]
struct ManualClock(Arc<std::sync::Mutex<std::time::Instant>>);

impl ManualClock {
    fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(std::time::Instant::now())))
    }

    fn advance_past_debounce(&self) {
        let mut guard = self.0.lock().expect("ManualClock mutex poisoned");
        *guard += crate::nudge_breaker::MIN_RENUDGE_INTERVAL + std::time::Duration::from_secs(1);
    }

    fn now_fn(&self) -> Arc<dyn Fn() -> std::time::Instant + Send + Sync> {
        let inner = self.0.clone();
        Arc::new(move || *inner.lock().expect("ManualClock mutex poisoned"))
    }
}

/// The engine's own probe text must name the exact marker its own parser
/// accepts.
///
/// This is the incident's first defect as a one-line invariant: a probe
/// that says "say so" and nothing more gets prose, and
/// [`crate::no_op_signal::transcript_signals_no_op`] deliberately does not
/// match prose (`marker_mentioned_in_prose_does_not_match`). The two ends
/// of that exchange must not be allowed to drift apart again.
///
/// Also asserts the converse for each probe: the engine's *own* text must
/// not itself satisfy the own-line match, or a transcript that merely
/// echoed the probe back would read as a worker terminal.
#[test]
fn probe_texts_name_the_no_op_marker() {
    let marker = crate::no_op_signal::NO_CHANGES_NEEDED_MARKER;
    let bound = probe_push_to_existing_pr("https://github.com/spinyfin/mono/pull/2622");

    for (label, text) in [
        ("PROBE_NO_PR", PROBE_NO_PR),
        ("probe_push_to_existing_pr", bound.as_str()),
    ] {
        assert!(
            text.contains(marker),
            "{label} must name the `{marker}` marker verbatim — a probe that only invites prose \
             asks for an answer no engine path can act on:\n{text}",
        );
        assert!(
            !crate::no_op_signal::transcript_signals_no_op(text),
            "{label} must keep the marker inline (backticked), never on a line of its own — the \
             engine's question must not be mistakable for the worker's answer:\n{text}",
        );
    }
}

/// The core regression: a debounced nudge must survive the worker's
/// trailing tool activity, and the recurring sweep must then advance the
/// ladder that no Stop boundary could have advanced.
///
/// In the incident the worker's last hook landed one second *after* the
/// debounced boundary. The old code read any watermark change as "the
/// worker resumed" and retired the intent outright, dropping the execution
/// from `pending_background_nudge_execution_ids()` permanently. The rule
/// now splits by [`NudgeHold`]: activity defers a debounced hold (and
/// re-records it against the new watermark), quiescence advances it.
#[tokio::test]
async fn debounced_nudge_survives_trailing_tool_activity_and_the_sweep_advances_it() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db, detector);
    // Zero descendants: background-children suppression is out of the
    // picture, so the only hold in play is the debounce itself.
    let probe = ToggleWatermarkProbe::new(0, Some("wm-before-stop"));
    let clock = ManualClock::new();
    let handler = handler
        .with_background_activity_probe(probe.clone())
        .with_now_fn(clock.now_fn());

    // Boundary 1: the ordinary nudge fires.
    assert!(
        matches!(handler.on_stop(&execution_id).await, StopOutcome::AwaitingInput),
        "the first boundary must nudge normally",
    );
    // Boundary 2: the worker's reply lands seconds later, inside the
    // debounce window. This is the boundary the incident died on.
    assert!(
        matches!(handler.on_stop(&execution_id).await, StopOutcome::NudgeDebounced),
        "a reply inside the re-nudge interval must debounce",
    );
    assert_eq!(probes.snapshot().len(), 1, "the debounced boundary must not re-probe");

    // The trailing hook the incident recorded at 18:32:14 — one second
    // after the debounced boundary.
    probe.set_watermark(Some("wm-after-stop"));

    // Sweep pass 1: activity defers. It must NOT retire the intent — that
    // retirement is what left the execution with nothing watching it.
    let deferred = handler.recheck_background_nudge(&execution_id).await;
    assert!(
        matches!(deferred, Some(StopOutcome::NudgeDebounced)),
        "tool activity must defer a debounced hold, not fire a nudge over live work; got {deferred:?}",
    );
    assert!(
        handler.pending_background_nudge_execution_ids().contains(&execution_id),
        "a debounced hold must NEVER be retired on hook activity — it is the only record that \
         can re-drive an execution whose next Stop can only come from a probe",
    );
    assert_eq!(probes.snapshot().len(), 1, "the deferring pass must not probe");

    // The worker now goes quiet (no further tool activity) and the
    // debounce interval genuinely elapses.
    clock.advance_past_debounce();

    let advanced = handler.recheck_background_nudge(&execution_id).await;
    assert!(
        matches!(advanced, Some(StopOutcome::AwaitingInput)),
        "a quiescent worker past the re-nudge interval must advance the ladder; got {advanced:?}",
    );
    let queued = probes.snapshot();
    assert_eq!(queued.len(), 2, "the sweep must queue the held probe; got {queued:?}");
    assert_eq!(queued[1].1, PROBE_NO_PR);
    assert_eq!(
        probes.deliver_snapshot(),
        [execution_id.as_str()],
        "a probe queued off a Stop fan-out must be delivered out-of-band — nothing else will \
         ever carry it to an idle pane",
    );
}

/// End-to-end on the incident's own shape: a worker that concludes with no
/// commit and no PR terminalizes, releases its slot, and settles its row.
///
/// Sequenced exactly as the incident ran, with the fix in place: prose
/// conclusion → nudge → prose again inside the debounce → sweep re-drives
/// the ladder and delivers the (now marker-naming) probe → the worker
/// answers with the sanctioned marker → clean terminal. The row lands on
/// `done`, not back on `todo`, so it is not re-dispatched to redo work that
/// is already complete.
#[tokio::test]
async fn worker_that_finishes_with_no_commit_and_no_pr_terminalizes_and_settles_its_row() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // What the incident worker actually wrote: a correct conclusion, in
    // prose, with no marker.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "## Summary\nCI is already green on the bound PR; no code change is needed.\n",
    );
    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler,
        cube,
        pane,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let probe = ToggleWatermarkProbe::new(0, Some("wm-0"));
    let clock = ManualClock::new();
    let handler = handler
        .with_background_activity_probe(probe.clone())
        .with_now_fn(clock.now_fn());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));
    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::NudgeDebounced
    ));

    clock.advance_past_debounce();
    assert!(
        matches!(
            handler.recheck_background_nudge(&execution_id).await,
            Some(StopOutcome::AwaitingInput)
        ),
        "the sweep must re-drive the stalled ladder",
    );
    assert_eq!(
        probes.deliver_snapshot(),
        [execution_id.as_str()],
        "the re-driven probe must actually reach the parked worker",
    );

    // The worker reads a probe that now names the marker, and answers in
    // the language the engine can read.
    write_assistant_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "## Summary\nNo local changes or commits remain to push; the bound PR is already \
         green.\n\nNO_CHANGES_NEEDED\n",
    );

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::NoChangesNeeded { work_item_id } if work_item_id == &chore_id),
        "a worker that finished with no artifact must reach the no-op terminal; got {outcome:?}",
    );

    // Terminalized.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.finished_at.is_some());
    // Slot released — lease and pane both torn down, which is what
    // `bossctl agents list` reads as a free slot.
    assert!(execution.cube_lease_id.is_none(), "the cube lease must be released");
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
    // Row settled, not recycled: `done` (and no fabricated pr_url), so the
    // rescan cannot dispatch a fresh worker to repeat completed work — the
    // downstream consequence of cancelling the run by hand, which dropped
    // the row back to `todo`.
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(chore) => {
            assert_eq!(chore.status, TaskStatus::Done, "a finished no-op must settle the row");
            assert!(chore.pr_url.is_none(), "a no-op must not fabricate a pr_url");
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// The ladder must terminate even if the worker never answers at all.
///
/// The sweep re-drive is not a timeout that kills long-idle work; it hands
/// the existing, already-bounded nudge/park ladder back its ability to
/// advance. That ladder's own terminal —
/// [`crate::nudge_breaker::DEFAULT_MAX_UNPRODUCTIVE_NUDGES`] consecutive
/// unproductive nudges — must still be reached from the sweep path, so a
/// silent worker releases its slot instead of holding it indefinitely.
#[tokio::test]
async fn a_silent_worker_still_reaches_the_park_terminal_from_the_sweep_path() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector);
    // Silent worker: watermark never moves, so every pass is quiescent.
    let probe = ToggleWatermarkProbe::new(0, Some("wm-frozen"));
    let clock = ManualClock::new();
    let handler = handler
        .with_background_activity_probe(probe)
        .with_now_fn(clock.now_fn());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));
    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::NudgeDebounced
    ));

    // Drive sweeps until the breaker trips. Bounded well above
    // DEFAULT_MAX_UNPRODUCTIVE_NUDGES so a failure reads as "never
    // terminalizes" rather than hanging the suite.
    let mut final_outcome = None;
    for _ in 0..(crate::nudge_breaker::ABSOLUTE_MAX_NUDGES + 2) {
        clock.advance_past_debounce();
        let outcome = handler.recheck_background_nudge(&execution_id).await;
        if matches!(outcome, Some(StopOutcome::NudgeBreakerParked { .. })) {
            final_outcome = outcome;
            break;
        }
    }
    assert!(
        final_outcome.is_some(),
        "a silent worker must reach the park terminal, not hold its slot forever",
    );

    let execution = db.get_execution(&execution_id).unwrap();
    assert!(
        execution.status.is_terminal(),
        "the parked execution must be terminal; got {}",
        execution.status,
    );
    assert!(
        execution.cube_lease_id.is_none(),
        "the park must release the cube lease"
    );
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert!(
        handler.pending_background_nudge_execution_ids().is_empty(),
        "a concluded ladder must not leave an intent behind",
    );
}

/// Counterfactual to the sweep re-drive: an execution that is genuinely
/// still working must be left alone.
///
/// The `BackgroundChildren` hold keeps its original retirement rule — hook
/// activity is exactly the resumption it was betting on — so a worker whose
/// delegated subagent reported back is dropped from the recheck set without
/// a probe. Only the `Debounced` hold changed. Pinned alongside the
/// re-drive so the split cannot be collapsed back into one rule.
#[tokio::test]
async fn a_resumed_background_children_hold_is_still_retired_without_a_probe() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, probes, .. } = TestHarness::new(db, detector);
    let probe = WatermarkedDescendantProbe::new("wm-before");
    let handler = handler.with_background_activity_probe(probe.clone());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::BackgroundChildrenPending { .. }
    ));
    probe.set_watermark("wm-after");

    assert_eq!(
        handler.recheck_background_nudge(&execution_id).await,
        None,
        "a background-children hold whose worker resumed must still retire",
    );
    assert!(handler.pending_background_nudge_execution_ids().is_empty());
    assert!(
        probes.snapshot().is_empty() && probes.deliver_snapshot().is_empty(),
        "a worker that is genuinely still working must be neither probed nor delivered to",
    );
}
