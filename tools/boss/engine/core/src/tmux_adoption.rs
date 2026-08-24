//! Boot-time tmux worker adoption pass.
//!
//! Every engine restart empties the engine's *derived bookkeeping* — the
//! `WorkerPool` slot claims, the [`crate::worker_registry::WorkerRegistry`]
//! pid/slot map, and the [`crate::live_worker_state::LiveWorkerStateRegistry`]
//! — even though the DB rows those structures were tracking may still be
//! perfectly live (see `tools/boss/docs/worker-liveness-contract.md`'s three
//! layers). For an app-hosted worker pane the app itself survives the engine
//! restart and can be asked what it hosts
//! ([`crate::app::readoption::ServerState::hosted_pane_slot_for_run`]). A
//! tmux-hosted worker has no such oracle to ask — nothing but tmux itself
//! knows the session is still there — so without this pass every tmux-hosted
//! worker would sit invisible to `bossctl agents list` until it happened to
//! hook or go terminal-and-get-reaped.
//!
//! This module closes that gap. On every boot, before
//! [`crate::run_reconcile`] gets a turn:
//!
//! 1. Enumerate the private `boss` tmux server ([`Tmux::list_sessions`]).
//! 2. For each session, read its **authoritative** identity token —
//!    `show-environment BOSS_SPAWN_TOKEN`, not the `@boss_spawn_token`
//!    session-option mirror `list_sessions` also returns. The option is set
//!    by a follow-up `tmux set-option` call after session creation
//!    ([`crate::spawn_flow`]'s `start_tmux_worker`); the environment
//!    variable is carried atomically by `new-session -e` at creation and can
//!    never be missing on a session this engine actually created.
//! 3. Exact-match every live token against
//!    [`crate::work::WorkDb::list_adoptable_tmux_runs`] — the non-terminal,
//!    local, tmux-tracked `work_runs` rows. A match proves the worker
//!    survived the restart, so its slot claim, `WorkerRegistry` entries,
//!    `LiveWorkerState` entry, and live-status summarizer task are rebuilt
//!    from the DB row plus a fresh read of the session's pane pid — the same
//!    things [`crate::app::readoption`] rebuilds for a *terminalized*
//!    survivor, minus the DB status flip that path needs and this one
//!    doesn't (the row was never wrong here).
//!
//!    A match whose `tmux_spawn_state` is still `intended` additionally
//!    repairs the DB: `intended` means the engine crashed between `tmux
//!    new-session` and the confirmation write
//!    ([`crate::work::WorkDb::record_tmux_session_created`]), and a live
//!    session with a matching token is exactly the durable evidence that
//!    write was only ever lost in-memory, not that the session never
//!    happened.
//! 4. Any live token that matched no adoptable row is looked up again with no
//!    status filter at all
//!    ([`crate::work::WorkDb::execution_id_for_tmux_spawn_token`]). If that
//!    resolves to a TERMINAL execution, the session is a live worker for a
//!    row the engine believes is dead — precisely the contradiction
//!    [`crate::worker_readoption`] exists to resolve — so it is handed off
//!    to that policy unchanged, via
//!    [`LiveWorkerConvergence::converge_live_worker`]. A token that resolves
//!    to nothing at all (or to a non-terminal execution this pass didn't
//!    adopt for some other reason, e.g. a non-`local` host) is left alone:
//!    the leaked/husk fan-out sweep across every enumerated session is a
//!    separate, dependent pass.
//!
//! Best-effort throughout, matching every other startup reconciler in this
//! crate: a DB read that fails, or a single session's `show-environment` or
//! `display-message` erroring out, never blocks engine startup — it is
//! logged and that session (or the whole pass) is skipped. The caller is
//! responsible for resolving [`Tmux`] itself and deciding what a resolve
//! failure means (see `app/server.rs`).
//!
//! ## Two guards ahead of every adoption decision
//!
//! Before a token match is ever handed to [`adopt_one`], two independent
//! questions are answered, because either one being wrong turns "adopt" into
//! "double-adopt":
//!
//! 1. **Is this engine process the only one adopting from this tmux server
//!    right now?** [`claim_or_detect_conflicting_owner`] stamps the
//!    server-scoped `@boss_engine_owner` option with this process's pid, or
//!    detects that a *different*, still-live engine process already holds
//!    it. A positive conflict refuses the entire pass — not just one
//!    session — because the property being protected (no two engines ever
//!    control the same worker) has nothing to do with which particular
//!    session is at issue. See the design doc's "Two engines briefly running
//!    at once" failure mode.
//! 2. **Was this session written by a build this engine can trust?** Every
//!    session this engine creates carries `BOSS_SESSION_SCHEMA`
//!    ([`crate::spawn_flow::TMUX_SESSION_SCHEMA`]) atomically at creation.
//!    [`check_session_schema`] rejects a session whose schema is missing,
//!    unparseable, or newer than this engine's own contract — "unknown"
//!    schemas are always version skew, even when the actual number happens
//!    to be smaller, because a schema this engine has never heard of is not
//!    evidence of anything. A session that fails this guard is refused
//!    *and reaped* by [`refuse_and_reap`]: refusing to adopt while leaving
//!    the session alive would let a redispatch put a second live worker in
//!    the same cube workspace, which is exactly what every other guard in
//!    this module exists to prevent. This check runs once per live session
//!    — before the token is even matched against an adoptable row — so it
//!    covers *every* adoption decision this pass can make, not just
//!    [`adopt_one`]: a live token that instead resolves to a terminal
//!    execution and is handed to [`crate::worker_readoption`] goes through
//!    the identical refuse-and-reap check in [`classify_untracked_session`] first.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use boss_tmux::{DisplayField, TMUX_SPAWN_TOKEN_ENV, Tmux};

use crate::coordinator::{ExecutionCoordinator, slot_id_from_worker_id};
use crate::dead_pid_sweep::{PidStatus, probe_pid};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::{LiveSpawnRouting, ReadoptionEvidence, attributed_pool_label};
use crate::spawn_flow::{TMUX_SESSION_SCHEMA, WorkerSpawner};
use crate::work::{TmuxRunHandle, WorkDb};
use crate::worker_readoption::LiveWorkerConvergence;

/// The tmux session environment variable carrying [`TMUX_SESSION_SCHEMA`].
/// Names what a different process generation wrote into its live session, so
/// it stays local to this compatibility boundary rather than being imported
/// from the spawn path that writes it.
const TMUX_SESSION_SCHEMA_ENV: &str = "BOSS_SESSION_SCHEMA";

/// Server-scoped tmux user option recording which engine process currently
/// owns Boss's private tmux server, set with `set-option -s` at the top of
/// every boot-time adoption pass. Orthogonal to `@boss_spawn_token`: that
/// option answers "is this session ours"; this one answers "is this process
/// the only engine currently adopting from this server". See the design
/// doc's "Two engines briefly running at once" failure mode.
const ENGINE_OWNER_OPTION: &str = "@boss_engine_owner";

/// `work_attention_items.kind` filed when [`refuse_and_reap`] reaps a
/// version-skewed session. Registered in [`crate::attention_lifecycle`] as
/// [`crate::attention_lifecycle::ClearedBy::WorkResumed`] — a later run
/// starting on the same work item is direct evidence the item is moving
/// again, the same shape as [`crate::dead_pid_sweep::PANE_DEATH_ATTENTION_KIND`].
pub const TMUX_ADOPTION_SCHEMA_SKEW_ATTENTION_KIND: &str = "tmux_adoption_schema_skew";

/// `work_attention_items.kind` filed when live sessions remain on the
/// pre-move `tmux -L boss` server after the engine has switched to an
/// explicit socket. Cleared when a later run starts on the same item.
pub const TMUX_LEGACY_LABEL_SERVER_ATTENTION_KIND: &str = "tmux_legacy_label_server";

/// Trigger name recorded on the dispatch event and carried into
/// [`LiveWorkerConvergence::converge_live_worker`] for a session whose token
/// matched a terminal execution — the third trigger alongside
/// `hook_after_terminal` and `redispatch_guard` (see
/// [`crate::worker_readoption`]'s module doc).
const TERMINAL_HANDOFF_TRIGGER: &str = "tmux_session_sweep";

/// A live Boss-owned tmux session whose spawn token has no durable run row.
///
/// The token is the durable identity, while the session name is the resource
/// the sweep must destroy after its two-pass confirmation succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrackedTmuxSession {
    pub session_name: String,
    pub spawn_token: String,
}

/// Read the authoritative durable identity token for a tmux session. Callers
/// deliberately choose their mismatch policy: boot adoption leaves foreign
/// sessions alone, while the liveness sweep treats a durable run's mismatch as
/// evidence that its pane is gone.
pub async fn session_spawn_token(tmux: &Tmux, session_name: &str) -> anyhow::Result<Option<String>> {
    tmux.show_environment(session_name, TMUX_SPAWN_TOKEN_ENV).await
}

/// What one pass did; the caller logs it.
///
/// Constructed with [`Default`] and its fields set directly; the
/// `bon::Builder` derive is present only to satisfy checkleft's
/// `rust/giant-structs` check (6+ named fields require it in
/// `boss-engine`'s internal types) — no caller builds one via
/// `TmuxAdoptionOutcome::builder()`. Mirrors [`crate::husk_pane_sweep::HuskPaneSweepOutcome`]'s
/// identical situation.
#[derive(Debug, Default, Clone, bon::Builder)]
pub struct TmuxAdoptionOutcome {
    /// Execution ids whose derived bookkeeping was rebuilt this pass.
    /// [`crate::run_reconcile`] should not re-probe these against cube — tmux
    /// already proved them alive more directly than a cube lease snapshot
    /// can.
    pub adopted_execution_ids: HashSet<String>,
    /// Adopted runs whose `tmux_spawn_state` was still `intended` and got
    /// durably repaired to `created` by this pass.
    pub repaired_intents: usize,
    /// Live sessions handed off to [`crate::worker_readoption`] because their
    /// token resolved to a terminal execution.
    pub terminal_handoffs: usize,
    /// Live sessions refused and reaped by [`refuse_and_reap`] because their
    /// `BOSS_SESSION_SCHEMA` failed [`check_session_schema`].
    pub refused_schema_skew: usize,
    /// `true` when this pass refused to run at all because a different,
    /// still-live engine process already holds `@boss_engine_owner` — see
    /// [`claim_or_detect_conflicting_owner`]. When `true`, every other field
    /// is zero/empty: nothing was enumerated.
    pub owner_conflict: bool,
    /// Sessions whose token has no row in this engine DB. They cannot be
    /// adopted or handed to contradiction convergence; the periodic husk
    /// sweep confirms and reaps them separately.
    pub untracked_sessions: Vec<UntrackedTmuxSession>,
}

/// Run the startup invocation of [`run_adoption_pass`], adding the
/// once-per-boot ownership guard ahead of it.
///
/// Kept as a named entry point so the startup ordering in `app/server.rs`
/// remains explicit; periodic callers use [`run_adoption_pass`] directly and
/// never re-run the ownership claim — see the module doc's "Two guards"
/// section for why that guard is boot-only. `identity_probe` is normally
/// [`PsEngineOwnerProbe`]; tests inject a fake so the ownership guard's "does
/// this pid look like a real engine" check never has to shell out to `ps`
/// for a scripted scenario.
pub async fn run_boot_time_adoption<S>(
    work_db: &WorkDb,
    tmux: &Tmux,
    coordinator: &ExecutionCoordinator,
    spawner: &S,
    convergence: &dyn LiveWorkerConvergence,
    dispatch_events: &dyn DispatchEventSink,
    identity_probe: &dyn EngineOwnerProbe,
) -> TmuxAdoptionOutcome
where
    S: WorkerSpawner + ?Sized,
{
    match claim_or_detect_conflicting_owner(tmux, identity_probe).await {
        Ok(EngineOwnershipOutcome::Claimed) => {}
        Ok(EngineOwnershipOutcome::Conflict { other_pid }) => {
            let this_pid = std::process::id();
            tracing::error!(
                other_pid,
                this_pid,
                "tmux boot adoption: refusing to adopt anything — a different engine process \
                 still holds @boss_engine_owner on the private tmux server and is still alive; \
                 double-adoption would risk two engines controlling the same worker sessions",
            );
            // Not scoped to any one execution — the whole pass is refused
            // before any session is even enumerated — so this dispatch event
            // carries the constant sentinel execution id `"engine-boot"`
            // rather than a real one. That keeps the JsonlFileSink mirror at
            // one stable `executions/engine-boot/` directory shared across
            // every boot that hits this conflict, instead of minting a new
            // per-pid directory (with no matching `work_runs` row) on every
            // occurrence. Without this event at all, an owner conflict would
            // be log-only: nothing in `bossctl agents list` or the dispatch
            // stream would show that adoption was silently disabled for this
            // boot.
            dispatch_events
                .emit(
                    DispatchEvent::new(
                        Stage::TmuxAdoptionOwnerConflict,
                        Outcome::Error,
                        "engine-boot".to_string(),
                    )
                    .with_details(serde_json::json!({
                        "other_pid": other_pid,
                        "this_pid": this_pid,
                    })),
                )
                .await;
            return TmuxAdoptionOutcome {
                owner_conflict: true,
                ..TmuxAdoptionOutcome::default()
            };
        }
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "tmux boot adoption: failed to claim/verify @boss_engine_owner; proceeding with \
                 adoption anyway (best-effort — a set-option/show-options failure here is not \
                 evidence of a real conflict)",
            );
        }
    }

    run_adoption_pass(work_db, tmux, coordinator, spawner, convergence, dispatch_events).await
}

/// One-time drain of sessions still living on the pre-move `-L boss` server.
/// Adopts matching runs against that server for the rest of their life so
/// they are not invisible to `WorkerPool` / stale sweep, and files an
/// attention item naming each surviving session plus the exact
/// `tmux -L boss attach` / `kill-session` command. New sessions are created
/// on the durable socket, not here.
pub async fn drain_legacy_label_server<S>(
    work_db: &WorkDb,
    program: &Path,
    coordinator: &ExecutionCoordinator,
    spawner: &S,
    convergence: &dyn LiveWorkerConvergence,
    dispatch_events: &dyn DispatchEventSink,
    identity_probe: &dyn EngineOwnerProbe,
) -> TmuxAdoptionOutcome
where
    S: WorkerSpawner + ?Sized,
{
    let tmux = match Tmux::for_legacy_label_server(program) {
        Ok(tmux) => tmux,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "tmux boot: could not construct a handle for the pre-move -L boss server; skipping drain"
            );
            return TmuxAdoptionOutcome::default();
        }
    };
    let sessions = match tmux.list_sessions().await {
        Ok(sessions) if sessions.is_empty() => return TmuxAdoptionOutcome::default(),
        Ok(sessions) => sessions,
        Err(err) => {
            tracing::debug!(
                error = %format!("{err:#}"),
                "tmux boot: pre-move -L boss server is absent; no legacy drain needed"
            );
            return TmuxAdoptionOutcome::default();
        }
    };
    tracing::warn!(
        count = sessions.len(),
        sessions = ?sessions.iter().map(|session| session.name.as_str()).collect::<Vec<_>>(),
        "tmux boot: live sessions remain on the pre-move -L boss server; adopting them there and \
         raising attention so they are not redispatched onto the new socket"
    );
    file_legacy_label_server_attention(work_db, &sessions);
    run_boot_time_adoption(
        work_db,
        &tmux,
        coordinator,
        spawner,
        convergence,
        dispatch_events,
        identity_probe,
    )
    .await
}

fn file_legacy_label_server_attention(work_db: &WorkDb, sessions: &[boss_tmux::Session]) {
    for session in sessions {
        let Some(token) = session.spawn_token.as_deref() else {
            continue;
        };
        let Ok(Some(execution_id)) = work_db.execution_id_for_tmux_spawn_token(token) else {
            continue;
        };
        let Ok(execution) = work_db.get_execution(&execution_id) else {
            continue;
        };
        let body = format!(
            "This worker's tmux session `{name}` is still on the pre-move `tmux -L boss` server. \
             The engine adopted it there so it will not be redispatched, but new sessions (and the \
             in-app viewer) use `tmux -S <state-root>/tmux.sock`.\n\n\
             Inspect:\n\n```sh\ntmux -L boss attach -t {name}\n```\n\n\
             When finished, kill it so the next run lands on the durable socket:\n\n\
             ```sh\ntmux -L boss kill-session -t {name}\n```",
            name = session.name,
        );
        if let Err(err) = work_db.upsert_work_item_attention(
            &execution.work_item_id,
            TMUX_LEGACY_LABEL_SERVER_ATTENTION_KIND,
            "Worker still running on the pre-move tmux server",
            &body,
        ) {
            tracing::warn!(
                execution_id,
                ?err,
                "tmux boot: failed to file legacy-label-server attention (non-fatal)"
            );
        }
    }
}

/// Run one tmux-server adoption pass. Startup and the periodic leaked-session
/// sweep share this exact inventory and classification so a session cannot be
/// called a leak merely because derived engine state has not been rebuilt yet.
#[allow(clippy::too_many_arguments)]
pub async fn run_adoption_pass<S>(
    work_db: &WorkDb,
    tmux: &Tmux,
    coordinator: &ExecutionCoordinator,
    spawner: &S,
    convergence: &dyn LiveWorkerConvergence,
    dispatch_events: &dyn DispatchEventSink,
) -> TmuxAdoptionOutcome
where
    S: WorkerSpawner + ?Sized,
{
    let mut outcome = TmuxAdoptionOutcome::default();

    let sessions = match tmux.list_sessions().await {
        Ok(sessions) => sessions,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "tmux session sweep: list-sessions failed; skipping the pass (best-effort)",
            );
            return outcome;
        }
    };
    if sessions.is_empty() {
        return outcome;
    }

    // The authoritative token per session — never the `@boss_spawn_token`
    // option mirror `list_sessions` also returns. See the module doc.
    //
    // The schema guard is read here too, once per live session, rather than
    // only for sessions that match an adoptable row: a live token that
    // matches no adoptable row can still resolve to a *terminal* execution
    // below, and that branch must be guarded exactly the same way — a
    // version-skewed session is equally unsafe to hand to
    // `LiveWorkerConvergence::converge_live_worker` as it is to `adopt_one`.
    // `None` means the schema itself could not be read (a `show-environment`
    // failure distinct from "unset"); both branches below leave that session
    // alone for a later sweep rather than guessing.
    let mut live_sessions = Vec::new();
    let mut session_schemas: HashMap<String, Option<Result<(), SchemaGuardFailure>>> = HashMap::new();
    for session in &sessions {
        // The coordinator shares the private server but is represented by a
        // metadata singleton rather than a worker run. Its lifecycle is
        // reconciled by `coordinator_tmux`; never feed its token into the
        // worker leaked-session path.
        if session.name == crate::coordinator_tmux::COORDINATOR_SESSION_NAME {
            continue;
        }
        match session_spawn_token(tmux, &session.name).await {
            Ok(Some(token)) => {
                let schema_check = match tmux.show_environment(&session.name, TMUX_SESSION_SCHEMA_ENV).await {
                    Ok(value) => Some(check_session_schema(value.as_deref())),
                    Err(err) => {
                        tracing::warn!(
                            session = %session.name,
                            error = %format!("{err:#}"),
                            "tmux session sweep: could not read BOSS_SESSION_SCHEMA; leaving this \
                             session for a later sweep to resolve",
                        );
                        None
                    }
                };
                session_schemas.insert(session.name.clone(), schema_check);
                live_sessions.push(UntrackedTmuxSession {
                    session_name: session.name.clone(),
                    spawn_token: token,
                });
            }
            Ok(None) => {
                // Not a Boss worker session (or predates this env contract)
                // — nothing to adopt.
            }
            Err(err) => {
                tracing::warn!(
                    session = %session.name,
                    error = %format!("{err:#}"),
                    "tmux session sweep: show-environment failed for a session; skipping it",
                );
            }
        }
    }
    if live_sessions.is_empty() {
        return outcome;
    }

    let adoptable = match work_db.list_adoptable_tmux_runs() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(
                error = %format!("{err:#}"),
                "tmux session sweep: failed to list adoptable runs; skipping the pass",
            );
            return outcome;
        }
    };

    let mut claimed_tokens: HashSet<String> = HashSet::new();
    for handle in &adoptable {
        let Some(session) = live_sessions
            .iter()
            .find(|session| session.spawn_token == handle.tmux_spawn_token)
        else {
            continue;
        };
        claimed_tokens.insert(handle.tmux_spawn_token.clone());

        let Some(schema_check) = session_schemas.get(&session.session_name).cloned() else {
            // Unreachable in practice: every entry in `live_sessions` got a
            // matching `session_schemas` entry in the same loop iteration
            // above. Treated the same as an unreadable schema: leave it.
            continue;
        };
        let Some(schema_check) = schema_check else {
            // `show-environment` for the schema itself failed earlier;
            // already logged there. Leave this run for a later sweep.
            continue;
        };
        if let Err(failure) = schema_check {
            tracing::error!(
                execution_id = handle.execution_id.as_str(),
                session = session.session_name.as_str(),
                failure = %failure.describe(),
                "tmux session sweep: refusing to adopt a session with an unsupported \
                 BOSS_SESSION_SCHEMA; reaping it",
            );
            refuse_and_reap(
                work_db,
                tmux,
                dispatch_events,
                &handle.execution_id,
                (session.session_name.as_str(), handle.tmux_spawn_token.as_str()),
                &failure,
                RefusedRowKind::NonTerminal,
            )
            .await;
            outcome.refused_schema_skew += 1;
            continue;
        }

        adopt_one(
            work_db,
            tmux,
            coordinator,
            spawner,
            dispatch_events,
            handle,
            &session.session_name,
            &mut outcome,
        )
        .await;
    }

    for session in live_sessions {
        if claimed_tokens.contains(&session.spawn_token) {
            continue;
        }
        let schema_check = session_schemas.get(&session.session_name).cloned().flatten();
        classify_untracked_session(
            work_db,
            tmux,
            coordinator,
            spawner,
            convergence,
            dispatch_events,
            session,
            schema_check,
            &mut outcome,
        )
        .await;
    }

    outcome
}

/// Rebuild derived bookkeeping for one confirmed-live, non-terminal match.
#[allow(clippy::too_many_arguments)]
async fn adopt_one<S>(
    work_db: &WorkDb,
    tmux: &Tmux,
    coordinator: &ExecutionCoordinator,
    spawner: &S,
    dispatch_events: &dyn DispatchEventSink,
    handle: &TmuxRunHandle,
    session_name: &str,
    outcome: &mut TmuxAdoptionOutcome,
) where
    S: WorkerSpawner + ?Sized,
{
    let execution_id = handle.execution_id.as_str();

    let Some(slot_id) = slot_id_from_worker_id(&handle.agent_id) else {
        tracing::error!(
            execution_id,
            agent_id = %handle.agent_id,
            "tmux boot adoption: adoptable run's agent_id does not parse as a pool worker id; skipping",
        );
        return;
    };

    // A fresh read, not the possibly-never-recorded `handle.tmux_pane_pid`:
    // this is both the adoption's proof of the current pid and, for an
    // `intended` row, the value the repair write below needs.
    let shell_pid = match tmux.display_message(session_name, DisplayField::PanePid).await {
        Ok(raw) => raw.trim().parse::<i32>().ok().filter(|pid| *pid > 0),
        Err(err) => {
            tracing::warn!(
                execution_id,
                session = session_name,
                error = %format!("{err:#}"),
                "tmux boot adoption: could not read the session's pane pid",
            );
            None
        }
    };
    let Some(shell_pid) = shell_pid else {
        tracing::warn!(
            execution_id,
            session = session_name,
            "tmux boot adoption: session matched but its pane pid could not be confirmed; \
             leaving this run for a later sweep to resolve",
        );
        return;
    };

    let repaired_intent = handle.tmux_spawn_state == "intended";
    if repaired_intent {
        match work_db.record_tmux_session_created_for_execution(
            execution_id,
            &handle.tmux_spawn_token,
            i64::from(shell_pid),
        ) {
            Ok(true) => outcome.repaired_intents += 1,
            Ok(false) => tracing::warn!(
                execution_id,
                "tmux boot adoption: intended session's confirmation write matched no run row",
            ),
            Err(err) => tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "tmux boot adoption: failed to repair the intended-to-created tmux spawn record",
            ),
        }
    }

    spawner
        .worker_registry()
        .register_adopted_tmux_run_slot(execution_id.to_owned(), slot_id, session_name);
    spawner.worker_registry().register(shell_pid, execution_id.to_owned());

    if !coordinator.reclaim_slot(&handle.agent_id, execution_id).await {
        tracing::warn!(
            execution_id,
            agent_id = %handle.agent_id,
            slot_id,
            "tmux boot adoption: pool slot could not be re-claimed (occupied by another execution); \
             the row's own status is the only re-dispatch protection until this is resolved",
        );
    }

    let execution = match work_db.get_execution(execution_id) {
        Ok(execution) => execution,
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "tmux boot adoption: could not load the execution row after registering its slot",
            );
            return;
        }
    };

    let driver = crate::driver_transcript::driver_for_execution(work_db, execution_id).or_else(|| {
        crate::driver::DriverRegistry::default()
            .require(crate::effort::ENGINE_DEFAULT_DRIVER)
            .ok()
    });

    if let Some(live_states) = spawner.live_worker_state_registry() {
        let binding = work_db
            .get_work_item(&execution.work_item_id)
            .ok()
            .map(|item| boss_protocol::WorkItemBinding {
                work_item_id: execution.work_item_id.clone(),
                work_item_name: crate::runner::work_item_name(&item).to_owned(),
                execution_id: execution_id.to_owned(),
            });
        let has_source_automation = matches!(
            work_db.source_automation_id_for_work_item(&execution.work_item_id),
            Ok(Some(_))
        );
        let pool = attributed_pool_label(execution.kind.clone(), has_source_automation);
        let model_label = driver
            .as_ref()
            .map(|driver| driver.descriptor().label.to_owned())
            .unwrap_or_else(|| crate::effort::ENGINE_DEFAULT_DRIVER.to_owned());
        let awaiting_input_capable = driver.as_ref().is_some_and(|driver| {
            driver
                .capabilities()
                .provides(crate::driver::Capability::AwaitingInputSignal)
        });
        live_states.register_readoption(
            slot_id,
            execution_id.to_owned(),
            model_label,
            shell_pid,
            binding,
            awaiting_input_capable,
            LiveSpawnRouting::new(pool, execution.kind.as_str()),
            ReadoptionEvidence::LiveShellPid,
        );
        spawner.publish_live_worker_states().await;
        if let Some(driver) = driver {
            spawner.start_live_status_slot(slot_id, execution_id, driver);
        }
    }

    outcome.adopted_execution_ids.insert(execution_id.to_owned());

    dispatch_events
        .emit(
            DispatchEvent::new(Stage::TmuxWorkerAdopted, Outcome::Ok, execution_id)
                .with_work_item(&execution.work_item_id)
                .with_details(serde_json::json!({
                    "slot_id": slot_id,
                    "shell_pid": shell_pid,
                    "tmux_session_name": session_name,
                    "repaired_intent": repaired_intent,
                })),
        )
        .await;
}

/// Classify a session that missed the normal non-terminal adoption set.
///
/// A missing DB row is a leak. A terminal row is a contradiction for
/// `worker_readoption` — but only once `schema_check` clears the same guard
/// [`adopt_one`]'s callers apply, exactly as on the direct-match branch: a
/// version-skewed session is exactly as unsafe to re-adopt via
/// [`LiveWorkerConvergence::converge_live_worker`] as it is to hand to
/// `adopt_one` directly, so a schema failure here is refused and reaped the
/// same way. A non-terminal row missed by the broad query is retried through
/// the same adoption routine rather than being reaped. `schema_check` of
/// `None` means the schema itself could not be read; that session is left
/// alone for a later sweep, same as an unknown token.
#[allow(clippy::too_many_arguments)]
async fn classify_untracked_session<S>(
    work_db: &WorkDb,
    tmux: &Tmux,
    coordinator: &ExecutionCoordinator,
    spawner: &S,
    convergence: &dyn LiveWorkerConvergence,
    dispatch_events: &dyn DispatchEventSink,
    session: UntrackedTmuxSession,
    schema_check: Option<Result<(), SchemaGuardFailure>>,
    outcome: &mut TmuxAdoptionOutcome,
) where
    S: WorkerSpawner + ?Sized,
{
    let execution_id = match work_db.execution_id_for_tmux_spawn_token(&session.spawn_token) {
        Ok(Some(id)) => id,
        Ok(None) => {
            outcome.untracked_sessions.push(session);
            return;
        }
        Err(err) => {
            tracing::warn!(
                session = %session.session_name,
                error = %format!("{err:#}"),
                "tmux session sweep: failed to resolve a live session's spawn token; skipping",
            );
            return;
        }
    };
    let execution = match work_db.get_execution(&execution_id) {
        Ok(execution) => execution,
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "tmux session sweep: failed to load the execution behind a live session's spawn token",
            );
            return;
        }
    };
    if execution.status.is_terminal() {
        let Some(schema_check) = schema_check else {
            // The schema itself could not be read; already logged where it
            // was read. Leave this session for a later sweep rather than
            // guessing.
            return;
        };
        if let Err(failure) = schema_check {
            tracing::error!(
                execution_id,
                session = %session.session_name,
                failure = %failure.describe(),
                "tmux session sweep: refusing to hand off a terminal-execution session with an \
                 unsupported BOSS_SESSION_SCHEMA; reaping it instead of re-adopting",
            );
            refuse_and_reap(
                work_db,
                tmux,
                dispatch_events,
                &execution_id,
                (session.session_name.as_str(), session.spawn_token.as_str()),
                &failure,
                RefusedRowKind::Terminal,
            )
            .await;
            outcome.refused_schema_skew += 1;
            return;
        }
        convergence
            .converge_live_worker(&execution_id, TERMINAL_HANDOFF_TRIGGER)
            .await;
        outcome.terminal_handoffs += 1;
        return;
    }

    match work_db.tmux_run_handle_for_spawn_token(&session.spawn_token) {
        Ok(Some(handle)) => {
            tracing::warn!(
                execution_id,
                session = %session.session_name,
                "tmux session sweep: non-terminal session missed normal adoption; retrying adoption",
            );
            adopt_one(
                work_db,
                tmux,
                coordinator,
                spawner,
                dispatch_events,
                &handle,
                &session.session_name,
                outcome,
            )
            .await;
        }
        Ok(None) => tracing::warn!(
            execution_id,
            session = %session.session_name,
            "tmux session sweep: non-terminal execution has no complete tmux run handle; skipping",
        ),
        Err(err) => tracing::warn!(
            execution_id,
            session = %session.session_name,
            error = %format!("{err:#}"),
            "tmux session sweep: failed to load non-terminal tmux run handle; skipping",
        ),
    }
}

/// What [`claim_or_detect_conflicting_owner`] decided.
enum EngineOwnershipOutcome {
    /// No conflicting live owner was found; this process's pid is now
    /// stamped on [`ENGINE_OWNER_OPTION`].
    Claimed,
    /// A different engine process's pid is recorded and still alive.
    Conflict { other_pid: i32 },
}

/// Best-effort check for whether a live pid is plausibly running a genuine
/// boss engine binary, used by [`claim_or_detect_conflicting_owner`] to tell
/// a real ownership conflict apart from a stale stamp whose pid has since
/// been recycled by an unrelated process. `None` means indeterminate (the
/// concrete impl couldn't resolve one side or the other) — every caller
/// treats that the same as "yes, could be a conflict", matching this
/// module's best-effort posture everywhere else: an inconclusive check must
/// never be the thing that silently disables the false-positive fix.
#[async_trait::async_trait]
pub trait EngineOwnerProbe: Send + Sync {
    async fn pid_looks_like_this_engine(&self, pid: i32) -> Option<bool>;
}

/// Production [`EngineOwnerProbe`]: compares the live pid's own executable
/// against this process's, via `ps -o comm=` — `comm` reports the full
/// executable path on macOS (unlike Linux's truncated 16-byte `comm`), which
/// is what makes a basename comparison meaningful here. Nothing else in the
/// engine pulls in a procfs/sysctl dependency for this and `ps` is reliably
/// available on macOS (see `main.rs`'s `parent_command_line`, the existing
/// precedent for this exact pattern).
pub struct PsEngineOwnerProbe;

#[async_trait::async_trait]
impl EngineOwnerProbe for PsEngineOwnerProbe {
    async fn pid_looks_like_this_engine(&self, pid: i32) -> Option<bool> {
        let own = own_exe_basename()?;
        let live = live_pid_exe_basename(pid).await?;
        Some(own == live)
    }
}

fn own_exe_basename() -> Option<String> {
    std::env::current_exe()
        .ok()?
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

async fn live_pid_exe_basename(pid: i32) -> Option<String> {
    let output = tokio::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if line.is_empty() {
        return None;
    }
    Path::new(&line)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Claim server-scoped tmux ownership for this engine process, or detect
/// that a different, still-live engine already holds it.
///
/// The recorded value is the bare pid. A pid alone is not durable evidence
/// of identity — if the engine that wrote it crashed, the OS can reassign
/// the same pid to an unrelated process, and `kill(pid, 0)` cannot tell the
/// two apart. [`EngineOwnerProbe`] is the mitigation: a live pid is only
/// treated as a genuine conflict when its own executable also looks like
/// this engine's binary. An indeterminate probe
/// result (e.g. this process's own exe path is unavailable) falls back to
/// the conservative "treat as conflict" default, same as every other
/// best-effort check in this module. Adequate for the single-user-desktop
/// threat model this guard is explicitly scoped to (see the design doc's
/// risk section): it is not a distributed lock, only a loud refusal when two
/// engine processes are plainly both alive at once.
///
/// A mechanical failure to read the option (e.g. the private tmux server has
/// never been started) is not evidence of a conflict — it is reported as
/// `Err` and the caller proceeds with adoption anyway, matching this
/// module's best-effort posture everywhere else.
async fn claim_or_detect_conflicting_owner(
    tmux: &Tmux,
    identity_probe: &dyn EngineOwnerProbe,
) -> anyhow::Result<EngineOwnershipOutcome> {
    let current_pid = std::process::id();
    let existing = match tmux.show_server_option(ENGINE_OWNER_OPTION).await {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "tmux boot adoption: could not read @boss_engine_owner (treating as unset)",
            );
            None
        }
    };
    if let Some(raw) = &existing {
        // Parse defensively: take the leading `:`-delimited segment so a
        // hand-set or otherwise malformed option value degrades to
        // "unparseable, treat as unset" rather than being mis-read.
        let other_pid = raw
            .split(':')
            .next()
            .and_then(|pid_part| pid_part.trim().parse::<i32>().ok())
            .filter(|pid| *pid != current_pid as i32);
        if let Some(other_pid) = other_pid
            && matches!(probe_pid(other_pid), PidStatus::Alive | PidStatus::PermissionDenied)
        {
            // `EPERM` means alive, per the liveness contract: a process we
            // cannot signal due to permissions unambiguously still exists.
            match identity_probe.pid_looks_like_this_engine(other_pid).await {
                Some(false) => {
                    tracing::warn!(
                        other_pid,
                        "tmux boot adoption: @boss_engine_owner's pid is alive but its executable \
                         does not look like this engine's binary; treating the stamp as stale \
                         (pid reuse after a crash) rather than a real conflict, and reclaiming \
                         ownership",
                    );
                }
                Some(true) | None => {
                    return Ok(EngineOwnershipOutcome::Conflict { other_pid });
                }
            }
        }
    }
    tmux.set_server_option(ENGINE_OWNER_OPTION, &current_pid.to_string())
        .await?;
    Ok(EngineOwnershipOutcome::Claimed)
}

/// Why a live session's `BOSS_SESSION_SCHEMA` failed the adoption guard.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SchemaGuardFailure {
    /// No `BOSS_SESSION_SCHEMA` in the session environment at all — predates
    /// the contract, or was written by a build that never set it.
    Missing,
    /// Present but not a value this engine can parse as a schema number.
    Unparseable(String),
    /// A schema number newer than [`TMUX_SESSION_SCHEMA`].
    TooNew(u32),
}

impl SchemaGuardFailure {
    fn describe(&self) -> String {
        match self {
            Self::Missing => "the session carries no BOSS_SESSION_SCHEMA at all".to_owned(),
            Self::Unparseable(raw) => {
                format!("BOSS_SESSION_SCHEMA={raw:?} is not a number this engine can parse")
            }
            Self::TooNew(schema) => format!(
                "BOSS_SESSION_SCHEMA={schema} is newer than this engine's contract (currently {TMUX_SESSION_SCHEMA})"
            ),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unparseable(_) => "unparseable",
            Self::TooNew(_) => "too_new",
        }
    }

    fn details(&self) -> serde_json::Value {
        let mut details = serde_json::json!({ "schema_guard_failure": self.kind() });
        let serde_json::Value::Object(map) = &mut details else {
            unreachable!("details is always constructed as an object")
        };
        match self {
            Self::Missing => {}
            Self::Unparseable(raw) => {
                map.insert("session_schema".to_owned(), serde_json::json!(raw));
            }
            Self::TooNew(schema) => {
                map.insert("session_schema".to_owned(), serde_json::json!(schema));
                map.insert("supported_schema".to_owned(), serde_json::json!(TMUX_SESSION_SCHEMA));
            }
        }
        details
    }
}

/// Checks a live session's `BOSS_SESSION_SCHEMA` (already read via
/// `show-environment`, never the absent-by-default option mirror) against
/// what this engine understands. `None` means the variable was unset.
///
/// Only "unknown" (missing/unparseable) or "newer than this engine's own
/// contract" fail — a schema this engine has never seen cannot be assumed
/// compatible, but nothing yet says an older, still-recognized schema is
/// incompatible, so that case is left open rather than guessed at.
fn check_session_schema(raw: Option<&str>) -> Result<(), SchemaGuardFailure> {
    let raw = raw.ok_or(SchemaGuardFailure::Missing)?;
    let schema: u32 = raw
        .trim()
        .parse()
        .map_err(|_| SchemaGuardFailure::Unparseable(raw.to_owned()))?;
    let current: u32 = TMUX_SESSION_SCHEMA
        .parse()
        .expect("TMUX_SESSION_SCHEMA must always parse as u32");
    if schema > current {
        return Err(SchemaGuardFailure::TooNew(schema));
    }
    Ok(())
}

/// Which of the two call sites [`refuse_and_reap`] is reaping a session for
/// — the attention body's account of what happens next differs between
/// them, so the caller must say which one it is rather than the shared body
/// guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefusedRowKind {
    /// The `adopt_one` branch: a live, non-terminal `work_runs` row. Once
    /// the kill succeeds it makes the row's recorded `shell_pid` genuinely
    /// dead, so the normal dead-worker reconcilers pick it up and
    /// redispatch exactly as they would for any other vanished session.
    NonTerminal,
    /// The [`hand_off_if_terminal`] branch: the row is already terminal, so
    /// there is no live row for a dead-worker reconciler to redispatch —
    /// the session is simply killed and the row is left in its terminal
    /// state.
    Terminal,
}

/// Kill a live session this engine refuses to adopt, and make the refusal
/// loud: a dispatch event on the execution it belonged to, plus an
/// attention item on the work item so an operator sees it without reading
/// engine logs. The execution row itself is always left untouched — what
/// happens to it next depends on `row_kind` (see [`RefusedRowKind`]).
///
/// Teardown goes through [`Tmux::kill_session_verified`] with the durably
/// recorded (and live-matched) spawn token — never a bare name kill — so a
/// recycled session name cannot destroy a different execution's worker.
///
/// The refuse-then-reap safety argument only holds when the kill actually
/// happened — a session left alive risks a second worker landing in the
/// same cube workspace once the work is redispatched. So a failed
/// `kill-session` is reported as such, not folded into the same "refused
/// and reaped" success shape: the dispatch event carries [`Outcome::Error`]
/// and a `"reaped": false` detail, and the attention body tells the
/// operator the session may still be running and names the manual command
/// to clear it. An already-absent session is treated as reaped (the
/// session is not live either way).
async fn refuse_and_reap(
    work_db: &WorkDb,
    tmux: &Tmux,
    dispatch_events: &dyn DispatchEventSink,
    execution_id: &str,
    // Live session name + the spawn token that matched it — the only
    // identity pair `Tmux::kill_session_verified` will accept.
    session: (&str, &str),
    failure: &SchemaGuardFailure,
    row_kind: RefusedRowKind,
) {
    let (session_name, spawn_token) = session;
    let reaped = match tmux.kill_session_verified(session_name, spawn_token).await {
        Ok(boss_tmux::KillSessionOutcome::Killed | boss_tmux::KillSessionOutcome::Absent) => true,
        Err(err) => {
            tracing::error!(
                execution_id,
                session = session_name,
                error = %format!("{err:#}"),
                "tmux boot adoption: failed to reap a refused session; it may still be live",
            );
            false
        }
    };

    let execution = match work_db.get_execution(execution_id) {
        Ok(execution) => execution,
        Err(err) => {
            tracing::error!(
                execution_id,
                error = %format!("{err:#}"),
                "tmux boot adoption: refused a session but could not load its execution to file \
                 the attention item",
            );
            return;
        }
    };

    let mut details = failure.details();
    if let serde_json::Value::Object(map) = &mut details {
        map.insert("session_name".to_owned(), serde_json::json!(session_name));
        map.insert("reason".to_owned(), serde_json::json!(failure.describe()));
        map.insert("reaped".to_owned(), serde_json::json!(reaped));
    }
    dispatch_events
        .emit(
            DispatchEvent::new(
                Stage::TmuxAdoptionRefused,
                if reaped { Outcome::Ok } else { Outcome::Error },
                execution_id,
            )
            .with_work_item(&execution.work_item_id)
            .with_details(details),
        )
        .await;

    let title = "Tmux worker session refused on adoption (version skew)".to_owned();
    let body = if reaped {
        let next_steps = match row_kind {
            RefusedRowKind::NonTerminal => {
                format!(
                    "Execution `{execution_id}` was left for the normal dead-worker reconcilers, \
                     which will redispatch the work. No PR or code changes are lost — only the \
                     in-progress turn."
                )
            }
            RefusedRowKind::Terminal => {
                format!(
                    "Execution `{execution_id}` was already terminal, so it is left as-is — there is \
                     no in-progress work for a reconciler to redispatch here. No PR or code changes \
                     are lost; this item exists only because a stale session outlived the row it \
                     belonged to."
                )
            }
        };
        format!(
            "The engine found this execution's tmux session alive on restart but refused to adopt \
             it and reaped it instead: {}.\n\n\
             This is expected after an engine upgrade that changed the tmux session contract — the \
             session was written by a build this engine no longer trusts to attach to safely, so it \
             was killed rather than left running unattended (leaving it alive would risk a second \
             worker landing in the same cube workspace once the work is redispatched).\n\n\
             {next_steps}\n\n\
             This item is informational; dismiss it once you've confirmed the chore resumed.",
            failure.describe(),
        )
    } else {
        let after_kill = match row_kind {
            RefusedRowKind::NonTerminal => {
                format!(
                    "Then let the normal dead-worker reconcilers redispatch execution \
                     `{execution_id}` — until it is killed, a redispatch risks a second worker \
                     landing in the same cube workspace."
                )
            }
            RefusedRowKind::Terminal => {
                format!(
                    "Execution `{execution_id}` was already terminal, so there is nothing to \
                     redispatch — killing the session is the whole remedy."
                )
            }
        };
        format!(
            "The engine found this execution's tmux session alive on restart and refused to adopt \
             it: {}.\n\n\
             This is expected after an engine upgrade that changed the tmux session contract — the \
             session was written by a build this engine no longer trusts to attach to safely. The \
             engine's attempt to kill the session ALSO FAILED, so it may still be running \
             unattended in tmux session `{session_name}`. Run `{} kill-session -t \
             {session_name}` manually to reap it. {after_kill}\n\n\
             This item is informational; dismiss it once you've manually killed the session and \
             confirmed the chore resumed.",
            failure.describe(),
            tmux.operator_prefix(),
        )
    };
    if let Err(err) = work_db.upsert_work_item_attention(
        &execution.work_item_id,
        TMUX_ADOPTION_SCHEMA_SKEW_ATTENTION_KIND,
        &title,
        &body,
    ) {
        tracing::warn!(
            execution_id,
            ?err,
            "tmux boot adoption: failed to file schema-skew attention item (non-fatal)",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Arc, Mutex as StdMutex};

    use boss_protocol::RequestExecutionInput;
    use boss_tmux::{CommandOutput, CommandRunner};
    use tokio::time::Duration;

    use crate::app::SendToAppError;
    use crate::coordinator::{ExecutionCoordinator, WorkerPool};
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::driver::AgentDriver;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::protocol::{EngineToAppRequest, EngineToAppResponse};
    use crate::test_support::{NoopCube, NoopRunner, create_test_chore_manual, create_test_product_named, open_db_arc};
    use crate::worker_readoption::{LiveWorkerConvergence, NoopLiveWorkerConvergence};
    use crate::worker_registry::WorkerRegistry;

    /// A fake `boss` tmux server: scripted answers for `list-sessions`,
    /// `show-environment`, and `display-message #{pane_pid}`, keyed by
    /// session name, plus a mutable server-option store and a kill log for
    /// `set-option -s` / `show-options -s` / `kill-session`. Panics on any
    /// other command shape — this pass never calls `new-session` or a
    /// session-scoped `set-option`/`show-options`.
    #[derive(Default)]
    struct FakeTmuxServer {
        /// Session names `list-sessions` should report.
        sessions: Vec<String>,
        /// `show-environment BOSS_SPAWN_TOKEN` answer per session. Absent
        /// entries answer "unknown variable" (unset), matching a session
        /// with no Boss identity.
        tokens: HashMap<String, String>,
        /// `show-environment BOSS_SESSION_SCHEMA` answer per session. Absent
        /// entries answer "unknown variable" (unset).
        schemas: HashMap<String, String>,
        /// `display-message #{pane_pid}` answer per session.
        pane_pids: HashMap<String, String>,
        /// Server-scoped option store, seeded before the call and mutated by
        /// `set-option -s` during it. `None` means the option starts unset.
        server_options: StdMutex<HashMap<String, String>>,
        /// Session names `kill-session -t <name>` was invoked for, in order.
        killed_sessions: StdMutex<Vec<String>>,
        /// Session names for which `kill-session -t <name>` should report
        /// failure (a non-success [`CommandOutput`]) instead of the default
        /// success — scripts the "reap failed" half of [`refuse_and_reap`].
        /// The name is still recorded into `killed_sessions` (the kill was
        /// attempted, it just didn't succeed).
        kill_session_failures: HashSet<String>,
    }

    impl FakeTmuxServer {
        fn with_engine_owner(mut self, value: impl Into<String>) -> Self {
            self.server_options
                .get_mut()
                .unwrap()
                .insert(ENGINE_OWNER_OPTION.trim_start_matches('@').to_owned(), value.into());
            self
        }
    }

    fn ok_output(stdout: String) -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            stdout,
            stderr: String::new(),
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for FakeTmuxServer {
        async fn run(&self, _program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
            assert!(cwd.is_none());
            let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
            match args.get(2).map(String::as_str) {
                Some("list-sessions") => {
                    let stdout = self
                        .sessions
                        .iter()
                        .map(|name| format!("{name}\t\n"))
                        .collect::<String>();
                    Ok(ok_output(stdout))
                }
                Some("show-environment") => {
                    assert_eq!(args[3], "-t");
                    let session = &args[4];
                    let var = args[5].as_str();
                    let table = if var == TMUX_SPAWN_TOKEN_ENV {
                        &self.tokens
                    } else if var == TMUX_SESSION_SCHEMA_ENV {
                        &self.schemas
                    } else {
                        panic!("unexpected show-environment variable in test: {var:?}");
                    };
                    match table.get(session) {
                        Some(value) => Ok(ok_output(format!("{var}={value}\n"))),
                        None => Ok(CommandOutput {
                            success: false,
                            code: Some(1),
                            stdout: String::new(),
                            stderr: format!("unknown variable: {var}"),
                        }),
                    }
                }
                Some("display-message") => {
                    assert_eq!(args[4], "-t");
                    let session = &args[5];
                    let pid = self.pane_pids.get(session).cloned().unwrap_or_else(|| "0".to_owned());
                    Ok(ok_output(format!("{pid}\n")))
                }
                Some("show-options") => {
                    assert_eq!(args[3], "-s", "tmux boot adoption never reads a session-scoped option");
                    assert_eq!(args[4], "-v");
                    let option = args[5].trim_start_matches('@');
                    match self.server_options.lock().unwrap().get(option) {
                        Some(value) => Ok(ok_output(format!("{value}\n"))),
                        None => Ok(CommandOutput {
                            success: false,
                            code: Some(1),
                            stdout: String::new(),
                            stderr: format!("invalid option: {option}"),
                        }),
                    }
                }
                Some("set-option") => {
                    assert_eq!(args[3], "-s", "tmux boot adoption never sets a session-scoped option");
                    let option = args[4].trim_start_matches('@').to_owned();
                    let value = args[5].clone();
                    self.server_options.lock().unwrap().insert(option, value);
                    Ok(ok_output(String::new()))
                }
                Some("kill-session") => {
                    assert_eq!(args[3], "-t");
                    let session = args[4].clone();
                    self.killed_sessions.lock().unwrap().push(session.clone());
                    if self.kill_session_failures.contains(&session) {
                        // Must NOT look like an absent-session error: those
                        // are treated as idempotent success by
                        // `kill_session_verified`. A real post-match kill
                        // failure has to surface as Err so refuse_and_reap
                        // reports reaped=false.
                        Ok(CommandOutput {
                            success: false,
                            code: Some(1),
                            stdout: String::new(),
                            stderr: format!("error connecting to /tmp/tmux-0/default ({session})"),
                        })
                    } else {
                        Ok(ok_output(String::new()))
                    }
                }
                other => panic!("unexpected tmux command in test: {other:?} (full args={args:?})"),
            }
        }
    }

    fn fake_tmux(server: FakeTmuxServer) -> (Tmux, Arc<FakeTmuxServer>) {
        let server = Arc::new(server);
        (
            Tmux::with_runner_and_socket(
                "/opt/homebrew/bin/tmux",
                Arc::clone(&server) as Arc<dyn CommandRunner>,
                boss_tmux::TEST_SOCKET_PATH,
            )
            .unwrap(),
            server,
        )
    }

    /// A session env map seeded with the current, supported
    /// `BOSS_SESSION_SCHEMA` — the shape every adoption-success test wants,
    /// so the schema guard added alongside the token/pid checks never
    /// changes what those tests are asserting about.
    fn supported_schema(session: &str) -> HashMap<String, String> {
        HashMap::from([(session.to_owned(), TMUX_SESSION_SCHEMA.to_owned())])
    }

    /// Request-and-start a local, `worker-N`-attributed execution: the exact
    /// shape [`WorkDb::list_adoptable_tmux_runs`] requires before a tmux
    /// identity is stamped on it.
    fn start_local_run(db: &WorkDb, agent_id: &str) -> String {
        let product = create_test_product_named(db, "p");
        let chore = create_test_chore_manual(db, product.id.clone(), "c");
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();
        db.start_execution_run_on_host(&execution.id, agent_id, "mono", "lease-1", "ws-1", "/tmp/ws-1", "local")
            .unwrap();
        execution.id
    }

    /// [`WorkerSpawner`] test double: real registries so assertions can read
    /// back exactly what the pass registered, plus call logs for the two
    /// hooks that have no other observable side effect
    /// (`start_live_status_slot`, `publish_live_worker_states`).
    #[derive(Default)]
    struct RecordingSpawner {
        registry: WorkerRegistry,
        live_states: LiveWorkerStateRegistry,
        live_status_calls: StdMutex<Vec<(u8, String)>>,
        publish_calls: StdMutex<usize>,
    }

    #[async_trait::async_trait]
    impl WorkerSpawner for RecordingSpawner {
        async fn send_to_app_request(
            &self,
            _request: EngineToAppRequest,
            _timeout: Duration,
        ) -> Result<EngineToAppResponse, SendToAppError> {
            panic!("tmux boot adoption must never call the app RPC");
        }

        fn worker_registry(&self) -> &WorkerRegistry {
            &self.registry
        }

        fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
            Some(&self.live_states)
        }

        async fn publish_live_worker_states(&self) {
            *self.publish_calls.lock().unwrap() += 1;
        }

        fn start_live_status_slot(&self, slot_id: u8, run_id: &str, _driver: Arc<dyn AgentDriver>) {
            self.live_status_calls
                .lock()
                .unwrap()
                .push((slot_id, run_id.to_owned()));
        }
    }

    #[derive(Default)]
    struct RecordingConvergence {
        calls: StdMutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl LiveWorkerConvergence for RecordingConvergence {
        async fn converge_live_worker(&self, execution_id: &str, trigger: &str) {
            self.calls
                .lock()
                .unwrap()
                .push((execution_id.to_owned(), trigger.to_owned()));
        }
    }

    /// [`EngineOwnerProbe`] test double returning a fixed, scripted answer —
    /// ownership tests never need to shell out to a real `ps` to exercise a
    /// scenario. `Some(true)` is the safe default for every test that
    /// doesn't care (most never even reach the probe, since it's only
    /// consulted once a *different* pid is found alive on the option).
    struct FixedEngineOwnerProbe(Option<bool>);

    #[async_trait::async_trait]
    impl EngineOwnerProbe for FixedEngineOwnerProbe {
        async fn pid_looks_like_this_engine(&self, _pid: i32) -> Option<bool> {
            self.0
        }
    }

    /// Spawns `true`, waits for it to exit, and returns its released pid —
    /// guaranteed dead. Mirrors `dead_pid_sweep`'s identical helper; there is
    /// a narrow race where the OS could recycle the pid before the caller's
    /// `kill(0)` probe, but in practice this does not occur in test
    /// environments.
    fn dead_pid() -> i32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let _ = child.wait();
        pid
    }

    fn coordinator_with_one_slot(db: Arc<WorkDb>) -> ExecutionCoordinator {
        ExecutionCoordinator::new(db, WorkerPool::new(1), Arc::new(NoopCube), Arc::new(NoopRunner))
    }

    #[tokio::test]
    async fn no_live_sessions_is_a_cheap_noop() {
        let (_dir, db) = open_db_arc();
        let (tmux, _tmux_server) = fake_tmux(FakeTmuxServer::default());
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(outcome.repaired_intents, 0);
        assert_eq!(outcome.terminal_handoffs, 0);
        assert!(sink.events().await.is_empty());
    }

    /// The cardinal case: engine restarts, the worker's tmux session (and its
    /// non-terminal execution row) survived. The pass must rebuild the slot
    /// claim, the `WorkerRegistry` pid/slot map, the `LiveWorkerState` entry,
    /// and start the live-status summarizer — using a freshly read pane pid,
    /// not the one `record_tmux_session_created` recorded at original spawn
    /// time.
    #[tokio::test]
    async fn non_terminal_match_rebuilds_derived_state() {
        let (_dir, db) = open_db_arc();
        let execution_id = start_local_run(&db, "worker-1");
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-1")
                .unwrap()
        );
        assert!(
            db.record_tmux_session_created_for_execution(&execution_id, "tok-1", 4242)
                .unwrap()
        );

        let (tmux, _tmux_server) = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-1".to_owned()],
            tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-1".to_owned())]),
            schemas: supported_schema("boss-worker-1"),
            pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
            ..Default::default()
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert_eq!(outcome.adopted_execution_ids, HashSet::from([execution_id.clone()]));
        assert_eq!(
            outcome.repaired_intents, 0,
            "the run was already 'created', not 'intended'"
        );
        assert_eq!(outcome.terminal_handoffs, 0);
        assert_eq!(outcome.refused_schema_skew, 0);

        assert_eq!(spawner.registry.lookup(4321).as_deref(), Some(execution_id.as_str()));
        assert_eq!(
            spawner.registry.pane_for_run(&execution_id),
            Some(crate::worker_registry::RegisteredWorkerPane {
                slot_id: 1,
                tmux_hosted: false,
                tmux_session_name: Some("boss-worker-1".to_owned()),
            }),
            "adopted sessions route input through tmux but retain legacy teardown and process reaping",
        );
        let live_state = spawner.live_states.get(1).expect("slot 1 must be registered");
        assert_eq!(live_state.run_id, execution_id);
        assert_eq!(live_state.shell_pid, 4321);
        assert_eq!(*spawner.publish_calls.lock().unwrap(), 1);
        assert_eq!(
            spawner.live_status_calls.lock().unwrap().as_slice(),
            &[(1u8, execution_id.clone())]
        );

        assert!(
            coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "the pool slot claim must be rebuilt",
        );

        let events = sink.events_for(&execution_id).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, Stage::TmuxWorkerAdopted.as_str());
    }

    /// A crash between `tmux new-session` and the confirmation write leaves
    /// `tmux_spawn_state = 'intended'` on an otherwise-live session. The pass
    /// must durably repair that to `created` (with the freshly read pane
    /// pid) in addition to rebuilding the in-memory state.
    #[tokio::test]
    async fn intended_state_is_repaired_before_rebuild() {
        let (_dir, db) = open_db_arc();
        let execution_id = start_local_run(&db, "worker-1");
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-intended")
                .unwrap()
        );

        let (tmux, _tmux_server) = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-1".to_owned()],
            tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-intended".to_owned())]),
            schemas: supported_schema("boss-worker-1"),
            pane_pids: HashMap::from([("boss-worker-1".to_owned(), "5555".to_owned())]),
            ..Default::default()
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert_eq!(outcome.adopted_execution_ids, HashSet::from([execution_id.clone()]));
        assert_eq!(outcome.repaired_intents, 1);

        let repaired = db
            .list_adoptable_tmux_runs()
            .unwrap()
            .into_iter()
            .find(|run| run.execution_id == execution_id)
            .expect("the run must still be adoptable after repair");
        assert_eq!(repaired.tmux_spawn_state, "created");
        assert_eq!(repaired.tmux_pane_pid, Some(5555));

        let events = sink.events_for(&execution_id).await;
        assert_eq!(events[0].details["repaired_intent"], serde_json::json!(true));
    }

    /// A live session whose token belongs to an execution the engine has
    /// already terminalized must be handed to `worker_readoption` unchanged
    /// — never adopted (rebuilding state for a terminal row would be exactly
    /// the double-tracking bug re-adoption exists to prevent) — as long as
    /// its `BOSS_SESSION_SCHEMA` clears the same guard `adopt_one`'s callers
    /// apply.
    #[tokio::test]
    async fn terminal_match_hands_off_to_convergence() {
        let (_dir, db) = open_db_arc();
        let execution_id = start_local_run(&db, "worker-1");
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-term")
                .unwrap()
        );
        assert!(
            db.record_tmux_session_created_for_execution(&execution_id, "tok-term", 111)
                .unwrap()
        );
        db.mark_execution_orphaned(&execution_id, "test: engine wrongly inferred death")
            .unwrap();

        let (tmux, _tmux_server) = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-1".to_owned()],
            tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-term".to_owned())]),
            schemas: supported_schema("boss-worker-1"),
            pane_pids: HashMap::from([("boss-worker-1".to_owned(), "111".to_owned())]),
            ..Default::default()
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let convergence = RecordingConvergence::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &convergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(outcome.terminal_handoffs, 1);
        assert_eq!(outcome.refused_schema_skew, 0);
        assert_eq!(
            convergence.calls.lock().unwrap().as_slice(),
            &[(execution_id, TERMINAL_HANDOFF_TRIGGER.to_owned())],
        );
        assert!(
            spawner.live_states.snapshot().is_empty(),
            "a terminal-status handoff must never rebuild live state directly",
        );
    }

    /// The counterpart to the schema-skew tests on the adoptable branch: a
    /// live session whose token resolves to a *terminal* execution must be
    /// refused and reaped, not handed to `worker_readoption`, when its
    /// `BOSS_SESSION_SCHEMA` fails the guard — otherwise a version-skewed
    /// session could dodge the guard entirely just by belonging to a
    /// terminalized row: the guard is enforced on both the `adopt_one`
    /// branch and this one.
    #[tokio::test]
    async fn terminal_match_with_bad_schema_is_refused_and_reaped_not_handed_off() {
        let (_dir, db) = open_db_arc();
        let execution_id = start_local_run(&db, "worker-1");
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-term-bad")
                .unwrap()
        );
        assert!(
            db.record_tmux_session_created_for_execution(&execution_id, "tok-term-bad", 111)
                .unwrap()
        );
        db.mark_execution_orphaned(&execution_id, "test: engine wrongly inferred death")
            .unwrap();
        let work_item_id = db.get_execution(&execution_id).unwrap().work_item_id;

        let (tmux, tmux_server) = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-1".to_owned()],
            tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-term-bad".to_owned())]),
            // No `schemas` entry: BOSS_SESSION_SCHEMA is missing entirely.
            pane_pids: HashMap::from([("boss-worker-1".to_owned(), "111".to_owned())]),
            ..Default::default()
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let convergence = RecordingConvergence::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &convergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert_eq!(outcome.terminal_handoffs, 0);
        assert_eq!(outcome.refused_schema_skew, 1);
        assert!(
            convergence.calls.lock().unwrap().is_empty(),
            "a schema-skewed session must never reach worker_readoption",
        );
        assert_eq!(
            tmux_server.killed_sessions.lock().unwrap().as_slice(),
            &["boss-worker-1".to_owned()]
        );

        let events = sink.events_for(&execution_id).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, Stage::TmuxAdoptionRefused.as_str());
        assert_eq!(events[0].details["schema_guard_failure"], serde_json::json!("missing"));

        let attentions = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(attentions.len(), 1);
        assert_eq!(attentions[0].kind, TMUX_ADOPTION_SCHEMA_SKEW_ATTENTION_KIND);
    }

    /// A live session whose token matches nothing in this DB at all — a
    /// leaked session — is left untouched. The dependent leaked/husk sweep
    /// task owns that case, not this pass.
    #[tokio::test]
    async fn unmatched_leaked_session_is_left_alone() {
        let (_dir, db) = open_db_arc();
        let (tmux, _tmux_server) = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-9".to_owned()],
            tokens: HashMap::from([("boss-worker-9".to_owned(), "tok-unknown".to_owned())]),
            ..Default::default()
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let convergence = RecordingConvergence::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &convergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(outcome.terminal_handoffs, 0);
        assert_eq!(
            outcome.untracked_sessions,
            vec![UntrackedTmuxSession {
                session_name: "boss-worker-9".to_owned(),
                spawn_token: "tok-unknown".to_owned(),
            }]
        );
        assert!(convergence.calls.lock().unwrap().is_empty());
        assert!(sink.events().await.is_empty());
    }

    /// The coordinator's session shares the worker tmux server but must
    /// never be adopted as a worker, even when it has the normal durable
    /// token and schema environment.
    #[tokio::test]
    async fn coordinator_session_is_ignored_by_worker_adoption() {
        let (_dir, db) = open_db_arc();
        let execution_id = start_local_run(&db, "worker-1");
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-coordinator", "coordinator-token")
                .unwrap()
        );
        let (tmux, tmux_server) = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-coordinator".to_owned()],
            tokens: HashMap::from([("boss-coordinator".to_owned(), "coordinator-token".to_owned())]),
            schemas: supported_schema("boss-coordinator"),
            pane_pids: HashMap::from([("boss-coordinator".to_owned(), "1234".to_owned())]),
            ..Default::default()
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(outcome.terminal_handoffs, 0);
        assert!(tmux_server.killed_sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn check_session_schema_pure_cases() {
        assert!(check_session_schema(Some(TMUX_SESSION_SCHEMA)).is_ok());
        assert_eq!(check_session_schema(None), Err(SchemaGuardFailure::Missing));
        assert_eq!(
            check_session_schema(Some("not-a-number")),
            Err(SchemaGuardFailure::Unparseable("not-a-number".to_owned()))
        );
        let too_new = TMUX_SESSION_SCHEMA.parse::<u32>().unwrap() + 1;
        assert_eq!(
            check_session_schema(Some(&too_new.to_string())),
            Err(SchemaGuardFailure::TooNew(too_new))
        );
    }

    /// A live session whose token matches a non-terminal row but carries no
    /// `BOSS_SESSION_SCHEMA` at all must be refused and reaped, not adopted
    /// — an unknown schema is always version skew, never assumed
    /// compatible, no matter how small its number would parse to.
    #[tokio::test]
    async fn missing_schema_is_refused_and_reaped() {
        let (_dir, db) = open_db_arc();
        let execution_id = start_local_run(&db, "worker-1");
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-noschema")
                .unwrap()
        );
        assert!(
            db.record_tmux_session_created_for_execution(&execution_id, "tok-noschema", 4242)
                .unwrap()
        );
        let work_item_id = db.get_execution(&execution_id).unwrap().work_item_id;

        let (tmux, tmux_server) = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-1".to_owned()],
            tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-noschema".to_owned())]),
            pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
            ..Default::default()
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(outcome.refused_schema_skew, 1);
        assert!(
            spawner.live_states.snapshot().is_empty(),
            "a refused session must never be adopted into live state",
        );
        assert_eq!(
            tmux_server.killed_sessions.lock().unwrap().as_slice(),
            &["boss-worker-1".to_owned()]
        );

        let events = sink.events_for(&execution_id).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, Stage::TmuxAdoptionRefused.as_str());
        assert_eq!(events[0].details["schema_guard_failure"], serde_json::json!("missing"));

        let attentions = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(attentions.len(), 1);
        assert_eq!(attentions[0].kind, TMUX_ADOPTION_SCHEMA_SKEW_ATTENTION_KIND);
        assert_eq!(attentions[0].status, "open");
    }

    /// When the refusal is right but the `kill-session` that is supposed to
    /// back it up fails, [`refuse_and_reap`] must report the failure rather
    /// than the unconditional-success shape: [`Outcome::Error`], a
    /// `"reaped": false` detail, and an attention body naming the manual
    /// recovery command — while still filing the attention item and still
    /// counting the refusal in [`TmuxAdoptionOutcome::refused_schema_skew`],
    /// since the guard itself was still correctly triggered.
    #[tokio::test]
    async fn schema_skew_with_failed_kill_reports_not_reaped() {
        let (_dir, db) = open_db_arc();
        let execution_id = start_local_run(&db, "worker-1");
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-noschema")
                .unwrap()
        );
        assert!(
            db.record_tmux_session_created_for_execution(&execution_id, "tok-noschema", 4242)
                .unwrap()
        );
        let work_item_id = db.get_execution(&execution_id).unwrap().work_item_id;

        let (tmux, tmux_server) = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-1".to_owned()],
            tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-noschema".to_owned())]),
            pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
            kill_session_failures: HashSet::from(["boss-worker-1".to_owned()]),
            ..Default::default()
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(
            outcome.refused_schema_skew, 1,
            "the guard was still correctly triggered even though the kill failed",
        );
        assert!(
            spawner.live_states.snapshot().is_empty(),
            "a refused session must never be adopted into live state",
        );
        assert_eq!(
            tmux_server.killed_sessions.lock().unwrap().as_slice(),
            &["boss-worker-1".to_owned()],
            "the kill was attempted even though it failed",
        );

        let events = sink.events_for(&execution_id).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, Stage::TmuxAdoptionRefused.as_str());
        assert_eq!(
            events[0].outcome,
            Outcome::Error.as_str(),
            "a failed kill must not report the same outcome as a successful reap",
        );
        assert_eq!(events[0].details["schema_guard_failure"], serde_json::json!("missing"));
        assert_eq!(events[0].details["reaped"], serde_json::json!(false));

        let attentions = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(
            attentions.len(),
            1,
            "the attention item must still be filed even when the kill itself failed",
        );
        assert_eq!(attentions[0].kind, TMUX_ADOPTION_SCHEMA_SKEW_ATTENTION_KIND);
        assert_eq!(attentions[0].status, "open");
        assert!(
            attentions[0].body_markdown.contains("kill-session"),
            "the not-reaped attention body must name the manual recovery command",
        );
    }

    /// A schema newer than this engine's own contract is refused and reaped
    /// the same way as a missing one — "unknown" and "too new" are both
    /// version skew.
    #[tokio::test]
    async fn newer_schema_is_refused_and_reaped() {
        let (_dir, db) = open_db_arc();
        let execution_id = start_local_run(&db, "worker-1");
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-newschema")
                .unwrap()
        );
        assert!(
            db.record_tmux_session_created_for_execution(&execution_id, "tok-newschema", 4242)
                .unwrap()
        );

        let too_new = TMUX_SESSION_SCHEMA.parse::<u32>().unwrap() + 1;
        let (tmux, tmux_server) = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-1".to_owned()],
            tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-newschema".to_owned())]),
            schemas: HashMap::from([("boss-worker-1".to_owned(), too_new.to_string())]),
            pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
            ..Default::default()
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(outcome.refused_schema_skew, 1);
        assert_eq!(
            tmux_server.killed_sessions.lock().unwrap().as_slice(),
            &["boss-worker-1".to_owned()]
        );

        let events = sink.events_for(&execution_id).await;
        assert_eq!(events[0].details["schema_guard_failure"], serde_json::json!("too_new"));
        assert_eq!(events[0].details["session_schema"], serde_json::json!(too_new));
        assert_eq!(
            events[0].details["supported_schema"],
            serde_json::json!(TMUX_SESSION_SCHEMA)
        );
    }

    /// Server-scoped ownership conflict: a different, still-live engine
    /// process already holds `@boss_engine_owner`. The whole pass must
    /// refuse rather than adopting anything, even though a perfectly valid
    /// matching session/token/schema is present.
    #[tokio::test]
    async fn conflicting_live_owner_refuses_the_whole_pass() {
        let (_dir, db) = open_db_arc();
        let execution_id = start_local_run(&db, "worker-1");
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-1")
                .unwrap()
        );
        assert!(
            db.record_tmux_session_created_for_execution(&execution_id, "tok-1", 4242)
                .unwrap()
        );

        // pid 1 (init/launchd) is always alive but is never this test
        // process; `kill(1, 0)` from a non-root process returns EPERM,
        // which the liveness contract treats as "alive" — exactly the case
        // this guard must refuse on.
        let (tmux, tmux_server) = fake_tmux(
            FakeTmuxServer {
                sessions: vec!["boss-worker-1".to_owned()],
                tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-1".to_owned())]),
                schemas: supported_schema("boss-worker-1"),
                pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
                ..Default::default()
            }
            .with_engine_owner("1"),
        );
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(outcome.owner_conflict);
        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(outcome.refused_schema_skew, 0);
        assert!(tmux_server.killed_sessions.lock().unwrap().is_empty());

        let events = sink.events().await;
        assert_eq!(
            events.len(),
            1,
            "a refused pass must not touch any execution, but the conflict itself must be loud, \
             not log-only"
        );
        assert_eq!(events[0].stage, Stage::TmuxAdoptionOwnerConflict.as_str());
        assert_eq!(events[0].outcome, Outcome::Error.as_str());
        assert_eq!(events[0].execution_id, "engine-boot");
        assert_eq!(events[0].details["other_pid"], serde_json::json!(1));
        assert_eq!(events[0].details["this_pid"], serde_json::json!(std::process::id()));
    }

    /// A server option already stamped with *this* process's own pid is not
    /// a conflict — the pass proceeds normally.
    #[tokio::test]
    async fn owner_option_matching_this_process_is_not_a_conflict() {
        let (_dir, db) = open_db_arc();
        let (tmux, _tmux_server) =
            fake_tmux(FakeTmuxServer::default().with_engine_owner(std::process::id().to_string()));
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(!outcome.owner_conflict);
    }

    /// A stamp left by an engine that has since died — the common
    /// post-crash case — must NOT be treated as a conflict, and must be
    /// overwritten by this pass's own claim.
    #[tokio::test]
    async fn dead_owner_pid_is_not_a_conflict_and_adoption_proceeds() {
        let (_dir, db) = open_db_arc();
        let execution_id = start_local_run(&db, "worker-1");
        assert!(
            db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-worker-1", "tok-1")
                .unwrap()
        );
        assert!(
            db.record_tmux_session_created_for_execution(&execution_id, "tok-1", 4242)
                .unwrap()
        );

        let dead = dead_pid();
        let (tmux, tmux_server) = fake_tmux(
            FakeTmuxServer {
                sessions: vec!["boss-worker-1".to_owned()],
                tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-1".to_owned())]),
                schemas: supported_schema("boss-worker-1"),
                pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
                ..Default::default()
            }
            .with_engine_owner(dead.to_string()),
        );
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            // Irrelevant here: `probe_pid(dead)` returns `Dead`, so the
            // identity probe is never even consulted for a dead pid.
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(!outcome.owner_conflict);
        assert_eq!(outcome.adopted_execution_ids, HashSet::from([execution_id]));

        let stamped = tmux_server
            .server_options
            .lock()
            .unwrap()
            .get(ENGINE_OWNER_OPTION.trim_start_matches('@'))
            .cloned()
            .expect("the claim must overwrite the stale stamp");
        assert_eq!(
            stamped,
            std::process::id().to_string(),
            "expected this process's own pid to be re-stamped, got {stamped:?}"
        );
    }

    /// A live pid whose executable does not look like this engine's own
    /// binary is treated as a stale stamp (pid reuse after a crash), not a
    /// conflict — the fix for a recycled pid otherwise disabling adoption
    /// forever with only a log line to show for it.
    #[tokio::test]
    async fn live_pid_that_is_not_an_engine_is_not_a_conflict() {
        let (_dir, db) = open_db_arc();
        // pid 1 (init/launchd) is always alive but is never this test
        // process; `kill(1, 0)` returns EPERM, which the liveness contract
        // treats as "alive" — the identity probe is what must then say "but
        // that's not an engine".
        let (tmux, tmux_server) = fake_tmux(FakeTmuxServer::default().with_engine_owner("1"));
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(false)),
        )
        .await;

        assert!(!outcome.owner_conflict);
        let stamped = tmux_server
            .server_options
            .lock()
            .unwrap()
            .get(ENGINE_OWNER_OPTION.trim_start_matches('@'))
            .cloned()
            .expect("the claim must overwrite the stale stamp");
        assert_eq!(stamped, std::process::id().to_string());
    }

    /// The claim write itself: a regression that silently dropped the
    /// `set_server_option` call would keep every other ownership test green,
    /// so this asserts the stamped value directly — the bare pid of this
    /// process.
    #[tokio::test]
    async fn claim_writes_own_pid() {
        let (_dir, db) = open_db_arc();
        let (tmux, tmux_server) = fake_tmux(FakeTmuxServer::default());
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(
            &db,
            &tmux,
            &coordinator,
            &spawner,
            &NoopLiveWorkerConvergence,
            &sink,
            &FixedEngineOwnerProbe(Some(true)),
        )
        .await;

        assert!(!outcome.owner_conflict);
        let stamped = tmux_server
            .server_options
            .lock()
            .unwrap()
            .get(ENGINE_OWNER_OPTION.trim_start_matches('@'))
            .cloned()
            .expect("claim_or_detect_conflicting_owner must always stamp the option on success");
        assert_eq!(stamped.parse::<u32>().unwrap(), std::process::id());
    }
}
