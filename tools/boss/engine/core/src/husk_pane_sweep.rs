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
//! transiently look like a husk. Requiring the SAME `(slot_id, run_id)` pair
//! to appear on two consecutive passes (mirroring
//! [`crate::terminal_work_sweep`]) gives any in-flight registration or
//! teardown a full interval to resolve before this sweep acts, and a slot
//! that stops looking like a husk between passes (registration landed, or
//! the pane was cleared by something else) simply drops out of the
//! confirmed set. Keying on the pair (not slot id alone) means a slot whose
//! husk run changes between passes — the original husk cleared and a
//! different leaked run took the same slot — resets the confirmation clock
//! for the new run instead of being mistaken for the same husk observed
//! twice.
//!
//! ## Liveness corroboration (why "the engine forgot it" is not enough)
//!
//! Two-pass confirmation guards against a *transient* bookkeeping gap. It
//! does nothing about a bookkeeping gap that is simply WRONG and stays
//! wrong, which is what happened on 2026-07-26: six live workers received a
//! synchronized `SessionEnd { reason: "other" }` burst inside 250ms while
//! their `claude` processes kept running. `apply_event` flipped each to
//! `WorkerActivity::Terminated`, [`crate::app::ServerState::list_husk_panes`]
//! filters terminal entries out of its live set, and so five slots looked
//! like husks on two consecutive passes and were retired 107 seconds later —
//! SIGTERMing five workers mid-work, three of them inside a foreground
//! `bazel` build. `retire_pane`'s own guard re-read the same wrong
//! bookkeeping and agreed.
//!
//! The lesson is that a sweep whose action is irreversible must not take
//! engine bookkeeping as its only input. [`live_process_evidence`] is the
//! second, independent opinion: the OS (`kill(pid, 0)`) plus the worker's
//! own hook stream. It runs in both places — when a pane is *classified*
//! (so a live worker is never flagged, never counted, and never appears in
//! `bossctl agents list --all` as a husk) and again inside `retire_pane`
//! (so the break-glass verb and any future caller inherit it too).
//!
//! ## Mass-retirement circuit breaker
//!
//! Even with corroboration, a pass that wants to retire many panes at once
//! is evidence about the engine, not about the panes. See
//! [`MAX_RETIREMENTS_PER_PASS`].
//!
//! ## Cadence
//!
//! Runs every [`DEFAULT_INTERVAL`] and fires once immediately on boot, same
//! as every other sweep in this crate.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{HostedPaneEntry, LiveWorkerState};

use crate::dead_pid_sweep::PidStatus;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};

/// How often the husk-pane sweep runs. 60s mirrors every other periodic
/// reconciler in this crate; the two-pass confirmation guard means the
/// earliest a genuine husk is retired is one interval after it is first
/// observed.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// How recent a hook event must be to count as corroboration that the
/// worker behind a *terminal* live-state entry is still running. Mirrors
/// [`crate::dead_pid_sweep::DEAD_PID_CORROBORATION_SECS`] deliberately:
/// both guards answer the same question ("is this process really gone?")
/// and should not drift apart.
pub const HUSK_LIVENESS_CORROBORATION_SECS: i64 = 120;

/// Most panes this sweep will retire in a single pass before tripping the
/// mass-retirement circuit breaker.
///
/// Retiring a pane is irreversible: it SIGTERMs the worker's process group
/// and destroys whatever uncommitted work that worker held. Several panes
/// going husk *within the same 60s pass* is not the shape a genuine leak
/// takes — leaks are produced by one release RPC failing at a time, and the
/// sweep reclaims them one or two per pass. A burst is far better explained
/// by engine-side amnesia: one bad global signal (see the `SessionEnd`
/// burst documented in [`crate::live_worker_state::LiveWorkerStateRegistry::apply_event`])
/// invalidating the bookkeeping for every slot at once.
///
/// The two failure modes are not symmetric. Wrongly retiring N live workers
/// destroys N workers' in-flight work and cannot be undone. Wrongly
/// *declining* to retire N genuine husks leaves N stray panes the operator
/// can reclaim at leisure with `bossctl agents retire-pane <slot>` — the
/// break-glass verb this sweep automates, which is unaffected by the
/// breaker. Choose the recoverable failure.
pub const MAX_RETIREMENTS_PER_PASS: usize = 3;

/// Decide whether a live-state entry contradicts the claim that its pane is
/// a husk — i.e. whether the worker process behind it is demonstrably still
/// running. `Some(evidence)` means "do not retire this pane", with
/// `evidence` naming the contradicting signal for the log.
///
/// A husk candidate reaches this function only when the engine's own
/// bookkeeping says the slot is free (no entry, or a terminal one). That
/// bookkeeping is exactly what the 2026-07-26 incident proved untrustworthy,
/// so this is a second, independent opinion sourced from the OS and from the
/// worker's own hook stream.
///
/// Corroboration needs BOTH halves, because neither is sufficient alone:
///
/// - **PID liveness alone is not enough.** A genuine husk keeps its
///   `shell_pid` alive: the pane hosts a shell, `claude` exited inside it,
///   and the shell lingers. `kill(pid, 0)` reports that shell as alive for a
///   husk and for a live worker identically, so treating "pid alive" as
///   "worker alive" would disable the sweep entirely.
/// - **Hook recency alone is not enough.** `last_event_at` is engine-side
///   bookkeeping too, and a slot recycled under a stale entry can carry a
///   prior run's timestamp.
///
/// Together they are strong: a shell process that still exists AND a worker
/// that either has an unbalanced `PreToolUse` outstanding or emitted a hook
/// within [`HUSK_LIVENESS_CORROBORATION_SECS`] is a worker doing work.
///
/// The tool-in-flight half is what covers the long-quiet case that hook
/// recency cannot: a worker inside a multi-minute foreground `bazel build`
/// emits nothing at all between its `PreToolUse` and the eventual
/// `PostToolUse`, so a pure recency window would age it out and kill it
/// mid-build. This mirrors
/// [`crate::dead_pid_sweep`]'s `corroborating_liveness` for the same reason.
pub(crate) fn live_process_evidence(state: &LiveWorkerState, now_epoch_secs: i64) -> Option<String> {
    // Guard before probing: `kill(0, 0)` signals the caller's own process
    // group, and a negative pid signals an arbitrary one. A slot that never
    // reported a shell pid offers no corroboration either way.
    if state.shell_pid <= 0 {
        return None;
    }
    let status = crate::dead_pid_sweep::probe_pid(state.shell_pid);
    live_process_evidence_with(state, &status, now_epoch_secs)
}

/// [`live_process_evidence`] with the PID probe injected, so the decision
/// is unit-testable without spawning real processes.
pub(crate) fn live_process_evidence_with(
    state: &LiveWorkerState,
    pid_status: &PidStatus,
    now_epoch_secs: i64,
) -> Option<String> {
    if state.shell_pid <= 0 {
        return None;
    }
    // Only an affirmative `ESRCH` clears the way to retire. `EPERM` (process
    // exists, owned by someone else) and `Unknown` are read conservatively as
    // "the process may well be there" — the conservative direction here is
    // sparing the pane, since the alternative is an unrecoverable kill.
    if matches!(pid_status, PidStatus::Dead) {
        return None;
    }
    let pid = state.shell_pid;

    // An unbalanced `PreToolUse`: the worker entered a tool and never left
    // it. Survives arbitrarily long quiet periods, which is the whole point.
    if let Some(tool) = state.current_tool.as_deref() {
        return Some(format!(
            "shell pid {pid} is alive and tool `{tool}` is still in flight (last hook {})",
            state.last_event_at.as_deref().unwrap_or("never"),
        ));
    }

    // Otherwise fall back to hook recency.
    let cutoff = crate::live_worker_state::iso8601_utc(now_epoch_secs - HUSK_LIVENESS_CORROBORATION_SECS);
    if let Some(event) = state.last_event_at.as_deref()
        && event >= cutoff.as_str()
    {
        return Some(format!(
            "shell pid {pid} is alive and a hook event arrived at {event}, within {HUSK_LIVENESS_CORROBORATION_SECS}s",
        ));
    }

    None
}

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
    /// `Some(n)` when the mass-retirement circuit breaker tripped: `n`
    /// confirmed husks — more than [`MAX_RETIREMENTS_PER_PASS`] — were held
    /// back rather than retired. Those candidates stay confirmed, so the
    /// breaker keeps tripping (and keeps logging) until an operator resolves
    /// the disagreement; it never silently degrades into retiring them.
    pub breaker_tripped: Option<usize>,
}

impl crate::sweep_loop::SweepOutcome for HuskPaneSweepOutcome {
    fn has_activity(&self) -> bool {
        // Observation passes count as activity, not just retirements. A
        // husk candidate seen on pass N is killed on pass N+1, so a pass
        // that only *observes* is the sole record of what the engine
        // believed one interval before an irreversible kill. Logging only
        // when `retired > 0` left that first pass invisible: five live
        // workers were retired with no trace of which slots were flagged
        // or what the live set held when they were flagged.
        self.retired > 0 || self.pending_confirmation > 0 || self.breaker_tripped.is_some()
    }

    fn log(&self) {
        tracing::info!(
            retired = self.retired,
            pending_confirmation = self.pending_confirmation,
            breaker_tripped = ?self.breaker_tripped,
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
    let seen_husks: Arc<tokio::sync::Mutex<HashSet<(u8, String)>>> = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
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
/// of `(slot_id, run_id)` pairs observed as husks on the *previous* pass; on
/// return it holds this pass's candidates so the next pass can confirm them.
/// Keying on the pair (not slot id alone) means a slot whose husk run
/// changes between passes never inherits a prior run's confirmation.
/// Returns a summary; callers may log it.
pub async fn run_one_pass(
    source: &dyn HuskPaneSweepSource,
    dispatch_events: &dyn DispatchEventSink,
    seen_husks: &mut HashSet<(u8, String)>,
) -> HuskPaneSweepOutcome {
    let mut outcome = HuskPaneSweepOutcome::default();

    let candidates = match source.list_husk_candidates().await {
        Some(panes) => panes,
        None => {
            outcome.list_failed = true;
            // Conservative: leave `seen_husks` untouched rather than
            // clearing it. A transient lookup failure sandwiched between two
            // genuine husk observations should not restart the two-pass
            // wait from scratch. Skipping the `confirm_two_pass` call is what
            // preserves `seen_husks` unchanged.
            return outcome;
        }
    };

    // Two-pass confirmation bookkeeping is shared with `terminal_work_sweep`.
    // Key on `(slot_id, run_id)` so a slot whose husk run changes between
    // passes never inherits a prior run's confirmation.
    let crate::sweep_loop::Confirmation { confirmed, pending } = crate::sweep_loop::confirm_two_pass(
        seen_husks,
        candidates
            .into_iter()
            .map(|pane| ((pane.slot_id, pane.run_id.clone()), pane)),
    );

    outcome.pending_confirmation = pending.len();
    for pane in pending {
        // `warn`, not `debug`: this line is the last record written before
        // the next pass kills the pane's process, and it is the only place
        // the flagging decision is visible. At `debug` it was filtered out
        // in practice, which is why a five-worker retirement could not be
        // traced back to what the engine believed one pass earlier.
        tracing::warn!(
            slot_id = pane.slot_id,
            run_id = %pane.run_id,
            task_title = ?pane.task_title,
            "husk-pane sweep: app-hosted pane with no engine-tracked run observed; \
             awaiting next-pass confirmation before retiring (next pass WILL kill this pane's process)",
        );
    }

    // Mass-retirement circuit breaker. See `MAX_RETIREMENTS_PER_PASS` for why
    // a burst is read as engine-side amnesia rather than as a burst of
    // genuine orphans, and why holding back is the recoverable failure.
    //
    // `seen_husks` has already been carried forward by `confirm_two_pass`, so
    // these candidates stay confirmed: the breaker re-trips (and re-logs)
    // every pass for as long as the condition holds, instead of quietly
    // relaxing into a mass kill on some later pass.
    if confirmed.len() > MAX_RETIREMENTS_PER_PASS {
        outcome.breaker_tripped = Some(confirmed.len());
        tracing::error!(
            confirmed = confirmed.len(),
            max_per_pass = MAX_RETIREMENTS_PER_PASS,
            slots = ?confirmed.iter().map(|pane| pane.slot_id).collect::<Vec<_>>(),
            "husk-pane sweep: MASS-RETIREMENT CIRCUIT BREAKER TRIPPED — {} panes confirmed as husks in a single \
             pass exceeds the {MAX_RETIREMENTS_PER_PASS}-per-pass limit. Retiring nothing: a burst this size is \
             far more likely to be engine bookkeeping that went wrong for every slot at once than that many \
             simultaneously-orphaned panes, and retiring a live worker is unrecoverable. Verify the panes by hand \
             and reclaim genuine husks with `bossctl agents retire-pane <slot>`.",
            confirmed.len(),
        );
        for pane in &confirmed {
            tracing::error!(
                slot_id = pane.slot_id,
                run_id = %pane.run_id,
                task_title = ?pane.task_title,
                "husk-pane sweep: held back by the mass-retirement circuit breaker; pane NOT retired",
            );
        }
        for pane in confirmed {
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::HuskPaneReconcile, Outcome::Skipped, pane.run_id.clone())
                        .with_worker(crate::coordinator::worker_id_for_slot(pane.slot_id))
                        .with_details(serde_json::json!({
                            "slot_id": pane.slot_id,
                            "task_title": pane.task_title,
                            "skipped_reason": "mass_retirement_circuit_breaker",
                            "max_per_pass": MAX_RETIREMENTS_PER_PASS,
                        })),
                )
                .await;
        }
        return outcome;
    }

    for pane in confirmed {
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

    /// A slot whose husk run changes between passes (the first husk cleared
    /// and a different leaked run took the same slot before the next pass)
    /// must NOT inherit the first run's confirmation — the swap resets the
    /// two-pass wait for the new run.
    #[tokio::test]
    async fn run_id_swap_on_same_slot_resets_confirmation() {
        let source = ScriptedSource::new(vec![
            Some(vec![husk(9, "exec-first")]),
            Some(vec![husk(9, "exec-second")]),
            Some(vec![husk(9, "exec-second")]),
        ]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        let first = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(first.pending_confirmation, 1);

        let second = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(
            second.retired, 0,
            "a different run on the same slot must not be treated as the same confirmed husk"
        );
        assert_eq!(
            second.pending_confirmation, 1,
            "the new run starts its own confirmation clock"
        );
        assert!(source.retired().is_empty());

        let third = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(
            third.retired, 1,
            "the new run is retired once it, too, is confirmed across two passes"
        );
        assert_eq!(source.retired(), vec![9]);

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].execution_id, "exec-second");
    }

    // ─── mass-retirement circuit breaker ─────────────────────────────────────

    fn husks(count: usize) -> Vec<HostedPaneEntry> {
        (0..count).map(|i| husk(i as u8, &format!("exec-{i}"))).collect()
    }

    /// Exactly [`MAX_RETIREMENTS_PER_PASS`] confirmed husks is under the
    /// limit and must still be retired — the breaker must not shrink the
    /// sweep's normal reclaim behaviour.
    #[tokio::test]
    async fn breaker_allows_a_pass_at_the_limit() {
        let batch = husks(MAX_RETIREMENTS_PER_PASS);
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &sink, &mut seen).await;
        let second = run_one_pass(&source, &sink, &mut seen).await;

        assert_eq!(second.retired, MAX_RETIREMENTS_PER_PASS);
        assert_eq!(second.breaker_tripped, None);
        assert_eq!(source.retired().len(), MAX_RETIREMENTS_PER_PASS);
    }

    /// Regression test for the 2026-07-26 incident's blast radius: five
    /// panes confirmed in one pass must retire NOTHING. Whatever produced
    /// five simultaneous husks is a statement about the engine, not about
    /// five independently-orphaned panes, and the kill is irreversible.
    #[tokio::test]
    async fn breaker_trips_and_retires_nothing_above_the_limit() {
        let batch = husks(MAX_RETIREMENTS_PER_PASS + 2);
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &sink, &mut seen).await;
        let second = run_one_pass(&source, &sink, &mut seen).await;

        assert_eq!(second.breaker_tripped, Some(MAX_RETIREMENTS_PER_PASS + 2));
        assert_eq!(second.retired, 0, "the breaker must retire nothing at all");
        assert!(
            source.retired().is_empty(),
            "no pane may be torn down while the breaker is tripped"
        );

        // Every held-back pane is reported as skipped, so the burst is
        // visible in the dispatch stream and not merely in the log.
        let events = sink.events().await;
        assert_eq!(events.len(), MAX_RETIREMENTS_PER_PASS + 2);
        assert!(events.iter().all(|event| event.outcome == "skipped"));
        assert!(
            events
                .iter()
                .all(|event| event.details["skipped_reason"] == "mass_retirement_circuit_breaker")
        );
    }

    /// A tripped breaker must not silently relax on the next pass: the
    /// candidates stay confirmed, so the sweep keeps refusing (and keeps
    /// logging) rather than eventually performing the mass kill it just
    /// declined.
    #[tokio::test]
    async fn breaker_stays_tripped_while_the_burst_persists() {
        let batch = husks(MAX_RETIREMENTS_PER_PASS + 2);
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch.clone()), Some(batch)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &sink, &mut seen).await;
        run_one_pass(&source, &sink, &mut seen).await;
        let third = run_one_pass(&source, &sink, &mut seen).await;

        assert_eq!(third.breaker_tripped, Some(MAX_RETIREMENTS_PER_PASS + 2));
        assert_eq!(third.retired, 0);
        assert!(source.retired().is_empty());
    }

    /// Once the burst subsides to a plausible size, the sweep resumes
    /// reclaiming genuine husks — the breaker is a rate limit, not a latch
    /// that disables the sweep forever.
    #[tokio::test]
    async fn breaker_resets_once_the_burst_subsides() {
        let big = husks(MAX_RETIREMENTS_PER_PASS + 2);
        let small = vec![husk(0, "exec-0")];
        let source = ScriptedSource::new(vec![Some(big.clone()), Some(big), Some(small)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &sink, &mut seen).await;
        let tripped = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(tripped.retired, 0);

        let third = run_one_pass(&source, &sink, &mut seen).await;
        assert_eq!(third.breaker_tripped, None);
        assert_eq!(third.retired, 1, "a single husk is retired normally again");
        assert_eq!(source.retired(), vec![0]);
    }

    // ─── liveness corroboration ──────────────────────────────────────────────

    /// Build a live-state entry overriding only the fields the corroboration
    /// decision reads.
    fn state_with(shell_pid: i32, last_event_at: Option<&str>, current_tool: Option<&str>) -> LiveWorkerState {
        let mut state = LiveWorkerState::new_spawning(1, "exec-run", "claude-opus-4-7", shell_pid, None);
        state.activity = boss_protocol::WorkerActivity::Terminated;
        state.last_event_at = last_event_at.map(ToOwned::to_owned);
        state.current_tool = current_tool.map(ToOwned::to_owned);
        state
    }

    const NOW: i64 = 1_800_000_000;

    /// The incident's core shape: a worker inside a long foreground `bazel`
    /// build. Its last hook is an unbalanced `PreToolUse` from far outside
    /// the recency window — recency alone would age it out and kill it — but
    /// the tool in flight proves it is mid-work.
    #[test]
    fn tool_in_flight_corroborates_life_however_long_the_quiet() {
        let state = state_with(
            4242,
            Some(&crate::live_worker_state::iso8601_utc(NOW - 3_600)),
            Some("Bash"),
        );
        let evidence = live_process_evidence_with(&state, &PidStatus::Alive, NOW).expect("must be spared");
        assert!(evidence.contains("Bash"), "evidence should name the tool: {evidence}");
    }

    /// The mid-inference victims' shape: no tool in flight, but a hook
    /// inside the corroboration window.
    #[test]
    fn recent_hook_corroborates_life_without_a_tool_in_flight() {
        let state = state_with(
            4242,
            Some(&crate::live_worker_state::iso8601_utc(
                NOW - HUSK_LIVENESS_CORROBORATION_SECS + 10,
            )),
            None,
        );
        assert!(live_process_evidence_with(&state, &PidStatus::Alive, NOW).is_some());
    }

    /// A quiet worker with no tool in flight is NOT corroborated — this is
    /// what keeps the sweep able to reclaim real husks, whose shell also
    /// stays alive after `claude` exits.
    #[test]
    fn alive_pid_alone_does_not_corroborate() {
        let state = state_with(
            4242,
            Some(&crate::live_worker_state::iso8601_utc(
                NOW - HUSK_LIVENESS_CORROBORATION_SECS - 10,
            )),
            None,
        );
        assert_eq!(live_process_evidence_with(&state, &PidStatus::Alive, NOW), None);
    }

    /// A dead shell pid is decisive the other way: nothing to protect, so
    /// even a tool that looked in-flight cannot spare the pane.
    #[test]
    fn dead_pid_is_never_corroborated() {
        let state = state_with(4242, Some(&crate::live_worker_state::iso8601_utc(NOW)), Some("Bash"));
        assert_eq!(live_process_evidence_with(&state, &PidStatus::Dead, NOW), None);
    }

    /// A slot that never reported a shell pid offers no corroboration and
    /// must not be probed (`kill(0, 0)` would signal our own process group).
    #[test]
    fn missing_shell_pid_yields_no_evidence() {
        let state = state_with(0, Some(&crate::live_worker_state::iso8601_utc(NOW)), Some("Bash"));
        assert_eq!(live_process_evidence(&state, NOW), None);
        assert_eq!(live_process_evidence_with(&state, &PidStatus::Alive, NOW), None);
    }

    /// `EPERM` means the process exists but is not ours — read as "alive"
    /// so an ambiguous probe never justifies an irreversible kill.
    #[test]
    fn permission_denied_probe_is_read_as_alive() {
        let state = state_with(4242, Some(&crate::live_worker_state::iso8601_utc(NOW)), Some("Bash"));
        assert!(live_process_evidence_with(&state, &PidStatus::PermissionDenied, NOW).is_some());
    }
}
