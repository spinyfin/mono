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
//! ## Escalating the breaker's refusal
//!
//! Declining to act is the right call (see [`MAX_RETIREMENTS_PER_PASS`]) —
//! but declining *silently* is its own failure, and was observed as one: on
//! a mass HTTP-529 event eleven slots tripped the breaker and it re-tripped
//! every 60 seconds for over an hour, producing nothing but a `tracing::error!`
//! line and a `husk_pane_reconcile/skipped` dispatch event per pane. Both are
//! pull-only surfaces. The operator found the wedged pool by chance.
//!
//! So a tripped breaker now escalates on two surfaces the operator does not
//! have to know to go looking at, in addition to the log and dispatch events:
//!
//! - a durable `husk_retirement_breaker_tripped` **attention item** on every
//!   work item whose pane is being held back
//!   ([`crate::work::ATTENTION_KIND_HUSK_BREAKER_TRIPPED`]) — those are
//!   exactly the cards whose next execution cannot land; and
//! - an error-severity **engine-health issue** via
//!   [`HuskRetirementBreakerHealth`], which
//!   [`crate::app::build_engine_health_report`] turns into the app's health
//!   banner — one aggregate signal for the pool-wide condition, covering the
//!   panes that have no kanban card to file against.
//!
//! Both are self-clearing: the first pass that confirms
//! [`MAX_RETIREMENTS_PER_PASS`] or fewer husks resolves the attention items
//! and clears the flag. Neither weakens the breaker — the panes are still
//! not retired.
//!
//! ## Cadence
//!
//! Runs every [`DEFAULT_INTERVAL`] and fires once immediately on boot, same
//! as every other sweep in this crate.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use boss_protocol::{HostedPaneEntry, LiveWorkerState};

use crate::dead_pid_sweep::PidStatus;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::WorkDb;

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

/// Snapshot of whether the mass-retirement circuit breaker is currently
/// holding panes back. Read by [`crate::app::build_engine_health_report`] to
/// raise the operator banner; cheap to clone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HuskRetirementBreakerStatus {
    /// True while the most recent conclusive pass tripped the breaker. A
    /// pass whose `list_husk_candidates` lookup failed is inconclusive and
    /// leaves this untouched — the same conservatism the confirmation set
    /// gets.
    pub tripped: bool,
    /// Confirmed husks in that pass (always > [`MAX_RETIREMENTS_PER_PASS`]
    /// while `tripped`).
    pub confirmed: usize,
    /// Slots being held back, ascending. Named in the health banner so the
    /// operator can go straight to `bossctl agents retire-pane <slot>`.
    pub slots: Vec<u8>,
    /// Epoch seconds at which the breaker first tripped in the current
    /// episode — not refreshed by later trips, so the banner can say how
    /// long the pool has been wedged rather than "just now" forever.
    pub tripped_since_epoch: Option<i64>,
}

/// Shared escalation flag for the mass-retirement circuit breaker: the sweep
/// writes it, [`crate::app::build_engine_health_report`] reads it. Mirrors
/// [`crate::syspolicyd_monitor::SyspolicydHealth`] — a `Mutex` around a small
/// snapshot, written once a minute at most and read on the health path only.
///
/// This exists because the breaker's `tracing::error!` and its per-pane
/// `skipped` dispatch events are both pull-only: nothing reached the operator
/// until they went looking. The flag is what makes the refusal visible
/// without knowing to look.
#[derive(Debug, Default)]
pub struct HuskRetirementBreakerHealth {
    status: StdMutex<HuskRetirementBreakerStatus>,
    /// Whether any pass has yet reached a conclusive under-the-limit verdict
    /// in this engine process. Until one has, the clear path runs its
    /// attention-item resolve unconditionally: an engine restarted in the
    /// middle of an episode has no in-memory record of it, and an item that
    /// says it clears itself must not be stranded open by a restart.
    cleared_since_boot: StdMutex<bool>,
}

impl HuskRetirementBreakerHealth {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> HuskRetirementBreakerStatus {
        self.status.lock().expect("husk breaker health mutex poisoned").clone()
    }

    /// Record a tripped pass. Preserves `tripped_since_epoch` across
    /// consecutive trips so the banner reports the age of the episode, not
    /// of the latest pass. Returns `true` if this is the first trip of a new
    /// episode, which the caller logs at a higher volume.
    fn record_tripped(&self, confirmed: usize, slots: Vec<u8>, now_epoch_secs: i64) -> bool {
        let mut status = self.status.lock().expect("husk breaker health mutex poisoned");
        let newly_tripped = !status.tripped;
        let tripped_since_epoch = if newly_tripped {
            Some(now_epoch_secs)
        } else {
            status.tripped_since_epoch
        };
        *status = HuskRetirementBreakerStatus {
            tripped: true,
            confirmed,
            slots,
            tripped_since_epoch,
        };
        newly_tripped
    }

    /// Record a conclusive pass that did NOT trip the breaker. Returns
    /// whether the caller should also retract the durable half of the
    /// escalation: true when this process has seen the breaker tripped, and
    /// once at startup so an episode that outlived an engine restart is
    /// cleaned up too. False on every subsequent healthy pass, so the steady
    /// state costs no DB write at all.
    fn record_clear(&self) -> bool {
        let was_tripped = {
            let mut status = self.status.lock().expect("husk breaker health mutex poisoned");
            std::mem::take(&mut *status).tripped
        };
        let mut cleared_since_boot = self
            .cleared_since_boot
            .lock()
            .expect("husk breaker health mutex poisoned");
        let first_clear = !*cleared_since_boot;
        *cleared_since_boot = true;
        was_tripped || first_clear
    }
}

/// Collaborators one sweep pass needs, bundled so [`run_one_pass`] keeps a
/// short argument list (mirrors [`crate::transient_recovery::RecoveryContext`]).
pub struct HuskSweepContext<'a> {
    pub source: &'a dyn HuskPaneSweepSource,
    pub dispatch_events: &'a dyn DispatchEventSink,
    /// Used only to escalate a tripped breaker: resolve each held-back pane's
    /// execution to its work item and file/resolve the attention item there.
    pub work_db: &'a WorkDb,
    pub breaker_health: &'a HuskRetirementBreakerHealth,
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
#[derive(Debug, Default, bon::Builder)]
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
    /// Work items an attention item was filed/refreshed against because the
    /// breaker is holding their worker's pane. Zero while `breaker_tripped`
    /// is `Some` only when none of the held-back panes maps to a kanban card
    /// (e.g. all automation executions) — the health banner still covers
    /// those.
    pub escalated_work_items: usize,
    /// Attention items resolved this pass because the burst subsided.
    pub escalations_cleared: usize,
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
        self.retired > 0
            || self.pending_confirmation > 0
            || self.breaker_tripped.is_some()
            || self.escalations_cleared > 0
    }

    fn log(&self) {
        tracing::info!(
            retired = self.retired,
            pending_confirmation = self.pending_confirmation,
            breaker_tripped = ?self.breaker_tripped,
            escalated_work_items = self.escalated_work_items,
            escalations_cleared = self.escalations_cleared,
            "husk-pane sweep: retired app-hosted pane(s) the engine no longer tracks",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`,
/// threading the cross-pass confirmation set so it survives between passes.
pub fn spawn_loop(
    source: Arc<dyn HuskPaneSweepSource>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    work_db: Arc<WorkDb>,
    breaker_health: Arc<HuskRetirementBreakerHealth>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let seen_husks: Arc<tokio::sync::Mutex<HashSet<(u8, String)>>> = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let source = Arc::clone(&source);
        let dispatch_events = Arc::clone(&dispatch_events);
        let work_db = Arc::clone(&work_db);
        let breaker_health = Arc::clone(&breaker_health);
        let seen_husks = Arc::clone(&seen_husks);
        async move {
            let mut seen_husks = seen_husks.lock().await;
            let cx = HuskSweepContext {
                source: source.as_ref(),
                dispatch_events: dispatch_events.as_ref(),
                work_db: work_db.as_ref(),
                breaker_health: breaker_health.as_ref(),
            };
            run_one_pass(&cx, &mut seen_husks).await
        }
    })
}

/// Run a single husk-pane reconciliation pass. `seen_husks` carries the set
/// of `(slot_id, run_id)` pairs observed as husks on the *previous* pass; on
/// return it holds this pass's candidates so the next pass can confirm them.
/// Keying on the pair (not slot id alone) means a slot whose husk run
/// changes between passes never inherits a prior run's confirmation.
/// Returns a summary; callers may log it.
pub async fn run_one_pass(cx: &HuskSweepContext<'_>, seen_husks: &mut HashSet<(u8, String)>) -> HuskPaneSweepOutcome {
    let &HuskSweepContext {
        source,
        dispatch_events,
        work_db,
        breaker_health,
    } = cx;
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
        // Escalate BEFORE the per-pane logging/events below: the whole point
        // is that an operator learns about this without having to read either.
        outcome.escalated_work_items = escalate_breaker(work_db, breaker_health, &confirmed).await;
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

    // A conclusive pass under the limit: whatever the breaker was holding is
    // gone, so retract the escalation. Deliberately not reached from the
    // `list_failed` early return above — a failed lookup is no information,
    // and clearing an operator-visible alert on no information is how an
    // alert stops being trustworthy.
    outcome.escalations_cleared = clear_breaker_escalation(work_db, breaker_health);

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

/// Raise the operator-visible escalation for a tripped breaker: set the
/// engine-health flag the app's banner reads, and file (or refresh) a
/// `husk_retirement_breaker_tripped` attention item on every work item whose
/// pane is being held back. Returns how many work items were filed against.
///
/// Best-effort by design: this is the alerting path, not the safety
/// mechanism. A DB failure here is logged and swallowed — it must never stop
/// the sweep, and it must never turn into the sweep retiring what the breaker
/// just declined to retire.
async fn escalate_breaker(
    work_db: &WorkDb,
    breaker_health: &HuskRetirementBreakerHealth,
    confirmed: &[HostedPaneEntry],
) -> usize {
    let mut slots: Vec<u8> = confirmed.iter().map(|pane| pane.slot_id).collect();
    slots.sort_unstable();
    slots.dedup();
    let newly_tripped = breaker_health.record_tripped(
        confirmed.len(),
        slots.clone(),
        boss_engine_utils::epoch_time::now_epoch_secs(),
    );
    if newly_tripped {
        tracing::error!(
            confirmed = confirmed.len(),
            ?slots,
            "husk-pane sweep: raising engine-health alert — the mass-retirement circuit breaker has \
             tripped and the affected slots are desynced from the app until an operator resolves it",
        );
    }

    let held_back: Vec<(String, u8)> = confirmed
        .iter()
        .map(|pane| (pane.run_id.clone(), pane.slot_id))
        .collect();
    match work_db.file_husk_breaker_attentions(&held_back, MAX_RETIREMENTS_PER_PASS) {
        Ok(filed) => filed.len(),
        Err(err) => {
            tracing::warn!(
                ?err,
                "husk-pane sweep: failed to file circuit-breaker attention items; the engine-health \
                 banner still reports the condition",
            );
            0
        }
    }
}

/// Retract the escalation once a conclusive pass finds the burst subsided:
/// clear the health flag and resolve exactly the attention items this module
/// filed. Returns how many were resolved (`0` on the normal healthy pass,
/// where nothing was outstanding).
fn clear_breaker_escalation(work_db: &WorkDb, breaker_health: &HuskRetirementBreakerHealth) -> usize {
    if !breaker_health.record_clear() {
        return 0;
    }
    match work_db.resolve_husk_breaker_attentions() {
        Ok(0) => 0,
        Ok(cleared) => {
            tracing::info!(
                cleared,
                "husk-pane sweep: mass-retirement circuit breaker cleared; retracting operator escalation",
            );
            cleared
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                "husk-pane sweep: failed to resolve circuit-breaker attention items after the burst \
                 subsided; the engine-health banner has already cleared",
            );
            0
        }
    }
}

/// End-to-end reproduction of the mass-wedge this module's breaker was
/// implicated in, and of the recovery. Spans this module and
/// [`crate::transient_recovery`], so it lives in its own file.
#[cfg(test)]
#[path = "husk_wedge_repro_tests.rs"]
mod wedge_repro_tests;

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::test_support::{create_active_chore, create_product, open_db};
    use crate::work::ATTENTION_KIND_HUSK_BREAKER_TRIPPED;

    fn husk(slot_id: u8, run_id: &str) -> HostedPaneEntry {
        HostedPaneEntry {
            slot_id,
            run_id: run_id.to_owned(),
            summary: None,
            task_title: Some("test chore".to_owned()),
        }
    }

    /// Per-test collaborators for [`run_one_pass`]: a real (temp) `WorkDb`
    /// because the escalation path files attention items through it, the
    /// breaker health flag, and the recording dispatch sink.
    struct Harness {
        _dir: tempfile::TempDir,
        db: WorkDb,
        health: HuskRetirementBreakerHealth,
        sink: RecordingDispatchEventSink,
    }

    impl Harness {
        fn new() -> Self {
            let (dir, db) = open_db();
            Self {
                _dir: dir,
                db,
                health: HuskRetirementBreakerHealth::new(),
                sink: RecordingDispatchEventSink::new(),
            }
        }

        async fn pass(
            &self,
            source: &dyn HuskPaneSweepSource,
            seen: &mut HashSet<(u8, String)>,
        ) -> HuskPaneSweepOutcome {
            let cx = HuskSweepContext {
                source,
                dispatch_events: &self.sink,
                work_db: &self.db,
                breaker_health: &self.health,
            };
            run_one_pass(&cx, seen).await
        }

        /// Register a chore with an execution, returning
        /// `(work_item_id, execution_id)`. A husk pane whose `run_id` is that
        /// execution id resolves to a real kanban card, which is what the
        /// escalation files its attention item against.
        fn chore_with_execution(&self, name: &str) -> (String, String) {
            use boss_protocol::RequestExecutionInput;
            let product_id = create_product(&self.db);
            let work_item_id = create_active_chore(&self.db, &product_id, name);
            let execution = self
                .db
                .request_execution(
                    RequestExecutionInput::builder()
                        .work_item_id(work_item_id.clone())
                        .build(),
                )
                .unwrap();
            (work_item_id, execution.id)
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
        let h = Harness::new();
        let mut seen = HashSet::new();

        let first = h.pass(&source, &mut seen).await;
        assert_eq!(first.retired, 0, "first pass must only record the candidate");
        assert_eq!(first.pending_confirmation, 1);
        assert!(source.retired().is_empty());
        assert!(h.sink.events().await.is_empty());

        let second = h.pass(&source, &mut seen).await;
        assert_eq!(second.retired, 1, "second pass must retire the confirmed husk");
        assert_eq!(source.retired(), vec![7]);

        let events = h.sink.events().await;
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
        let h = Harness::new();
        let mut seen = HashSet::new();

        let first = h.pass(&source, &mut seen).await;
        assert_eq!(first.pending_confirmation, 1);

        let second = h.pass(&source, &mut seen).await;
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
        let h = Harness::new();
        let mut seen = HashSet::new();

        let first = h.pass(&source, &mut seen).await;
        assert_eq!(first.pending_confirmation, 1);

        let second = h.pass(&source, &mut seen).await;
        assert!(second.list_failed);
        assert_eq!(second.retired, 0);
        assert_eq!(seen.len(), 1, "seen set must survive the failed pass unchanged");

        let third = h.pass(&source, &mut seen).await;
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
        let h = Harness::new();
        let mut seen = HashSet::new();

        for _ in 0..2 {
            let outcome = h.pass(&source, &mut seen).await;
            assert_eq!(outcome.retired, 0);
            assert_eq!(outcome.pending_confirmation, 0);
        }
        assert!(source.retired().is_empty());
        assert!(h.sink.events().await.is_empty());
    }

    /// Two distinct husks confirmed in the same pass are both retired, each
    /// emitting its own dispatch event.
    #[tokio::test]
    async fn retires_multiple_confirmed_husks_independently() {
        let source = ScriptedSource::new(vec![
            Some(vec![husk(1, "exec-x"), husk(2, "exec-y")]),
            Some(vec![husk(1, "exec-x"), husk(2, "exec-y")]),
        ]);
        let h = Harness::new();
        let mut seen = HashSet::new();

        h.pass(&source, &mut seen).await;
        let second = h.pass(&source, &mut seen).await;

        assert_eq!(second.retired, 2);
        let mut retired = source.retired();
        retired.sort_unstable();
        assert_eq!(retired, vec![1, 2]);
        assert_eq!(h.sink.events().await.len(), 2);
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
        let h = Harness::new();
        let mut seen = HashSet::new();

        let first = h.pass(&source, &mut seen).await;
        assert_eq!(first.pending_confirmation, 1);

        let second = h.pass(&source, &mut seen).await;
        assert_eq!(
            second.retired, 0,
            "a different run on the same slot must not be treated as the same confirmed husk"
        );
        assert_eq!(
            second.pending_confirmation, 1,
            "the new run starts its own confirmation clock"
        );
        assert!(source.retired().is_empty());

        let third = h.pass(&source, &mut seen).await;
        assert_eq!(
            third.retired, 1,
            "the new run is retired once it, too, is confirmed across two passes"
        );
        assert_eq!(source.retired(), vec![9]);

        let events = h.sink.events().await;
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
        let h = Harness::new();
        let mut seen = HashSet::new();

        h.pass(&source, &mut seen).await;
        let second = h.pass(&source, &mut seen).await;

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
        let h = Harness::new();
        let mut seen = HashSet::new();

        h.pass(&source, &mut seen).await;
        let second = h.pass(&source, &mut seen).await;

        assert_eq!(second.breaker_tripped, Some(MAX_RETIREMENTS_PER_PASS + 2));
        assert_eq!(second.retired, 0, "the breaker must retire nothing at all");
        assert!(
            source.retired().is_empty(),
            "no pane may be torn down while the breaker is tripped"
        );

        // Every held-back pane is reported as skipped, so the burst is
        // visible in the dispatch stream and not merely in the log.
        let events = h.sink.events().await;
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
        let h = Harness::new();
        let mut seen = HashSet::new();

        h.pass(&source, &mut seen).await;
        h.pass(&source, &mut seen).await;
        let third = h.pass(&source, &mut seen).await;

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
        let h = Harness::new();
        let mut seen = HashSet::new();

        h.pass(&source, &mut seen).await;
        let tripped = h.pass(&source, &mut seen).await;
        assert_eq!(tripped.retired, 0);

        let third = h.pass(&source, &mut seen).await;
        assert_eq!(third.breaker_tripped, None);
        assert_eq!(third.retired, 1, "a single husk is retired normally again");
        assert_eq!(source.retired(), vec![0]);
    }

    // ─── breaker escalation ──────────────────────────────────────────────────

    /// The defect this closes: before escalation, a tripped breaker produced
    /// only a log line and pull-only dispatch events. It must now raise the
    /// engine-health flag (the app's banner) AND file a durable attention
    /// item on every work item whose pane is being held back — while still
    /// retiring nothing.
    #[tokio::test]
    async fn tripped_breaker_escalates_to_health_flag_and_attention_items() {
        let h = Harness::new();
        let mut cards = Vec::new();
        let mut panes = Vec::new();
        for i in 0..(MAX_RETIREMENTS_PER_PASS + 2) {
            let (work_item_id, execution_id) = h.chore_with_execution(&format!("wedged chore {i}"));
            panes.push(husk(i as u8, &execution_id));
            cards.push(work_item_id);
        }
        let source = ScriptedSource::new(vec![Some(panes.clone()), Some(panes)]);
        let mut seen = HashSet::new();

        let first = h.pass(&source, &mut seen).await;
        assert_eq!(first.breaker_tripped, None, "nothing is confirmed on the first pass");
        assert!(
            !h.health.snapshot().tripped,
            "an unconfirmed observation must not raise an operator alert",
        );

        let second = h.pass(&source, &mut seen).await;
        assert_eq!(second.breaker_tripped, Some(MAX_RETIREMENTS_PER_PASS + 2));
        assert_eq!(second.retired, 0, "escalating must not soften the refusal");
        assert!(source.retired().is_empty());

        let status = h.health.snapshot();
        assert!(status.tripped, "the engine-health banner must report the wedge");
        assert_eq!(status.confirmed, MAX_RETIREMENTS_PER_PASS + 2);
        assert_eq!(
            status.slots,
            (0..(MAX_RETIREMENTS_PER_PASS + 2) as u8).collect::<Vec<_>>(),
            "the banner must name the held-back slots so the operator can act on them",
        );
        assert!(status.tripped_since_epoch.is_some());

        assert_eq!(second.escalated_work_items, cards.len());
        for work_item_id in &cards {
            let items = h.db.list_attention_items_for_work_item(work_item_id).unwrap();
            let breaker: Vec<_> = items
                .iter()
                .filter(|item| item.kind == ATTENTION_KIND_HUSK_BREAKER_TRIPPED)
                .collect();
            assert_eq!(breaker.len(), 1, "one attention item per wedged work item");
            assert_eq!(breaker[0].status, "open");
            assert!(
                breaker[0].body_markdown.contains("retire-pane"),
                "the item must tell the operator how to reclaim a genuine husk",
            );
        }
    }

    /// Re-tripping every 60s must refresh the same attention item rather than
    /// piling up a new one per pass, and must not restart the "wedged since"
    /// clock — an operator needs to see how long the pool has been stuck.
    #[tokio::test]
    async fn repeated_trips_refresh_one_item_and_keep_the_original_timestamp() {
        let h = Harness::new();
        let mut panes = Vec::new();
        let mut cards = Vec::new();
        for i in 0..(MAX_RETIREMENTS_PER_PASS + 1) {
            let (work_item_id, execution_id) = h.chore_with_execution(&format!("wedged chore {i}"));
            panes.push(husk(i as u8, &execution_id));
            cards.push(work_item_id);
        }
        let source = ScriptedSource::new(vec![Some(panes.clone()), Some(panes.clone()), Some(panes)]);
        let mut seen = HashSet::new();

        h.pass(&source, &mut seen).await;
        h.pass(&source, &mut seen).await;
        let first_trip = h.health.snapshot().tripped_since_epoch;
        let third = h.pass(&source, &mut seen).await;

        assert_eq!(third.breaker_tripped, Some(MAX_RETIREMENTS_PER_PASS + 1));
        assert_eq!(
            h.health.snapshot().tripped_since_epoch,
            first_trip,
            "a persisting episode keeps its original start time",
        );
        for work_item_id in &cards {
            let open =
                h.db.list_attention_items_for_work_item(work_item_id)
                    .unwrap()
                    .into_iter()
                    .filter(|item| item.kind == ATTENTION_KIND_HUSK_BREAKER_TRIPPED && item.status == "open")
                    .count();
            assert_eq!(open, 1, "re-tripping must refresh, not duplicate");
        }
    }

    /// The other half of "a path out": once the burst subsides, the operator
    /// alert must retract itself. An alert nobody can clear without a manual
    /// gesture is the same silent-failure shape from the other direction.
    #[tokio::test]
    async fn escalation_clears_itself_once_the_burst_subsides() {
        let h = Harness::new();
        let mut panes = Vec::new();
        let mut cards = Vec::new();
        for i in 0..(MAX_RETIREMENTS_PER_PASS + 2) {
            let (work_item_id, execution_id) = h.chore_with_execution(&format!("wedged chore {i}"));
            panes.push(husk(i as u8, &execution_id));
            cards.push(work_item_id);
        }
        // Burst, burst (trips), then the panes are gone.
        let source = ScriptedSource::new(vec![Some(panes.clone()), Some(panes), Some(vec![])]);
        let mut seen = HashSet::new();

        h.pass(&source, &mut seen).await;
        let tripped = h.pass(&source, &mut seen).await;
        assert!(tripped.breaker_tripped.is_some());
        assert!(h.health.snapshot().tripped);

        let healthy = h.pass(&source, &mut seen).await;
        assert_eq!(healthy.breaker_tripped, None);
        assert_eq!(healthy.escalations_cleared, cards.len());
        assert_eq!(
            h.health.snapshot(),
            HuskRetirementBreakerStatus::default(),
            "the health banner must clear itself",
        );
        for work_item_id in &cards {
            let open =
                h.db.list_attention_items_for_work_item(work_item_id)
                    .unwrap()
                    .into_iter()
                    .filter(|item| item.kind == ATTENTION_KIND_HUSK_BREAKER_TRIPPED && item.status == "open")
                    .count();
            assert_eq!(open, 0, "the attention item must resolve on its own");
        }
    }

    /// A failed `list_husk_candidates` lookup is no information at all. It
    /// must not retract a standing operator alert — clearing an alert on a
    /// transport blip is how an alert stops being believed.
    #[tokio::test]
    async fn failed_lookup_does_not_clear_a_standing_escalation() {
        let h = Harness::new();
        let mut panes = Vec::new();
        for i in 0..(MAX_RETIREMENTS_PER_PASS + 2) {
            let (_, execution_id) = h.chore_with_execution(&format!("wedged chore {i}"));
            panes.push(husk(i as u8, &execution_id));
        }
        let source = ScriptedSource::new(vec![Some(panes.clone()), Some(panes), None]);
        let mut seen = HashSet::new();

        h.pass(&source, &mut seen).await;
        h.pass(&source, &mut seen).await;
        assert!(h.health.snapshot().tripped);

        let blip = h.pass(&source, &mut seen).await;
        assert!(blip.list_failed);
        assert_eq!(blip.escalations_cleared, 0);
        assert!(
            h.health.snapshot().tripped,
            "an inconclusive pass must leave the alert standing",
        );
    }

    /// A healthy pass on a healthy pool must not touch the DB or log a
    /// retraction — the clear path is only for retracting something real.
    #[tokio::test]
    async fn healthy_pass_reports_no_escalation_activity() {
        let source = ScriptedSource::new(vec![Some(vec![husk(1, "exec-a")]), Some(vec![husk(1, "exec-a")])]);
        let h = Harness::new();
        let mut seen = HashSet::new();

        h.pass(&source, &mut seen).await;
        let second = h.pass(&source, &mut seen).await;

        assert_eq!(second.retired, 1);
        assert_eq!(second.escalated_work_items, 0);
        assert_eq!(second.escalations_cleared, 0);
        assert!(!h.health.snapshot().tripped);
    }

    /// Held-back panes whose run has no kanban card (an automation execution,
    /// or a row already retention-swept) still trip the breaker and still
    /// raise the aggregate health alert — there is simply no card to file an
    /// attention item against, and that must not be mistaken for "nothing to
    /// escalate".
    #[tokio::test]
    async fn panes_without_a_work_item_still_raise_the_health_alert() {
        let batch = husks(MAX_RETIREMENTS_PER_PASS + 2);
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch)]);
        let h = Harness::new();
        let mut seen = HashSet::new();

        h.pass(&source, &mut seen).await;
        let second = h.pass(&source, &mut seen).await;

        assert_eq!(second.breaker_tripped, Some(MAX_RETIREMENTS_PER_PASS + 2));
        assert_eq!(
            second.escalated_work_items, 0,
            "no execution rows exist for these run ids, so there is no card to file on"
        );
        assert!(
            h.health.snapshot().tripped,
            "the aggregate health alert must still fire for cardless panes",
        );
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
