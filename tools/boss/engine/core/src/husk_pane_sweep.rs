//! Periodic reconciler that retires "husk" panes — worker panes the macOS
//! app is still hosting for a slot the engine has already forgotten.
//!
//! ## The gap this closes
//!
//! Every other reconciler in this crate (`dead_pid_sweep`, `spawn_ack_sweep`,
//! `terminal_work_sweep`, `pool_claim_sweep`, …) is driven by the ENGINE's
//! own bookkeeping: a `LiveWorkerStateRegistry` entry, or the worker pool's
//! own claim table. That bookkeeping is cleared unconditionally at the end
//! of [`crate::app::ServerState::release_worker_pane`] — "now that the pane
//! has been torn down — successfully or not — the engine and the app are
//! back in agreement that slot N is free" (see that function's own
//! comment). The "successfully or not" is the gap: if the app's
//! `ReleaseWorkerPane` RPC times out, the app session is transiently
//! unreachable, or a terminal-transition site (an ack timeout, a
//! fire-and-forget teardown task) clears engine state without the app RPC
//! ever landing, the engine's own state is clean — no live-state entry, no
//! pool claim — while the app is still physically hosting the pane. Nothing
//! ENGINE-STATE-DRIVEN can ever see this: `bossctl agents list` reads
//! exactly the same `LiveWorkerStateRegistry` the leak already cleared, and
//! `terminal_work_sweep`/`pool_claim_sweep` both iterate structures the leak
//! already emptied.
//!
//! The operator-observed 2026-07-14 incident (worker "O'Brien"'s exec
//! created 06:26:19Z, pane spawned only 06:31:22Z; dispatch showed twelve
//! `request_recorded` → `worker_claimed=skipped` cycles from 06:29:06 to
//! 06:31:10) is this shape: a slot the engine's pool considered free was
//! actually still occupied by a real app-hosted pane, so `SpawnWorkerPane`
//! for the next dispatch kept losing the race against `SlotBusy` until the
//! stray pane finally cleared. 77 occurrences of the
//! `[engine-reconcile] live hook event arrived for a TERMINAL execution`
//! WARN that same day (see [`crate::app::worker_events::dispatch_live_worker_state`])
//! are the same contradiction observed from the hook-fan-out side: a run the
//! engine had already terminalized was still alive and emitting hooks.
//!
//! [`crate::app::ServerState::list_husk_panes`] and
//! [`crate::app::ServerState::retire_pane`] already exist as the manual,
//! operator-invoked break-glass path (`bossctl agents list --all` /
//! `bossctl agents retire-pane`) for exactly this "husk" shape — but until
//! this sweep, nothing called them automatically. This sweep is the
//! backstop: it asks the APP what it hosts, diffs against the engine's live
//! set (the same diff `list_husk_panes` already performs), and — once a
//! slot has been reported a husk on two consecutive passes — retires it,
//! regardless of which terminal-transition site produced the divergence.
//!
//! ## Two-pass confirmation
//!
//! A slot the app just started hosting (a fresh `SpawnWorkerPane` whose
//! `register_spawn` call hasn't landed in `LiveWorkerStateRegistry` yet) can
//! transiently look like a husk. Requiring the SAME slot to appear on two
//! consecutive passes (mirroring [`crate::terminal_work_sweep`]) gives any
//! in-flight registration or teardown a full interval to resolve before this
//! sweep acts, and a slot that stops looking like a husk between passes
//! (registration landed, or the pane was cleared by something else) simply
//! drops out of the confirmed set.
//!
//! ## Cadence
//!
//! Runs every [`DEFAULT_INTERVAL`] and fires once immediately on boot, same
//! as every other sweep in this crate.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use boss_protocol::HostedPaneEntry;

use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};

/// How often the husk-pane sweep runs. 60s mirrors every other periodic
/// reconciler in this crate; the two-pass confirmation guard means the
/// earliest a genuine husk is retired is one interval after it is first
/// observed.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Abstracts the app round-trips this sweep needs so it is unit-testable
/// without a full `ServerState`/app session. Implemented by
/// [`crate::app::ServerState`] in `app/server.rs`.
#[async_trait::async_trait]
pub trait HuskPaneSweepSource: Send + Sync {
    /// List the slots the app currently hosts a session in that the engine
    /// has no live-tracked run for (the same diff
    /// [`crate::app::ServerState::list_husk_panes`] performs). `None` means
    /// the lookup itself failed (e.g. no app session registered, transport
    /// error) — treated as "skip this pass", never as "no husks", so a
    /// transient app-side hiccup can't be misread as an all-clear.
    async fn list_husk_candidates(&self) -> Option<Vec<HostedPaneEntry>>;

    /// Retire the husk pane hosted in `slot_id` — the same teardown
    /// [`crate::app::ServerState::retire_pane`] performs. Idempotent: a slot
    /// the app already cleared (or that raced back to being live-tracked) is
    /// a no-op there.
    async fn retire_husk(&self, slot_id: u8);
}

/// Counts from one sweep pass; logged at `info` when any pane was retired.
#[derive(Debug, Default)]
pub struct HuskPaneSweepOutcome {
    /// Confirmed husks (seen on two consecutive passes) retired this pass.
    pub retired: usize,
    /// Husks observed for the first time this pass; held for one more pass
    /// before any retirement (two-pass confirmation).
    pub pending_confirmation: usize,
    /// `true` when this pass's `list_husk_candidates` call failed and the
    /// pass was skipped conservatively.
    pub list_failed: bool,
}

impl crate::sweep_loop::SweepOutcome for HuskPaneSweepOutcome {
    fn has_activity(&self) -> bool {
        self.retired > 0
    }

    fn log(&self) {
        tracing::info!(
            retired = self.retired,
            pending_confirmation = self.pending_confirmation,
            "husk-pane sweep: retired app-hosted pane(s) the engine no longer tracks",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`,
/// threading the cross-pass confirmation set so it survives between passes.
pub fn spawn_loop(
    source: Arc<dyn HuskPaneSweepSource>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let seen_husks: Arc<tokio::sync::Mutex<HashSet<u8>>> = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let source = Arc::clone(&source);
        let dispatch_events = Arc::clone(&dispatch_events);
        let seen_husks = Arc::clone(&seen_husks);
        async move {
            let mut seen_husks = seen_husks.lock().await;
            run_one_pass(source.as_ref(), dispatch_events.as_ref(), &mut seen_husks).await
        }
    })
}

/// Run a single husk-pane reconciliation pass. `seen_husks` carries the set
/// of slot ids observed as husks on the *previous* pass; on return it holds
/// this pass's candidates so the next pass can confirm them. Returns a
/// summary; callers may log it.
pub async fn run_one_pass(
    source: &dyn HuskPaneSweepSource,
    dispatch_events: &dyn DispatchEventSink,
    seen_husks: &mut HashSet<u8>,
) -> HuskPaneSweepOutcome {
    let mut outcome = HuskPaneSweepOutcome::default();

    let candidates = match source.list_husk_candidates().await {
        Some(panes) => panes,
        None => {
            outcome.list_failed = true;
            // Conservative: leave `seen_husks` untouched rather than
            // clearing it. A transient lookup failure sandwiched between two
            // genuine husk observations should not restart the two-pass
            // wait from scratch.
            return outcome;
        }
    };

    let mut current_candidates: HashSet<u8> = HashSet::new();

    for pane in candidates {
        current_candidates.insert(pane.slot_id);

        if !seen_husks.contains(&pane.slot_id) {
            outcome.pending_confirmation += 1;
            tracing::debug!(
                slot_id = pane.slot_id,
                run_id = %pane.run_id,
                "husk-pane sweep: app-hosted pane with no engine-tracked run observed; \
                 awaiting next-pass confirmation before retiring",
            );
            continue;
        }

        tracing::warn!(
            slot_id = pane.slot_id,
            run_id = %pane.run_id,
            task_title = ?pane.task_title,
            "husk-pane sweep: app-hosted pane outlived engine tracking; retiring and freeing slot",
        );

        source.retire_husk(pane.slot_id).await;
        outcome.retired += 1;

        dispatch_events
            .emit(
                DispatchEvent::new(Stage::HuskPaneReconcile, Outcome::Ok, pane.run_id.clone())
                    .with_worker(crate::coordinator::worker_id_for_slot(pane.slot_id))
                    .with_details(serde_json::json!({
                        "slot_id": pane.slot_id,
                        "task_title": pane.task_title,
                    })),
            )
            .await;
    }

    *seen_husks = current_candidates;
    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::dispatch_events::RecordingDispatchEventSink;

    fn husk(slot_id: u8, run_id: &str) -> HostedPaneEntry {
        HostedPaneEntry {
            slot_id,
            run_id: run_id.to_owned(),
            summary: None,
            task_title: Some("test chore".to_owned()),
        }
    }

    /// Test double that returns a scripted sequence of `list_husk_candidates`
    /// results (one per pass) and records every `retire_husk` call.
    struct ScriptedSource {
        passes: Mutex<std::collections::VecDeque<Option<Vec<HostedPaneEntry>>>>,
        retired: Mutex<Vec<u8>>,
    }

    impl ScriptedSource {
        fn new(passes: Vec<Option<Vec<HostedPaneEntry>>>) -> Self {
            Self {
                passes: Mutex::new(passes.into()),
                retired: Mutex::new(Vec::new()),
            }
        }

        fn retired(&self) -> Vec<u8> {
            self.retired.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HuskPaneSweepSource for ScriptedSource {
        async fn list_husk_candidates(&self) -> Option<Vec<HostedPaneEntry>> {
            self.passes.lock().unwrap().pop_front().flatten()
        }

        async fn retire_husk(&self, slot_id: u8) {
            self.retired.lock().unwrap().push(slot_id);
        }
    }

    /// The core invariant: a husk observed on two consecutive passes is
    /// retired on the second, not the first.
    #[tokio::test]
    async fn retires_husk_confirmed_across_two_passes() {
        let source = ScriptedSource::new(vec![Some(vec![husk(7, "exec-a")]), Some(vec![husk(7, "exec-a")])]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        let first = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(first.retired, 0, "first pass must only record the candidate");
        assert_eq!(first.pending_confirmation, 1);
        assert!(source.retired().is_empty());
        assert!(sink.events().await.is_empty());

        let second = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(second.retired, 1, "second pass must retire the confirmed husk");
        assert_eq!(source.retired(), vec![7]);

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "husk_pane_reconcile");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].execution_id, "exec-a");
        assert_eq!(events[0].details["slot_id"], 7);
    }

    /// A husk that disappears before confirmation (the registration landed,
    /// or something else cleared it) is never retired.
    #[tokio::test]
    async fn does_not_retire_when_husk_clears_before_confirmation() {
        let source = ScriptedSource::new(vec![Some(vec![husk(3, "exec-b")]), Some(vec![])]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        let first = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(first.pending_confirmation, 1);

        let second = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(second.retired, 0, "a cleared husk must not be retired");
        assert_eq!(second.pending_confirmation, 0);
        assert!(source.retired().is_empty());
    }

    /// A lookup failure is a conservative skip that preserves the
    /// confirmation set — a transient blip between two genuine observations
    /// must not restart the two-pass wait.
    #[tokio::test]
    async fn lookup_failure_preserves_confirmation_set() {
        let source = ScriptedSource::new(vec![Some(vec![husk(5, "exec-c")]), None, Some(vec![husk(5, "exec-c")])]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        let first = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(first.pending_confirmation, 1);

        let second = run_one_pass(&source, &sink, &mut seen).await;
        assert!(second.list_failed);
        assert_eq!(second.retired, 0);
        assert_eq!(seen.len(), 1, "seen set must survive the failed pass unchanged");

        let third = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(
            third.retired, 1,
            "the pre-blip observation must still count toward confirmation"
        );
        assert_eq!(source.retired(), vec![5]);
    }

    /// No husks at all across several passes is simply quiet.
    #[tokio::test]
    async fn no_husks_is_a_no_op() {
        let source = ScriptedSource::new(vec![Some(vec![]), Some(vec![])]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        for _ in 0..2 {
            let outcome = run_one_pass(&source, &sink, &mut seen).await;
            assert_eq!(outcome.retired, 0);
            assert_eq!(outcome.pending_confirmation, 0);
        }
        assert!(source.retired().is_empty());
        assert!(sink.events().await.is_empty());
    }

    /// Two distinct husks confirmed in the same pass are both retired, each
    /// emitting its own dispatch event.
    #[tokio::test]
    async fn retires_multiple_confirmed_husks_independently() {
        let source = ScriptedSource::new(vec![
            Some(vec![husk(1, "exec-x"), husk(2, "exec-y")]),
            Some(vec![husk(1, "exec-x"), husk(2, "exec-y")]),
        ]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &sink, &mut seen).await;
        let second = run_one_pass(&source, &sink, &mut seen).await;

        assert_eq!(second.retired, 2);
        let mut retired = source.retired();
        retired.sort_unstable();
        assert_eq!(retired, vec![1, 2]);
        assert_eq!(sink.events().await.len(), 2);
    }
}
