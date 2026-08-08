//! Periodic reconciler that retires leaked tmux sessions — worker panes the
//! private `boss` tmux server is still hosting under a spawn token that has
//! no durable row in this engine's DB at all.
//!
//! ## The gap this closes
//!
//! The tmux server, not the macOS app's hosted-pane inventory, is the
//! authoritative record of what worker sessions physically exist.
//! [`crate::tmux_adoption`] enumerates that server and, for every live
//! session, resolves its `BOSS_SPAWN_TOKEN` against `work_runs` via
//! [`crate::work::WorkDb::execution_id_for_tmux_spawn_token`]: a match
//! re-adopts the session (non-terminal execution) or hands it to
//! [`crate::worker_readoption`] (terminal execution, a live-vs-terminal
//! contradiction). A session whose token matches **no row at all** is
//! neither — it is a [`crate::tmux_adoption::UntrackedTmuxSession`], pushed
//! onto [`crate::tmux_adoption::TmuxAdoptionOutcome::untracked_sessions`],
//! and this sweep is what confirms and reaps those.
//!
//! Historically the same shape (a pane the app still hosts that the
//! engine's own bookkeeping no longer references) was reached from the
//! *app*-hosted-pane side: `release_worker_pane` clears engine state
//! unconditionally at the end of teardown — "successfully or not" — so an
//! `ReleaseWorkerPane` RPC that timed out, or a terminal-transition site
//! that cleared engine state without the app RPC ever landing, left the
//! app still physically hosting a pane no engine-state-driven reconciler
//! could see (`bossctl agents list` and `terminal_work_sweep`/
//! `pool_claim_sweep` all iterate structures the leak already emptied).
//! The operator-observed 2026-07-14 incident (worker "O'Brien"'s exec
//! created 06:26:19Z, pane spawned only 06:31:22Z; dispatch showed twelve
//! `request_recorded` → `worker_claimed=skipped` cycles from 06:29:06 to
//! 06:31:10) is that shape: a slot the engine's pool considered free was
//! actually still occupied by a real hosted pane, so `SpawnWorkerPane` for
//! the next dispatch kept losing the race against `SlotBusy` until the
//! stray pane finally cleared. tmux-session enumeration closes the same gap
//! from the resource-identity side instead of the app-RPC side: the tmux
//! server is asked directly what exists, so no teardown RPC failure mode
//! can hide a leaked session from it.
//!
//! [`crate::app::ServerState::list_husk_panes`] and
//! [`crate::app::ServerState::retire_pane`] remain as the manual,
//! operator-invoked break-glass path (`bossctl agents list --all` /
//! `bossctl agents retire-pane`) over the app's own hosted-pane inventory.
//! This sweep is the automatic backstop over the tmux server's inventory
//! instead: once a leaked session has been reported untracked on two
//! consecutive passes, it retires it (`tmux -L boss kill-session`),
//! regardless of which terminal-transition site produced the divergence.
//!
//! ## Two-pass confirmation
//!
//! A session tmux just created (a fresh spawn whose intent write hasn't
//! landed in `work_runs` yet, or whose adoption pass hasn't run) can
//! transiently look untracked. Requiring the SAME `(session_name,
//! spawn_token)` pair to appear on two consecutive passes (mirroring
//! [`crate::terminal_work_sweep`]) gives any in-flight registration or
//! teardown a full interval to resolve before this sweep acts, and a
//! session that stops looking untracked between passes (its row landed, or
//! it was cleared by something else) simply drops out of the confirmed
//! set. Keying on the pair (not session name alone) means a session name
//! whose spawn token changes between passes — the original leak cleared and
//! a different leaked session reused the same name — resets the
//! confirmation clock for the new token instead of being mistaken for the
//! same leak observed twice.
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
//! own hook stream. It backs the manual, app-hosted-pane break-glass path
//! instead — `list_husk_panes`'s own classification (so a live worker is
//! never flagged, never counted, and never appears in `bossctl agents list
//! --all` as a husk) and again inside `retire_pane`'s guard (so the
//! break-glass verb and any future caller inherit it too). This periodic
//! sweep does not need a separate liveness probe for the same reason: a
//! session only reaches it as an [`crate::tmux_adoption::UntrackedTmuxSession`]
//! after [`crate::tmux_adoption::classify_untracked_session`] has already
//! failed to resolve its token to ANY execution, live or dead — there is no
//! `LiveWorkerState` for it to corroborate against in the first place.
//!
//! ## Corroboration when there is NO live-state entry at all
//!
//! [`live_process_evidence`] reads a `LiveWorkerState`, so it can only help
//! for a slot that still has one. The 2026-07-28 incident was the other
//! shape: six executions were terminalized while their workers ran on, and
//! `release_worker_pane` drops the live-state entry unconditionally on every
//! terminal path — so the entry those workers would have been judged by no
//! longer existed. "The engine has no entry for this slot" is the weakest
//! possible evidence of death precisely because it is what a wrong
//! terminalization produces.
//!
//! `ServerState::durable_live_process_evidence` covers that branch from
//! durable state instead: `work_runs.shell_pid` (which survives the teardown
//! that emptied the registry) plus the execution's status. It spares a pane
//! only when the pid is alive AND the status is terminal *by inference*
//! (`orphaned` / `abandoned`) — a lingering shell under a `completed` or
//! `cancelled` run is still the genuine husk this sweep exists to reclaim.
//!
//! Those spared panes are not abandoned: they are re-adoption candidates, and
//! [`crate::worker_readoption`] is what resolves them. Without this branch the
//! two halves of convergence would race — a worker that is alive but quiet
//! could be confirmed a husk across two passes and SIGTERMed before the
//! re-adoption path, running on the same 60 s cadence, reached it.
//!
//! ## Mass-retirement circuit breaker
//!
//! Even with corroboration, a pass that wants to retire many panes at once
//! is evidence about the engine, not about the panes. See
//! [`MAX_UNCONFIRMED_RETIREMENTS_PER_PASS`].
//!
//! ## Independent death evidence (why a burst is no longer self-sustaining)
//!
//! The breaker as first written declined to retire ANY pane once a pass
//! confirmed more than three, which is correct when the burst really is
//! engine amnesia and catastrophic when it is not. A mass transient-API
//! outage produces exactly the shape it refuses: every worker dies on a
//! `529 Overloaded`, [`crate::transient_recovery`] orphans each execution
//! and frees each slot's engine-side bookkeeping *without* tearing the pane
//! down, and every resume dispatch is then rejected `SlotBusy` by an app
//! that still hosts a pane there. Retirement is the only thing that clears
//! the desync, so a tripped breaker keeps the pool wedged, keeps re-tripping
//! on the next pass, and never recovers — eleven slots across both pools sat
//! like this for over an hour, cleared only by an operator running
//! `bossctl agents retire-pane` once per slot.
//!
//! The way out is not a bigger limit — that just re-opens the 2026-07-26
//! incident. It is to stop *assuming* the burst means engine amnesia and
//! test it per candidate, because the two causes leave different fingerprints
//! on the engine's two independent records of the same run:
//!
//! | | `LiveWorkerStateRegistry` | `work_executions` row |
//! |---|---|---|
//! | engine amnesia (2026-07-26) | terminal / absent | **still `running`** |
//! | genuine mass orphan | absent | **`orphaned`** |
//!
//! The 2026-07-26 `SessionEnd` burst reached
//! [`crate::live_worker_state::LiveWorkerStateRegistry::apply_event`] and
//! nothing else: it flipped in-memory activity, and the DB rows for those
//! five live workers stayed `running` the whole time. A genuine orphan, by
//! contrast, is terminal in the DB because some deliberate transition put it
//! there (`request_resume_execution`, `mark_execution_orphaned`, the
//! completion path). So [`death_evidence_from_db`] is a second opinion the
//! husk classifier cannot supply — note that a pane whose live-state entry
//! was *released* reaches [`crate::app::ServerState::list_husk_panes`]'s
//! "no entry at all" arm, where [`live_process_evidence`] is never consulted
//! and there is no per-candidate corroboration to weigh at all.
//!
//! [`run_one_pass`] therefore partitions each pass's confirmed husks:
//!
//! * **Independently confirmed dead** (the DB agrees the run is terminal) —
//!   exempt from the burst limit. Fifty of these is fifty separate,
//!   individually-evidenced facts, not one suspicious global signal, so the
//!   pool heals on the next pass with no operator involvement.
//! * **Unconfirmed** (the DB says the run is NOT terminal, or the lookup
//!   failed) — the [`MAX_UNCONFIRMED_RETIREMENTS_PER_PASS`] limit applies
//!   exactly as before, and a burst of these still retires nothing at all.
//!   This is the partition the 2026-07-26 workers land in.
//!
//! ## Escalation (a refusal has to be loud)
//!
//! A safety valve that declines to act must say so somewhere a human looks.
//! The tripped breaker used to produce a `tracing::error!` and a dispatch
//! event and nothing else, which is why an hour-long pool wedge was found by
//! chance. A trip now also files a [`HUSK_BREAKER_ATTENTION_KIND`] attention
//! item naming every held-back slot and the manual reclaim command — the
//! same escalation channel [`crate::transient_recovery`] uses when it stops
//! auto-resuming, idempotent so repeated passes update rather than pile up.
//!
//! ## Cadence
//!
//! Runs every [`DEFAULT_INTERVAL`] and fires once immediately on boot, same
//! as every other sweep in this crate.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use boss_protocol::LiveWorkerState;

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

/// Most panes this sweep will retire in a single pass **on the strength of
/// engine bookkeeping alone** before tripping the mass-retirement circuit
/// breaker.
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
///
/// The limit counts only candidates whose death is NOT independently
/// confirmed — see [`death_evidence_from_db`] and the module docs. That
/// carve-out is what keeps "decline to act" from being self-sustaining
/// during a genuine mass-orphan event, without loosening the limit by one
/// for the amnesia case this constant exists to catch: the 2026-07-26
/// workers were `running` in the DB, so all five would still land in the
/// counted partition and all five would still be spared.
pub const MAX_UNCONFIRMED_RETIREMENTS_PER_PASS: usize = 3;

/// Attention-item `kind` filed when the mass-retirement circuit breaker
/// holds panes back. Distinct kind so the Notifications UI can surface
/// "the pool is wedged and the engine is refusing to unwedge it" apart
/// from the per-worker recovery escalations
/// [`crate::transient_recovery`] files, and so repeated passes dedupe
/// against the already-open item instead of piling up one row a minute.
pub const HUSK_BREAKER_ATTENTION_KIND: &str = "husk_retirement_breaker_tripped";

/// Per-candidate evidence, sourced independently of the
/// [`crate::live_worker_state::LiveWorkerStateRegistry`], about whether the
/// run behind a husk pane really is over.
///
/// The registry is what the husk classification itself reads, and
/// 2026-07-26 proved it can be uniformly wrong. The `work_executions` row
/// is the second opinion: it moves only on deliberate lifecycle
/// transitions, and the hook-event path that corrupted the registry that
/// day never touches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HuskDeathEvidence {
    /// The DB's own execution row is terminal — some deliberate transition
    /// (`request_resume_execution`, `mark_execution_orphaned`, the
    /// completion path) recorded this run as over. Positive, durable, and
    /// independent of the registry.
    DbConfirmedTerminal { status: String },
    /// The DB still considers this run non-terminal. The engine's two
    /// records of the same run disagree — which is either a live worker the
    /// registry forgot (the 2026-07-26 shape) or a run mid-transition.
    /// Either way, not confirmation.
    DbSaysNotTerminal { status: String },
    /// The spawn token has no row in `work_runs` at all. Every candidate
    /// this sweep sees reached it precisely because
    /// [`crate::tmux_adoption::classify_untracked_session`] already failed
    /// to resolve its token to an execution — so this is not "unknown", it
    /// is positive: the token was never durably recorded against any
    /// execution this engine ever dispatched, and therefore cannot belong
    /// to a run the engine still considers live. Independently confirmed,
    /// same as [`Self::DbConfirmedTerminal`].
    TokenUnknownToDb,
    /// The spawn-token or execution lookup itself failed (a DB error, not a
    /// miss). No opinion, so no confirmation.
    Unknown { reason: String },
}

impl HuskDeathEvidence {
    /// True for [`Self::DbConfirmedTerminal`] and [`Self::TokenUnknownToDb`].
    /// Absence of contradiction is deliberately not confirmation on its
    /// own — an unreadable DB must not be able to buy a candidate its way
    /// past the burst limit — but a token that provably never had a row is
    /// positive evidence, not absence of it.
    pub fn is_independently_confirmed(&self) -> bool {
        matches!(self, Self::DbConfirmedTerminal { .. } | Self::TokenUnknownToDb)
    }

    /// Short stable label for logs and dispatch-event details.
    pub fn label(&self) -> &'static str {
        match self {
            Self::DbConfirmedTerminal { .. } => "db_confirmed_terminal",
            Self::DbSaysNotTerminal { .. } => "db_says_not_terminal",
            Self::TokenUnknownToDb => "token_unknown_to_db",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// Human-readable detail for the attention item and the warn lines.
    pub fn detail(&self) -> String {
        match self {
            Self::DbConfirmedTerminal { status } | Self::DbSaysNotTerminal { status } => {
                format!("execution row status `{status}`")
            }
            Self::TokenUnknownToDb => "spawn token has no work_runs row in this engine's DB".to_owned(),
            Self::Unknown { reason } => reason.clone(),
        }
    }
}

/// Ask the work DB whether the run behind a husk candidate is terminal.
///
/// Candidates reach this sweep only after
/// [`crate::tmux_adoption::classify_untracked_session`] already tried and
/// failed to resolve their spawn token to an execution via
/// [`crate::work::WorkDb::execution_id_for_tmux_spawn_token`] — a husk's
/// `spawn_token` is NOT its execution id, so this repeats that same
/// token-keyed lookup (rather than treating the token as an execution id)
/// to see whether that classification still holds a pass later.
pub(crate) fn death_evidence_from_db(work_db: &WorkDb, spawn_token: &str) -> HuskDeathEvidence {
    match work_db.execution_id_for_tmux_spawn_token(spawn_token) {
        Ok(Some(execution_id)) => match work_db.get_execution(&execution_id) {
            Ok(execution) => {
                let status = execution.status.as_str().to_owned();
                if execution.status.is_terminal() {
                    HuskDeathEvidence::DbConfirmedTerminal { status }
                } else {
                    HuskDeathEvidence::DbSaysNotTerminal { status }
                }
            }
            Err(err) => HuskDeathEvidence::Unknown {
                reason: format!("execution lookup failed: {err}"),
            },
        },
        Ok(None) => HuskDeathEvidence::TokenUnknownToDb,
        Err(err) => HuskDeathEvidence::Unknown {
            reason: format!("spawn-token lookup failed: {err}"),
        },
    }
}

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
    /// Enumerate the private tmux server and return sessions whose token has
    /// no durable DB row. `None` means the inventory itself failed, so this
    /// pass must be skipped rather than treating a transient tmux failure as
    /// an empty server.
    async fn list_husk_candidates(&self) -> Option<Vec<crate::tmux_adoption::UntrackedTmuxSession>>;

    /// Retire this exact leaked tmux session. The session name, not a pool
    /// slot, is the tmux resource identity.
    async fn retire_husk(&self, session: &crate::tmux_adoption::UntrackedTmuxSession);
}

/// Counts from one sweep pass; logged at `info` when any pane was retired.
///
/// Constructed with [`Default`] and incremented in place; the `bon::Builder`
/// derive is present only to satisfy the repo's giant-struct convention
/// (`checkleft`'s rust-giant-structs-use-builder, which flags 6+ named
/// fields) — the sweep never builds one.
#[derive(Debug, Default, bon::Builder)]
pub struct HuskPaneSweepOutcome {
    /// Confirmed husks (seen on two consecutive passes) retired this pass.
    pub retired: usize,
    /// Husks observed for the first time this pass; held for one more pass
    /// before any retirement (two-pass confirmation).
    pub pending_confirmation: usize,
    /// How many of `retired` were exempt from the burst limit because the
    /// work DB independently confirmed the run terminal.
    pub independently_confirmed: usize,
    /// `true` when this pass's `list_husk_candidates` call failed and the
    /// pass was skipped conservatively.
    pub list_failed: bool,
    /// `Some(n)` when the mass-retirement circuit breaker tripped: `n`
    /// confirmed husks whose death is NOT independently confirmed — more
    /// than [`MAX_UNCONFIRMED_RETIREMENTS_PER_PASS`] — were held back rather
    /// than retired. Those candidates stay confirmed, so the breaker keeps
    /// tripping (and keeps logging) until the disagreement resolves; it
    /// never silently degrades into retiring them.
    pub breaker_tripped: Option<usize>,
    /// `true` when a breaker trip successfully filed (or refreshed) its
    /// [`HUSK_BREAKER_ATTENTION_KIND`] attention item. A trip that could
    /// not escalate is worse than one that could, so this is tracked
    /// rather than assumed.
    pub escalated: bool,
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
            independently_confirmed = self.independently_confirmed,
            pending_confirmation = self.pending_confirmation,
            breaker_tripped = ?self.breaker_tripped,
            escalated = self.escalated,
            "husk-pane sweep: retired leaked tmux session(s) with no durable token row",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`,
/// threading the cross-pass confirmation set so it survives between passes.
pub fn spawn_loop(
    source: Arc<dyn HuskPaneSweepSource>,
    work_db: Arc<WorkDb>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let seen_husks: Arc<tokio::sync::Mutex<HashSet<(String, String)>>> =
        Arc::new(tokio::sync::Mutex::new(HashSet::new()));
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let source = Arc::clone(&source);
        let work_db = Arc::clone(&work_db);
        let dispatch_events = Arc::clone(&dispatch_events);
        let seen_husks = Arc::clone(&seen_husks);
        async move {
            let mut seen_husks = seen_husks.lock().await;
            run_one_pass(
                source.as_ref(),
                work_db.as_ref(),
                dispatch_events.as_ref(),
                &mut seen_husks,
            )
            .await
        }
    })
}

/// Run a single husk-pane reconciliation pass. `seen_husks` carries the set
/// of `(session_name, spawn_token)` pairs observed as untracked on the
/// *previous* pass; on return it holds this pass's candidates so the next
/// pass can confirm them. Keying on the pair (not session name alone) means
/// a session name whose spawn token changes between passes never inherits a
/// prior leak's confirmation. Returns a summary; callers may log it.
pub async fn run_one_pass(
    source: &dyn HuskPaneSweepSource,
    work_db: &WorkDb,
    dispatch_events: &dyn DispatchEventSink,
    seen_husks: &mut HashSet<(String, String)>,
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
    // Key on `(session_name, spawn_token)` so a newly-created session cannot
    // inherit a prior leaked session's confirmation.
    let crate::sweep_loop::Confirmation { confirmed, pending } = crate::sweep_loop::confirm_two_pass(
        seen_husks,
        candidates
            .into_iter()
            .map(|session| ((session.session_name.clone(), session.spawn_token.clone()), session)),
    );

    outcome.pending_confirmation = pending.len();
    for pane in pending {
        // `warn`, not `debug`: this line is the last record written before
        // the next pass kills the pane's process, and it is the only place
        // the flagging decision is visible. At `debug` it was filtered out
        // in practice, which is why a five-worker retirement could not be
        // traced back to what the engine believed one pass earlier.
        tracing::warn!(
            session = %pane.session_name,
            "husk-pane sweep: tmux session with no durable spawn-token row observed; \
             awaiting next-pass confirmation before destroying it",
        );
    }

    // Weigh each confirmed candidate against a record the husk
    // classification did NOT consult and the 2026-07-26 signal could not
    // corrupt: the work DB's own execution row. See the module docs for why
    // this separates a genuine mass-orphan event from engine amnesia.
    let mut independently_dead: Vec<(crate::tmux_adoption::UntrackedTmuxSession, HuskDeathEvidence)> = Vec::new();
    let mut unconfirmed: Vec<(crate::tmux_adoption::UntrackedTmuxSession, HuskDeathEvidence)> = Vec::new();
    for pane in confirmed {
        let evidence = death_evidence_from_db(work_db, &pane.spawn_token);
        if evidence.is_independently_confirmed() {
            independently_dead.push((pane, evidence));
        } else {
            unconfirmed.push((pane, evidence));
        }
    }

    // Mass-retirement circuit breaker. See `MAX_UNCONFIRMED_RETIREMENTS_PER_PASS`
    // for why a burst of *unconfirmed* candidates is read as engine-side amnesia
    // rather than as a burst of genuine orphans, and why holding back is the
    // recoverable failure.
    //
    // `seen_husks` has already been carried forward by `confirm_two_pass`, so
    // held-back candidates stay confirmed: the breaker re-trips (and re-logs)
    // every pass for as long as the condition holds, instead of quietly
    // relaxing into a mass kill on some later pass.
    let mut to_retire = independently_dead;
    if unconfirmed.len() > MAX_UNCONFIRMED_RETIREMENTS_PER_PASS {
        outcome.breaker_tripped = Some(unconfirmed.len());
        tracing::error!(
            unconfirmed = unconfirmed.len(),
            max_per_pass = MAX_UNCONFIRMED_RETIREMENTS_PER_PASS,
            independently_confirmed = to_retire.len(),
            sessions = ?unconfirmed.iter().map(|(pane, _)| &pane.session_name).collect::<Vec<_>>(),
            "husk-pane sweep: MASS-RETIREMENT CIRCUIT BREAKER TRIPPED — {} tmux sessions confirmed as leaks in a single \
             pass WITHOUT independent confirmation that their runs are over exceeds the \
             {MAX_UNCONFIRMED_RETIREMENTS_PER_PASS}-per-pass limit. Retiring none of them: a burst this size with \
             the engine's own execution rows still disagreeing is far better explained by engine bookkeeping that \
             went wrong for every slot at once than by that many simultaneously-orphaned panes, and retiring a live \
             worker is unrecoverable. Verify the sessions named below by hand and reclaim genuine husks with \
             `tmux -L boss kill-session -t <session>`.",
            unconfirmed.len(),
        );
        for (pane, evidence) in &unconfirmed {
            tracing::error!(
                session = %pane.session_name,
                death_evidence = evidence.label(),
                evidence_detail = %evidence.detail(),
                "husk-pane sweep: held back by the mass-retirement circuit breaker; pane NOT retired",
            );
        }
        // Defect the 2026-07-26 breaker shipped with: declining silently.
        // The log line above is not a channel anyone watches, so raise the
        // refusal where an operator actually sees it.
        outcome.escalated = file_breaker_attention(work_db, &unconfirmed);
        for (pane, evidence) in unconfirmed {
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::HuskPaneReconcile, Outcome::Skipped, pane.spawn_token.clone())
                        .with_details(serde_json::json!({
                            "tmux_session_name": pane.session_name,
                            "skipped_reason": "mass_retirement_circuit_breaker",
                            "max_per_pass": MAX_UNCONFIRMED_RETIREMENTS_PER_PASS,
                            "death_evidence": evidence.label(),
                            "evidence_detail": evidence.detail(),
                            "escalated": outcome.escalated,
                        })),
                )
                .await;
        }
    } else {
        to_retire.extend(unconfirmed);
    }

    // A large reclaim is legitimate but still worth saying out loud: it means
    // something orphaned many runs at once (a provider outage driving
    // `transient_recovery`, a mass cancel). Logged rather than capped —
    // capping it is precisely the wedge this partition exists to prevent.
    let confirmed_dead_count = to_retire
        .iter()
        .filter(|(_, evidence)| evidence.is_independently_confirmed())
        .count();
    if confirmed_dead_count > MAX_UNCONFIRMED_RETIREMENTS_PER_PASS {
        tracing::warn!(
            independently_confirmed = confirmed_dead_count,
            sessions = ?to_retire.iter().map(|(pane, _)| &pane.session_name).collect::<Vec<_>>(),
            "husk-pane sweep: reclaiming {confirmed_dead_count} tmux sessions in one pass — above the \
             {MAX_UNCONFIRMED_RETIREMENTS_PER_PASS}-per-pass limit, but every one of them has an execution row the \
             engine itself already marked terminal, so this is a mass-orphan event and not the uniform-bookkeeping \
             failure the limit guards against.",
        );
    }

    for (pane, evidence) in to_retire {
        tracing::warn!(
            session = %pane.session_name,
            death_evidence = evidence.label(),
            evidence_detail = %evidence.detail(),
            "husk-pane sweep: tmux session has no durable spawn-token row; destroying leaked session",
        );

        source.retire_husk(&pane).await;
        outcome.retired += 1;
        if evidence.is_independently_confirmed() {
            outcome.independently_confirmed += 1;
        }

        dispatch_events
            .emit(
                DispatchEvent::new(Stage::HuskPaneReconcile, Outcome::Ok, pane.spawn_token.clone()).with_details(
                    serde_json::json!({
                        "tmux_session_name": pane.session_name,
                        "death_evidence": evidence.label(),
                        "evidence_detail": evidence.detail(),
                    }),
                ),
            )
            .await;
    }

    outcome
}

/// File (or refresh) the [`HUSK_BREAKER_ATTENTION_KIND`] attention item for a
/// tripped breaker. Returns whether an item was successfully filed.
///
/// Anchored to one affected work item rather than fanned out across all of
/// them, mirroring [`crate::trunk_queue_poller`]'s queue-level attentions: a
/// wedged pool is one fact, and N copies of it is noise. The body names every
/// held-back slot so the anchor choice costs no information. `held_back` is
/// scanned in slot order so the anchor is stable across passes and
/// `upsert_work_item_attention`'s open-item dedupe actually bites.
fn file_breaker_attention(
    work_db: &WorkDb,
    held_back: &[(crate::tmux_adoption::UntrackedTmuxSession, HuskDeathEvidence)],
) -> bool {
    let mut ordered: Vec<&(crate::tmux_adoption::UntrackedTmuxSession, HuskDeathEvidence)> = held_back.iter().collect();
    ordered.sort_by_key(|(pane, _)| (pane.session_name.clone(), pane.spawn_token.clone()));

    // A pane only lands in `held_back` (the unconfirmed partition) when its
    // spawn token DOES resolve to a `work_runs` row whose execution is not
    // terminal — a token with no row at all is [`HuskDeathEvidence::TokenUnknownToDb`],
    // which is independently confirmed and never reaches this function. So
    // this two-step, token-keyed lookup (never "treat the token as an
    // execution id") is expected to resolve for every held-back pane; the
    // `find_map` only has to survive an actual DB error on some candidates.
    let Some(work_item_id) = ordered.iter().find_map(|(pane, _)| {
        work_db
            .execution_id_for_tmux_spawn_token(&pane.spawn_token)
            .ok()
            .flatten()
            .and_then(|execution_id| work_db.get_execution(&execution_id).ok())
            .map(|execution| execution.work_item_id)
    }) else {
        tracing::error!(
            held_back = held_back.len(),
            "husk-pane sweep: circuit breaker tripped but NO held-back pane resolved to a work item, so the \
             refusal could not be escalated. Reclaim the sessions by hand, e.g. `tmux -L boss kill-session -t <session>`.",
        );
        return false;
    };

    let slot_lines = ordered
        .iter()
        .map(|(pane, evidence)| format!("- tmux session `{}` ({})", pane.session_name, evidence.detail(),))
        .collect::<Vec<_>>()
        .join("\n");
    let retire_lines = ordered
        .iter()
        .map(|(pane, _)| format!("tmux -L boss kill-session -t {}", pane.session_name))
        .collect::<Vec<_>>()
        .join("\n");

    let title = format!(
        "{} worker slot(s) wedged: husk retirement is being refused",
        ordered.len()
    );
    let body = format!(
        "The husk-pane sweep confirmed {count} tmux session(s) with no durable token row, which is more \
         than the {MAX_UNCONFIRMED_RETIREMENTS_PER_PASS}-per-pass mass-retirement limit for candidates whose death \
         the engine cannot independently confirm. **It retired none of them**, and it will keep refusing every pass \
         until the condition clears.\n\n\
         That refusal is deliberate — a burst this size usually means the engine's own worker bookkeeping went \
         wrong for every slot at once, and retiring a pane SIGTERMs a worker's process group irreversibly. But \
         while it holds, those slots stay claimed by panes the pool believes are free, so every dispatch that \
         lands on one is rejected `SlotBusy` and the work behind it cannot start.\n\n\
         **Held back:**\n\n{slot_lines}\n\n\
         **What to check:** each slot above has an execution row that does NOT say the run is over. If those \
         workers are genuinely still running, nothing is wrong with them and the fix belongs wherever the engine \
         lost track of them. If they are genuinely dead, reclaim the slots:\n\n\
         ```sh\n{retire_lines}\n```\n\n\
         Panes whose execution row the engine HAS marked terminal are exempt from this limit and are already being \
         reclaimed automatically; they will not appear above.",
        count = ordered.len(),
    );

    match work_db.upsert_work_item_attention(&work_item_id, HUSK_BREAKER_ATTENTION_KIND, &title, &body) {
        Ok(_) => true,
        Err(err) => {
            tracing::error!(
                work_item_id = %work_item_id,
                ?err,
                "husk-pane sweep: circuit breaker tripped but the attention item could not be filed; the refusal \
                 is visible only in this log",
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::test_support::{create_active_chore, create_product, open_db};

    /// A DB with no execution rows at all. Every husk candidate looked up
    /// against it yields [`HuskDeathEvidence::TokenUnknownToDb`] — the
    /// production shape, since a candidate only ever reaches this sweep
    /// after `classify_untracked_session` already failed to resolve its
    /// token to any execution.
    fn empty_db() -> (TempDir, WorkDb) {
        open_db()
    }

    fn husk(slot_id: u8, spawn_token: &str) -> crate::tmux_adoption::UntrackedTmuxSession {
        crate::tmux_adoption::UntrackedTmuxSession {
            session_name: format!("boss-worker-{slot_id}"),
            spawn_token: spawn_token.to_owned(),
        }
    }

    /// Test double that returns a scripted sequence of `list_husk_candidates`
    /// results (one per pass) and records every `retire_husk` call.
    struct ScriptedSource {
        passes: Mutex<std::collections::VecDeque<Option<Vec<crate::tmux_adoption::UntrackedTmuxSession>>>>,
        retired: Mutex<Vec<String>>,
    }

    impl ScriptedSource {
        fn new(passes: Vec<Option<Vec<crate::tmux_adoption::UntrackedTmuxSession>>>) -> Self {
            Self {
                passes: Mutex::new(passes.into()),
                retired: Mutex::new(Vec::new()),
            }
        }

        fn retired(&self) -> Vec<String> {
            self.retired.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HuskPaneSweepSource for ScriptedSource {
        async fn list_husk_candidates(&self) -> Option<Vec<crate::tmux_adoption::UntrackedTmuxSession>> {
            self.passes.lock().unwrap().pop_front().flatten()
        }

        async fn retire_husk(&self, session: &crate::tmux_adoption::UntrackedTmuxSession) {
            self.retired.lock().unwrap().push(session.session_name.clone());
        }
    }

    /// The core invariant: a husk observed on two consecutive passes is
    /// retired on the second, not the first.
    #[tokio::test]
    async fn retires_husk_confirmed_across_two_passes() {
        let source = ScriptedSource::new(vec![Some(vec![husk(7, "exec-a")]), Some(vec![husk(7, "exec-a")])]);
        let sink = RecordingDispatchEventSink::new();
        let (_dir, db) = empty_db();
        let mut seen = HashSet::new();

        let first = run_one_pass(&source, &db, &sink, &mut seen).await;
        assert_eq!(first.retired, 0, "first pass must only record the candidate");
        assert_eq!(first.pending_confirmation, 1);
        assert!(source.retired().is_empty());
        assert!(sink.events().await.is_empty());

        let second = run_one_pass(&source, &db, &sink, &mut seen).await;
        assert_eq!(second.retired, 1, "second pass must retire the confirmed husk");
        assert_eq!(source.retired(), vec!["boss-worker-7"]);

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "husk_pane_reconcile");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].execution_id, "exec-a");
        assert_eq!(events[0].details["tmux_session_name"], "boss-worker-7");
    }

    /// A husk that disappears before confirmation (the registration landed,
    /// or something else cleared it) is never retired.
    #[tokio::test]
    async fn does_not_retire_when_husk_clears_before_confirmation() {
        let source = ScriptedSource::new(vec![Some(vec![husk(3, "exec-b")]), Some(vec![])]);
        let sink = RecordingDispatchEventSink::new();
        let (_dir, db) = empty_db();
        let mut seen = HashSet::new();

        let first = run_one_pass(&source, &db, &sink, &mut seen).await;
        assert_eq!(first.pending_confirmation, 1);

        let second = run_one_pass(&source, &db, &sink, &mut seen).await;
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
        let (_dir, db) = empty_db();
        let mut seen = HashSet::new();

        let first = run_one_pass(&source, &db, &sink, &mut seen).await;
        assert_eq!(first.pending_confirmation, 1);

        let second = run_one_pass(&source, &db, &sink, &mut seen).await;
        assert!(second.list_failed);
        assert_eq!(second.retired, 0);
        assert_eq!(seen.len(), 1, "seen set must survive the failed pass unchanged");

        let third = run_one_pass(&source, &db, &sink, &mut seen).await;
        assert_eq!(
            third.retired, 1,
            "the pre-blip observation must still count toward confirmation"
        );
        assert_eq!(source.retired(), vec!["boss-worker-5"]);
    }

    /// No husks at all across several passes is simply quiet.
    #[tokio::test]
    async fn no_husks_is_a_no_op() {
        let source = ScriptedSource::new(vec![Some(vec![]), Some(vec![])]);
        let sink = RecordingDispatchEventSink::new();
        let (_dir, db) = empty_db();
        let mut seen = HashSet::new();

        for _ in 0..2 {
            let outcome = run_one_pass(&source, &db, &sink, &mut seen).await;
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
        let (_dir, db) = empty_db();
        let mut seen = HashSet::new();

        run_one_pass(&source, &db, &sink, &mut seen).await;
        let second = run_one_pass(&source, &db, &sink, &mut seen).await;

        assert_eq!(second.retired, 2);
        let mut retired = source.retired();
        retired.sort_unstable_by_key(|session| {
            session
                .strip_prefix("boss-worker-")
                .and_then(|slot| slot.parse::<u8>().ok())
                .expect("fixture sessions use numeric boss-worker slots")
        });
        assert_eq!(retired, vec!["boss-worker-1", "boss-worker-2"]);
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
        let (_dir, db) = empty_db();
        let mut seen = HashSet::new();

        let first = run_one_pass(&source, &db, &sink, &mut seen).await;
        assert_eq!(first.pending_confirmation, 1);

        let second = run_one_pass(&source, &db, &sink, &mut seen).await;
        assert_eq!(
            second.retired, 0,
            "a different run on the same slot must not be treated as the same confirmed husk"
        );
        assert_eq!(
            second.pending_confirmation, 1,
            "the new run starts its own confirmation clock"
        );
        assert!(source.retired().is_empty());

        let third = run_one_pass(&source, &db, &sink, &mut seen).await;
        assert_eq!(
            third.retired, 1,
            "the new run is retired once it, too, is confirmed across two passes"
        );
        assert_eq!(source.retired(), vec!["boss-worker-9"]);

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].execution_id, "exec-second");
    }

    // ─── mass-retirement circuit breaker ─────────────────────────────────────
    //
    // These use `EvidenceDb::running_session_token` to build genuine
    // "unconfirmed" candidates: a real `work_runs` row whose execution is
    // not terminal. That is the ONLY shape that still lands in the
    // breaker's counted partition — a spawn token with no row at all is
    // `HuskDeathEvidence::TokenUnknownToDb`, independently confirmed, and
    // bypasses the breaker entirely (see
    // `tokens_unknown_to_the_db_are_independently_confirmed_and_reclaimed`
    // below the "independent death evidence" divider).

    fn unconfirmed_husks(fixture: &EvidenceDb, count: usize) -> Vec<crate::tmux_adoption::UntrackedTmuxSession> {
        (0..count)
            .map(|i| husk(i as u8, &fixture.running_session_token()))
            .collect()
    }

    /// Spawn tokens with no `work_runs` row anywhere in the DB — the shape
    /// every real candidate this sweep sees takes, since
    /// `classify_untracked_session` already failed to resolve them to an
    /// execution before this sweep ever saw them.
    fn unknown_token_husks(count: usize) -> Vec<crate::tmux_adoption::UntrackedTmuxSession> {
        (0..count).map(|i| husk(i as u8, &format!("spawn-token-{i}"))).collect()
    }

    /// Exactly [`MAX_UNCONFIRMED_RETIREMENTS_PER_PASS`] confirmed husks is under the
    /// limit and must still be retired — the breaker must not shrink the
    /// sweep's normal reclaim behaviour.
    #[tokio::test]
    async fn breaker_allows_a_pass_at_the_limit() {
        let fixture = EvidenceDb::new();
        let batch = unconfirmed_husks(&fixture, MAX_UNCONFIRMED_RETIREMENTS_PER_PASS);
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        let second = run_one_pass(&source, &fixture.db, &sink, &mut seen).await;

        assert_eq!(second.retired, MAX_UNCONFIRMED_RETIREMENTS_PER_PASS);
        assert_eq!(second.breaker_tripped, None);
        assert_eq!(source.retired().len(), MAX_UNCONFIRMED_RETIREMENTS_PER_PASS);
    }

    /// Regression test for the 2026-07-26 incident's blast radius: five
    /// panes confirmed in one pass must retire NOTHING. Whatever produced
    /// five simultaneous husks is a statement about the engine, not about
    /// five independently-orphaned panes, and the kill is irreversible.
    #[tokio::test]
    async fn breaker_trips_and_retires_nothing_above_the_limit() {
        let fixture = EvidenceDb::new();
        let batch = unconfirmed_husks(&fixture, MAX_UNCONFIRMED_RETIREMENTS_PER_PASS + 2);
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        let second = run_one_pass(&source, &fixture.db, &sink, &mut seen).await;

        assert_eq!(second.breaker_tripped, Some(MAX_UNCONFIRMED_RETIREMENTS_PER_PASS + 2));
        assert_eq!(second.retired, 0, "the breaker must retire nothing at all");
        assert!(
            source.retired().is_empty(),
            "no pane may be torn down while the breaker is tripped"
        );

        // Every held-back pane is reported as skipped, so the burst is
        // visible in the dispatch stream and not merely in the log.
        let events = sink.events().await;
        assert_eq!(events.len(), MAX_UNCONFIRMED_RETIREMENTS_PER_PASS + 2);
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
        let fixture = EvidenceDb::new();
        let batch = unconfirmed_husks(&fixture, MAX_UNCONFIRMED_RETIREMENTS_PER_PASS + 2);
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch.clone()), Some(batch)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        let third = run_one_pass(&source, &fixture.db, &sink, &mut seen).await;

        assert_eq!(third.breaker_tripped, Some(MAX_UNCONFIRMED_RETIREMENTS_PER_PASS + 2));
        assert_eq!(third.retired, 0);
        assert!(source.retired().is_empty());
    }

    /// Once the burst subsides to a plausible size, the sweep resumes
    /// reclaiming genuine husks — the breaker is a rate limit, not a latch
    /// that disables the sweep forever.
    #[tokio::test]
    async fn breaker_resets_once_the_burst_subsides() {
        let fixture = EvidenceDb::new();
        let big = unconfirmed_husks(&fixture, MAX_UNCONFIRMED_RETIREMENTS_PER_PASS + 2);
        // Reuse the first big-batch candidate's exact (session_name, spawn_token)
        // pair, rather than minting a fresh one, so it is already confirmed from
        // the prior passes and this pass reclaims it immediately.
        let small = vec![big[0].clone()];
        let source = ScriptedSource::new(vec![Some(big.clone()), Some(big), Some(small)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        let tripped = run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        assert_eq!(tripped.retired, 0);

        let third = run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        assert_eq!(third.breaker_tripped, None);
        assert_eq!(third.retired, 1, "a single husk is retired normally again");
        assert_eq!(source.retired(), vec!["boss-worker-0"]);
    }

    // ─── independent death evidence ──────────────────────────────────────────

    /// A DB holding real work rows, so husk candidates can be looked up for
    /// death evidence the way they are in production.
    struct EvidenceDb {
        _dir: TempDir,
        db: WorkDb,
        product_id: String,
        /// One chore per execution — a work item may hold only one live
        /// execution at a time, and a real burst is many different chores'
        /// workers dying at once anyway.
        work_item_ids: Mutex<Vec<String>>,
    }

    impl EvidenceDb {
        fn new() -> Self {
            let (dir, db) = open_db();
            let product_id = create_product(&db);
            Self {
                _dir: dir,
                db,
                product_id,
                work_item_ids: Mutex::new(Vec::new()),
            }
        }

        /// Create an execution that has actually run, and return its id — the
        /// same id the app reports as a hosted pane's `run_id`.
        fn running_execution(&self) -> String {
            use boss_protocol::RequestExecutionInput;
            let mut work_item_ids = self.work_item_ids.lock().unwrap();
            let work_item_id = create_active_chore(
                &self.db,
                &self.product_id,
                &format!("wedged chore {}", work_item_ids.len()),
            );
            work_item_ids.push(work_item_id.clone());
            drop(work_item_ids);

            let execution = self
                .db
                .request_execution(
                    RequestExecutionInput::builder()
                        .work_item_id(&work_item_id)
                        .preferred_workspace_id("mono-agent-007")
                        .build(),
                )
                .unwrap();
            self.db
                .start_execution_run(
                    &execution.id,
                    "worker-1",
                    "repo-1",
                    "lease-1",
                    "mono-agent-007",
                    "/tmp/mono-agent-007",
                )
                .unwrap();
            execution.id
        }

        /// A run the engine deliberately gave up on — what
        /// `transient_recovery`'s orphan+respawn leaves behind when a worker
        /// dies on a `529 Overloaded`.
        fn orphaned_execution(&self) -> String {
            let id = self.running_execution();
            self.db.mark_execution_orphaned(&id, "529 Overloaded").unwrap();
            id
        }

        /// Record a genuine tmux spawn token for an execution — the same
        /// call `crate::tmux_adoption`'s spawn path makes before tmux ever
        /// creates the session. Deliberately distinct from the execution
        /// id: [`crate::tmux_adoption::UntrackedTmuxSession::spawn_token`]
        /// is never an execution id in production, and a fixture that used
        /// the execution id as the token would exercise a lookup shape
        /// `death_evidence_from_db` never actually sees.
        fn record_spawn_token(&self, execution_id: &str) -> String {
            let token = format!("spawn-token-for-{execution_id}");
            self.db
                .record_tmux_spawn_intent_for_execution(
                    execution_id,
                    "boss",
                    &format!("session-{execution_id}"),
                    &token,
                )
                .unwrap();
            token
        }

        /// A genuine "unconfirmed" candidate: a real, non-terminal
        /// execution's spawn token, the only shape
        /// [`HuskDeathEvidence::DbSaysNotTerminal`] can arise from.
        fn running_session_token(&self) -> String {
            let execution_id = self.running_execution();
            self.record_spawn_token(&execution_id)
        }

        /// A genuine "independently confirmed dead" candidate sourced from a
        /// real terminal execution row, as opposed to
        /// [`HuskDeathEvidence::TokenUnknownToDb`], the other
        /// independently-confirmed shape.
        fn orphaned_session_token(&self) -> String {
            let execution_id = self.orphaned_execution();
            self.record_spawn_token(&execution_id)
        }

        /// Every breaker attention item across every work item this fixture
        /// created — the escalation anchors to one of them, and the test
        /// should not have to know which.
        fn breaker_attentions(&self) -> Vec<boss_protocol::WorkAttentionItem> {
            self.work_item_ids
                .lock()
                .unwrap()
                .iter()
                .flat_map(|work_item_id| self.db.list_attention_items_for_work_item(work_item_id).unwrap())
                .filter(|item| item.kind == HUSK_BREAKER_ATTENTION_KIND)
                .collect()
        }
    }

    /// The wedge, end to end. Eleven workers die on a transient API error,
    /// `transient_recovery` orphans each execution and frees each slot's
    /// engine-side bookkeeping without tearing the pane down, and the app is
    /// left hosting eleven panes the pool believes are free. Before this
    /// change the breaker refused all eleven on every pass forever and the
    /// only way out was eleven `bossctl agents retire-pane` invocations.
    ///
    /// Every one of those runs is `orphaned` in the DB — the engine's own
    /// durable record that it is over — so the pool now heals itself on the
    /// pass after confirmation, with no operator involvement and no
    /// escalation, because there is nothing left for an operator to do.
    #[tokio::test]
    async fn mass_orphan_burst_reclaims_every_slot_without_manual_intervention() {
        let fixture = EvidenceDb::new();
        let batch: Vec<crate::tmux_adoption::UntrackedTmuxSession> = (0..11)
            .map(|slot| husk(slot as u8, &fixture.orphaned_session_token()))
            .collect();
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        let first = run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        assert_eq!(first.pending_confirmation, 11, "first pass only observes");
        assert_eq!(first.retired, 0);

        let second = run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        assert_eq!(
            second.breaker_tripped, None,
            "eleven independently-confirmed dead runs are eleven facts, not one bad global signal",
        );
        assert_eq!(second.retired, 11, "every wedged slot must be reclaimed");
        assert_eq!(second.independently_confirmed, 11);

        let mut retired = source.retired();
        retired.sort_unstable_by_key(|session| {
            session
                .strip_prefix("boss-worker-")
                .and_then(|slot| slot.parse::<u8>().ok())
                .expect("fixture sessions use numeric boss-worker slots")
        });
        assert_eq!(
            retired,
            (0..11).map(|slot| format!("boss-worker-{slot}")).collect::<Vec<_>>(),
            "the whole pool comes back"
        );

        // Nothing to escalate: the engine resolved it.
        assert!(!second.escalated);
        assert!(fixture.breaker_attentions().is_empty());

        // Each retirement records the evidence that justified it, so the
        // decision is auditable in `bossctl dispatch tail` after the fact.
        let events = sink.events().await;
        assert_eq!(events.len(), 11);
        assert!(events.iter().all(|event| event.outcome == "ok"));
        assert!(
            events
                .iter()
                .all(|event| event.details["death_evidence"] == "db_confirmed_terminal")
        );
    }

    /// The 2026-07-26 incident replayed against the new partition: five live
    /// workers whose registry entries a spurious `SessionEnd` burst flipped
    /// to terminal. Their executions are still `running` in the DB — that
    /// burst never touched it — so all five are unconfirmed, the breaker
    /// trips exactly as before, and not one pane is torn down.
    ///
    /// The refusal is now also escalated, which is the half that was missing:
    /// the incident that motivated this row went unnoticed for an hour
    /// because declining produced only a log line.
    #[tokio::test]
    async fn breaker_still_refuses_and_now_escalates_when_the_db_says_the_runs_are_live() {
        let fixture = EvidenceDb::new();
        let batch: Vec<crate::tmux_adoption::UntrackedTmuxSession> = (0..5)
            .map(|slot| husk(slot as u8, &fixture.running_session_token()))
            .collect();
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        let second = run_one_pass(&source, &fixture.db, &sink, &mut seen).await;

        assert_eq!(second.breaker_tripped, Some(5));
        assert_eq!(second.retired, 0, "a live worker must never be retired");
        assert!(source.retired().is_empty());

        assert!(second.escalated, "a refusal has to reach a human");
        let attentions = fixture.breaker_attentions();
        assert_eq!(attentions.len(), 1);
        assert_eq!(attentions[0].status, "open");
        for slot in 0..5 {
            assert!(
                attentions[0]
                    .body_markdown
                    .contains(&format!("tmux session `boss-worker-{slot}`")),
                "every held-back slot must be named in the escalation: {}",
                attentions[0].body_markdown,
            );
            assert!(
                attentions[0]
                    .body_markdown
                    .contains(&format!("tmux -L boss kill-session -t boss-worker-{slot}")),
                "the escalation must carry the manual reclaim command",
            );
        }

        let events = sink.events().await;
        assert_eq!(events.len(), 5);
        assert!(
            events
                .iter()
                .all(|event| event.details["death_evidence"] == "db_says_not_terminal")
        );
    }

    /// The partitions are independent. A pass carrying both shapes at once
    /// reclaims the provably-dead panes and still holds back the suspicious
    /// ones — a burst of contradictions must not strand slots the engine has
    /// positive evidence about, and provable deaths must not buy the
    /// contradictions a pass.
    #[tokio::test]
    async fn mixed_burst_reclaims_the_confirmed_dead_and_holds_back_the_rest() {
        let fixture = EvidenceDb::new();
        let mut batch: Vec<crate::tmux_adoption::UntrackedTmuxSession> = (0..6)
            .map(|slot| husk(slot as u8, &fixture.orphaned_session_token()))
            .collect();
        batch.extend((6..10).map(|slot| husk(slot as u8, &fixture.running_session_token())));
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch)]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        let second = run_one_pass(&source, &fixture.db, &sink, &mut seen).await;

        assert_eq!(second.breaker_tripped, Some(4), "only the unconfirmed four are counted");
        assert_eq!(second.retired, 6);
        assert_eq!(second.independently_confirmed, 6);
        let mut retired = source.retired();
        retired.sort_unstable();
        assert_eq!(
            retired,
            (0..6).map(|slot| format!("boss-worker-{slot}")).collect::<Vec<_>>()
        );
        assert!(second.escalated);
    }

    /// A persisting burst re-trips every pass by design. The escalation must
    /// not re-file every 60s — `upsert_work_item_attention` dedupes against
    /// the open item, so the operator sees one notification, not one a
    /// minute for as long as the pool is wedged.
    #[tokio::test]
    async fn escalation_dedupes_while_the_burst_persists() {
        let fixture = EvidenceDb::new();
        let batch: Vec<crate::tmux_adoption::UntrackedTmuxSession> = (0..5)
            .map(|slot| husk(slot as u8, &fixture.running_session_token()))
            .collect();
        let source = ScriptedSource::new(vec![
            Some(batch.clone()),
            Some(batch.clone()),
            Some(batch.clone()),
            Some(batch),
        ]);
        let sink = RecordingDispatchEventSink::new();
        let mut seen = HashSet::new();

        for _ in 0..4 {
            run_one_pass(&source, &fixture.db, &sink, &mut seen).await;
        }

        assert_eq!(
            fixture.breaker_attentions().len(),
            1,
            "three consecutive trips must not file three attention items",
        );
    }

    /// The only production shape: every real candidate this sweep sees
    /// reached it because `classify_untracked_session` already failed to
    /// resolve its spawn token to any execution at all — there is no row
    /// for the DB to disagree with. That is positive evidence the token
    /// cannot belong to a live tracked run, so a burst of these bypasses
    /// the breaker entirely and reclaims every slot, unlike a burst of the
    /// [`EvidenceDb`]-backed "unconfirmed" shape exercised above.
    #[tokio::test]
    async fn tokens_unknown_to_the_db_are_independently_confirmed_and_reclaimed() {
        let batch = unknown_token_husks(5);
        let source = ScriptedSource::new(vec![Some(batch.clone()), Some(batch)]);
        let sink = RecordingDispatchEventSink::new();
        let (_dir, db) = empty_db();
        let mut seen = HashSet::new();

        run_one_pass(&source, &db, &sink, &mut seen).await;
        let second = run_one_pass(&source, &db, &sink, &mut seen).await;

        assert_eq!(
            second.breaker_tripped, None,
            "there is no execution row for the breaker to be suspicious of",
        );
        assert_eq!(second.retired, 5);
        assert_eq!(second.independently_confirmed, 5);
        assert_eq!(source.retired().len(), 5);

        let events = sink.events().await;
        assert_eq!(events.len(), 5);
        assert!(
            events
                .iter()
                .all(|event| event.details["death_evidence"] == "token_unknown_to_db")
        );
    }

    /// `death_evidence_from_db` reads the execution row and nothing else.
    #[tokio::test]
    async fn death_evidence_reads_the_execution_row() {
        let fixture = EvidenceDb::new();

        let orphaned = death_evidence_from_db(&fixture.db, &fixture.orphaned_session_token());
        assert!(orphaned.is_independently_confirmed());
        assert_eq!(orphaned.label(), "db_confirmed_terminal");
        assert!(orphaned.detail().contains("orphaned"));

        let running = death_evidence_from_db(&fixture.db, &fixture.running_session_token());
        assert!(!running.is_independently_confirmed());
        assert_eq!(running.label(), "db_says_not_terminal");
        assert!(running.detail().contains("running"));

        let unknown_token = death_evidence_from_db(&fixture.db, "spawn-token-that-never-existed");
        assert!(
            unknown_token.is_independently_confirmed(),
            "a token with no work_runs row at all can never belong to a live tracked run",
        );
        assert_eq!(unknown_token.label(), "token_unknown_to_db");
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
