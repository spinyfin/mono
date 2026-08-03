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

use std::collections::{HashMap, HashSet};

use boss_tmux::{DisplayField, Tmux};

use crate::coordinator::{ExecutionCoordinator, slot_id_from_worker_id};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::{LiveSpawnRouting, ReadoptionEvidence, attributed_pool_label};
use crate::spawn_flow::WorkerSpawner;
use crate::work::{TmuxRunHandle, WorkDb};
use crate::worker_readoption::LiveWorkerConvergence;

/// The tmux session environment variable carrying the durable spawn token.
/// Mirrors [`crate::spawn_flow`]'s private constant of the same name — this
/// module reads what that one writes, from a different process generation at
/// boot, so it is redeclared rather than imported.
const TMUX_SPAWN_TOKEN_ENV: &str = "BOSS_SPAWN_TOKEN";

/// Trigger name recorded on the dispatch event and carried into
/// [`LiveWorkerConvergence::converge_live_worker`] for a session whose token
/// matched a terminal execution — the third trigger alongside
/// `hook_after_terminal` and `redispatch_guard` (see
/// [`crate::worker_readoption`]'s module doc).
const TERMINAL_HANDOFF_TRIGGER: &str = "boot_tmux_adoption";

/// What one pass did; the caller logs it.
#[derive(Debug, Default, Clone)]
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
}

/// Run one boot-time adoption pass. See the module doc for the full
/// algorithm; this is the entry point `app/server.rs` calls once at startup,
/// ahead of [`crate::run_reconcile::probe_in_flight_runs`].
pub async fn run_boot_time_adoption<S>(
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
                "tmux boot adoption: list-sessions failed; skipping the pass (best-effort)",
            );
            return outcome;
        }
    };
    if sessions.is_empty() {
        return outcome;
    }

    // The authoritative token per session — never the `@boss_spawn_token`
    // option mirror `list_sessions` also returns. See the module doc.
    let mut live_tokens: HashMap<String, String> = HashMap::new();
    for session in &sessions {
        match tmux.show_environment(&session.name, TMUX_SPAWN_TOKEN_ENV).await {
            Ok(Some(token)) => {
                live_tokens.insert(token, session.name.clone());
            }
            Ok(None) => {
                // Not a Boss worker session (or predates this env contract)
                // — nothing to adopt.
            }
            Err(err) => {
                tracing::warn!(
                    session = %session.name,
                    error = %format!("{err:#}"),
                    "tmux boot adoption: show-environment failed for a session; skipping it",
                );
            }
        }
    }
    if live_tokens.is_empty() {
        return outcome;
    }

    let adoptable = match work_db.list_adoptable_tmux_runs() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(
                error = %format!("{err:#}"),
                "tmux boot adoption: failed to list adoptable runs; skipping the pass",
            );
            return outcome;
        }
    };

    let mut claimed_tokens: HashSet<String> = HashSet::new();
    for handle in &adoptable {
        let Some(session_name) = live_tokens.get(&handle.tmux_spawn_token) else {
            continue;
        };
        claimed_tokens.insert(handle.tmux_spawn_token.clone());
        adopt_one(
            work_db,
            tmux,
            coordinator,
            spawner,
            dispatch_events,
            handle,
            session_name,
            &mut outcome,
        )
        .await;
    }

    for (token, session_name) in &live_tokens {
        if claimed_tokens.contains(token) {
            continue;
        }
        hand_off_if_terminal(work_db, convergence, token, session_name, &mut outcome).await;
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
        .register_run_slot(execution_id.to_owned(), slot_id);
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

/// A live token that matched no adoptable row: if it resolves to a terminal
/// execution, hand it to [`crate::worker_readoption`] unchanged. Anything
/// else (unknown token, or a non-terminal execution excluded from
/// `list_adoptable_tmux_runs` for some other reason) is left alone — out of
/// scope for this pass.
async fn hand_off_if_terminal(
    work_db: &WorkDb,
    convergence: &dyn LiveWorkerConvergence,
    token: &str,
    session_name: &str,
    outcome: &mut TmuxAdoptionOutcome,
) {
    let execution_id = match work_db.execution_id_for_tmux_spawn_token(token) {
        Ok(Some(id)) => id,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(
                session = session_name,
                error = %format!("{err:#}"),
                "tmux boot adoption: failed to resolve a live session's spawn token; skipping",
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
                "tmux boot adoption: failed to load the execution behind a live session's spawn token",
            );
            return;
        }
    };
    if !execution.status.is_terminal() {
        // Non-terminal but absent from `list_adoptable_tmux_runs` — e.g. a
        // non-`local` host_id. Not this pass's job.
        return;
    }
    convergence
        .converge_live_worker(&execution_id, TERMINAL_HANDOFF_TRIGGER)
        .await;
    outcome.terminal_handoffs += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
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
    /// `show-environment BOSS_SPAWN_TOKEN`, and `display-message
    /// #{pane_pid}`, keyed by session name. Panics on any other command —
    /// this pass never calls `new-session`/`set-option`/anything else.
    #[derive(Default)]
    struct FakeTmuxServer {
        /// Session names `list-sessions` should report.
        sessions: Vec<String>,
        /// `show-environment BOSS_SPAWN_TOKEN` answer per session. Absent
        /// entries answer "unknown variable" (unset), matching a session
        /// with no Boss identity.
        tokens: HashMap<String, String>,
        /// `display-message #{pane_pid}` answer per session.
        pane_pids: HashMap<String, String>,
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
                    assert_eq!(args[5], TMUX_SPAWN_TOKEN_ENV);
                    match self.tokens.get(session) {
                        Some(token) => Ok(ok_output(format!("{TMUX_SPAWN_TOKEN_ENV}={token}\n"))),
                        None => Ok(CommandOutput {
                            success: false,
                            code: Some(1),
                            stdout: String::new(),
                            stderr: "unknown variable: BOSS_SPAWN_TOKEN".to_owned(),
                        }),
                    }
                }
                Some("display-message") => {
                    assert_eq!(args[4], "-t");
                    let session = &args[5];
                    let pid = self.pane_pids.get(session).cloned().unwrap_or_else(|| "0".to_owned());
                    Ok(ok_output(format!("{pid}\n")))
                }
                other => panic!("unexpected tmux command in test: {other:?} (full args={args:?})"),
            }
        }
    }

    fn fake_tmux(server: FakeTmuxServer) -> Tmux {
        Tmux::with_runner("/opt/homebrew/bin/tmux", Arc::new(server)).unwrap()
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

    fn coordinator_with_one_slot(db: Arc<WorkDb>) -> ExecutionCoordinator {
        ExecutionCoordinator::new(db, WorkerPool::new(1), Arc::new(NoopCube), Arc::new(NoopRunner))
    }

    #[tokio::test]
    async fn no_live_sessions_is_a_cheap_noop() {
        let (_dir, db) = open_db_arc();
        let tmux = fake_tmux(FakeTmuxServer::default());
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome =
            run_boot_time_adoption(&db, &tmux, &coordinator, &spawner, &NoopLiveWorkerConvergence, &sink).await;

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

        let tmux = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-1".to_owned()],
            tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-1".to_owned())]),
            pane_pids: HashMap::from([("boss-worker-1".to_owned(), "4321".to_owned())]),
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome =
            run_boot_time_adoption(&db, &tmux, &coordinator, &spawner, &NoopLiveWorkerConvergence, &sink).await;

        assert_eq!(outcome.adopted_execution_ids, HashSet::from([execution_id.clone()]));
        assert_eq!(
            outcome.repaired_intents, 0,
            "the run was already 'created', not 'intended'"
        );
        assert_eq!(outcome.terminal_handoffs, 0);

        assert_eq!(spawner.registry.lookup(4321).as_deref(), Some(execution_id.as_str()));
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

        let tmux = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-1".to_owned()],
            tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-intended".to_owned())]),
            pane_pids: HashMap::from([("boss-worker-1".to_owned(), "5555".to_owned())]),
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome =
            run_boot_time_adoption(&db, &tmux, &coordinator, &spawner, &NoopLiveWorkerConvergence, &sink).await;

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
    /// the double-tracking bug re-adoption exists to prevent).
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

        let tmux = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-1".to_owned()],
            tokens: HashMap::from([("boss-worker-1".to_owned(), "tok-term".to_owned())]),
            pane_pids: HashMap::from([("boss-worker-1".to_owned(), "111".to_owned())]),
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let convergence = RecordingConvergence::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(&db, &tmux, &coordinator, &spawner, &convergence, &sink).await;

        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(outcome.terminal_handoffs, 1);
        assert_eq!(
            convergence.calls.lock().unwrap().as_slice(),
            &[(execution_id, TERMINAL_HANDOFF_TRIGGER.to_owned())],
        );
        assert!(
            spawner.live_states.snapshot().is_empty(),
            "a terminal-status handoff must never rebuild live state directly",
        );
    }

    /// A live session whose token matches nothing in this DB at all — a
    /// leaked session — is left untouched. The dependent leaked/husk sweep
    /// task owns that case, not this pass.
    #[tokio::test]
    async fn unmatched_leaked_session_is_left_alone() {
        let (_dir, db) = open_db_arc();
        let tmux = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-worker-9".to_owned()],
            tokens: HashMap::from([("boss-worker-9".to_owned(), "tok-unknown".to_owned())]),
            pane_pids: HashMap::new(),
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let convergence = RecordingConvergence::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome = run_boot_time_adoption(&db, &tmux, &coordinator, &spawner, &convergence, &sink).await;

        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(outcome.terminal_handoffs, 0);
        assert!(convergence.calls.lock().unwrap().is_empty());
        assert!(sink.events().await.is_empty());
    }

    /// A tmux session with no `BOSS_SPAWN_TOKEN` at all (e.g. the
    /// coordinator's own session, sharing the same `-L boss` server) must
    /// never be treated as a candidate.
    #[tokio::test]
    async fn session_without_a_spawn_token_is_ignored() {
        let (_dir, db) = open_db_arc();
        let tmux = fake_tmux(FakeTmuxServer {
            sessions: vec!["boss-coordinator".to_owned()],
            tokens: HashMap::new(),
            pane_pids: HashMap::new(),
        });
        let coordinator = coordinator_with_one_slot(db.clone());
        let spawner = RecordingSpawner::default();
        let sink = RecordingDispatchEventSink::new();

        let outcome =
            run_boot_time_adoption(&db, &tmux, &coordinator, &spawner, &NoopLiveWorkerConvergence, &sink).await;

        assert!(outcome.adopted_execution_ids.is_empty());
        assert_eq!(outcome.terminal_handoffs, 0);
    }
}
