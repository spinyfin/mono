//! Coverage for [`crate::app::tmux_teardown`]'s token-verified reap
//! sequence, driven against a stubbed [`Tmux`] so these tests need no real
//! tmux binary on the host — mirroring [`boss_engine_runtime::tmux_adoption`]'s
//! `FakeTmuxServer` pattern.

use super::tmux_stub::{failure, fake_tmux, ok};
use super::*;
use crate::app::tmux_teardown::TmuxTeardownOutcome;
use boss_engine_runtime::test_support::*;
use boss_work_db::TmuxIdentity;

/// Seed an execution with a started (unfinished) local run and a durably
/// recorded tmux identity — the shape `reap_tmux_worker` reads from.
fn seed_tmux_run(db: &WorkDb, work_item_id: &str, session_name: &str, token: &str, pane_pid: i64) -> String {
    let execution_id = create_old_execution(db, work_item_id);
    db.start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::SERVER_LABEL, session_name, token)
            .unwrap(),
        "intent write must find the just-started run row",
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, token, pane_pid)
            .unwrap(),
        "creation write must find the intent row it just wrote",
    );
    execution_id
}

fn read_back_identity(db: &WorkDb, execution_id: &str) -> TmuxIdentity {
    db.tmux_identity_for_execution(execution_id)
        .unwrap()
        .expect("seeded identity must round-trip")
}

/// A real probe failure must retain the slot; tmux 3.6a's missing-session
/// response on the next attempt must finish the reap and restore capacity.
#[tokio::test]
async fn no_such_session_releases_the_pool_slot_after_a_refused_reap() {
    let (state, _dir) = test_server_state();
    let db = state.work_db.as_ref();
    let product = create_product(db);
    let item = create_active_chore(db, &product, "absent worker");
    let exec = seed_tmux_run(db, &item, "boss-absent", "tok-absent", 999_999);
    let identity = read_back_identity(db, &exec);
    let pool = state.execution_coordinator.worker_pool();
    assert_eq!(pool.claim_worker(&exec, None).await.unwrap(), "worker-1");
    state.worker_registry.register_run_slot(&exec, 1);
    let peer_pid = std::process::id() as libc::pid_t;
    state.worker_registry.register(peer_pid, &exec);
    state.live_worker_states.register_spawn(1, &exec, "test", 0, None);
    let (tmux, runner) = fake_tmux([
        failure("error connecting to /state/boss/tmux.sock (Permission denied)"),
        failure("no such session: boss-absent"),
    ]);
    state.set_tmux_override_for_test(tmux);

    assert_eq!(state.release_worker_pane(&exec).await, PaneReleaseOutcome::NoLiveWorker);
    assert_eq!(db.tmux_identity_for_execution(&exec).unwrap(), Some(identity));
    assert_eq!(pool.idle_count().await, 0);
    assert_eq!(state.worker_registry.slot_for_run(&exec), Some(1));
    assert_eq!(
        state.worker_registry.lookup_with_ancestor_walk(peer_pid).as_deref(),
        Some(exec.as_str())
    );
    assert!(state.live_worker_states.get(1).is_some());

    assert_eq!(state.release_worker_pane(&exec).await, PaneReleaseOutcome::Reaped);
    assert!(db.tmux_identity_for_execution(&exec).unwrap().is_none());
    assert_eq!(pool.idle_count().await, 1);
    assert!(state.worker_registry.slot_for_run(&exec).is_none());
    assert_eq!(state.worker_registry.lookup_with_ancestor_walk(peer_pid), None);
    assert!(state.live_worker_states.get(1).is_none());
    let observation = db.tmux_pane_observation_for_execution(&exec).unwrap().unwrap();
    assert_eq!(observation.kind, boss_work_db::TmuxPaneObservationKind::SessionMissing);
    assert_eq!(pool.claim_worker("next-execution", None).await.unwrap(), "worker-1");
    assert_eq!(runner.calls().len(), 2, "neither probe should signal or kill a session");
    assert!(runner.calls().iter().all(|args| args[2] == "show-environment"));
}

#[tokio::test]
async fn terminal_run_retains_confinement_when_teardown_is_refused() {
    let (state, _dir) = test_server_state();
    let product = create_product(&state.work_db);
    let item = create_active_chore(&state.work_db, &product, "finished worker");
    let exec = seed_tmux_run(&state.work_db, &item, "boss-finished", "tok-finished", 999_999);
    let peer_pid = std::process::id() as libc::pid_t;
    state.worker_registry.register(peer_pid, &exec);
    state.worker_registry.register_run_slot(&exec, 1);
    state.work_db.cancel_running_execution(&exec).unwrap();
    let (tmux, _) = fake_tmux([failure("permission denied")]);
    state.set_tmux_override_for_test(tmux);

    assert_eq!(state.release_worker_pane(&exec).await, PaneReleaseOutcome::NoLiveWorker);
    assert_eq!(state.worker_registry.slot_for_run(&exec), Some(1));
    assert_eq!(state.worker_registry.lookup_with_ancestor_walk(peer_pid), Some(exec));
    assert!(state.classify_peer(Some(peer_pid)).is_worker());
}

/// The cardinal case: live token matches, so the pane pid's process group
/// is signalled, the session is killed, and the identity columns clear.
#[tokio::test]
async fn matched_token_signals_kills_and_clears_identity() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = seed_tmux_run(db, &work_item_id, "boss-1-example", "tok-match", i64::from(pid));
    let identity = read_back_identity(db, &execution_id);

    // A verified token match now also runs the narrow `#{pane_dead}` probe
    // ([`boss_engine_runtime::tmux_adoption::observe_pane_dead_state`]) — just
    // `#{pane_dead}` here, since the pane is alive — before
    // `kill_session_verified` re-verifies the token a second time and
    // issues `kill-session`.
    let (tmux, runner) = fake_tmux([
        ok("BOSS_SPAWN_TOKEN=tok-match\n"),
        ok("0"),
        ok("BOSS_SPAWN_TOKEN=tok-match\n"),
        ok(""),
        ok(""),
    ]);
    runner.own_process(i64::from(pid), "tok-match");
    let outcome = server_state
        .reap_tmux_worker_with(&tmux, &execution_id, &identity)
        .await;
    assert_eq!(outcome, TmuxTeardownOutcome::Reaped);
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1-example",
                "BOSS_SPAWN_TOKEN"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "display-message",
                "-p",
                "-t",
                "boss-1-example",
                "#{pane_dead}"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1-example",
                "BOSS_SPAWN_TOKEN"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "detach-client",
                "-s",
                "=boss-1-example"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "if-shell",
                "-F",
                "-t",
                "=boss-1-example:",
                "#{==:#{session_attached_list},}",
                "kill-session -t '=boss-1-example'",
                "display-message -p boss-viewers-attached"
            ],
        ],
    );

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "the pane pid's process group must have been signalled"
    );

    assert!(
        db.tmux_identity_for_execution(&execution_id).unwrap().is_none(),
        "identity columns must be cleared after a successful reap",
    );

    let observation = db
        .tmux_pane_observation_for_execution(&execution_id)
        .unwrap()
        .expect("the completion-path probe must persist an observation, not just the sweep's");
    assert_eq!(observation.kind, boss_work_db::TmuxPaneObservationKind::Alive);
    assert_eq!(
        observation.pane_dead,
        Some(false),
        "pane was alive at probe time; teardown's own signal/kill happens after this observation",
    );
}

#[tokio::test]
async fn matched_token_persists_dead_pane_observation_and_logs_at_info() {
    let buffer = boss_engine_runtime::test_support::log_capture::install();
    let start = buffer.lock().len();
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = seed_tmux_run(db, &work_item_id, "boss-1-dead", "tok-dead", i64::from(pid));
    let identity = read_back_identity(db, &execution_id);
    let (tmux, runner) = fake_tmux([
        ok("BOSS_SPAWN_TOKEN=tok-dead\n"),
        ok("1"),
        ok("7"),
        ok("BOSS_SPAWN_TOKEN=tok-dead\n"),
        ok(""),
        ok(""),
    ]);
    runner.own_process(i64::from(pid), "tok-dead");

    assert_eq!(
        server_state
            .reap_tmux_worker_with(&tmux, &execution_id, &identity)
            .await,
        TmuxTeardownOutcome::Reaped,
    );
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "the pane pid's process group must have been signalled"
    );

    assert!(
        db.tmux_identity_for_execution(&execution_id).unwrap().is_none(),
        "identity columns must still be cleared by the same reap that recorded the Dead observation",
    );

    let observation = db
        .tmux_pane_observation_for_execution(&execution_id)
        .unwrap()
        .expect("the normal completion path must persist a Dead observation, surviving the identity clear above");
    assert_eq!(observation.kind, boss_work_db::TmuxPaneObservationKind::Dead);
    assert_eq!(observation.pane_dead, Some(true));
    assert_eq!(observation.pane_dead_status.as_deref(), Some("7"));
    assert_eq!(
        observation.session_name, "boss-1-dead",
        "the retained session name must be recorded",
    );

    let captured = String::from_utf8(buffer.lock()[start..].to_vec()).expect("utf8 log capture");
    let line = captured
        .lines()
        .find(|line| line.contains("token-verified pane observation") && line.contains(&execution_id))
        .unwrap_or_else(|| panic!("no pane-observation log for {execution_id}; captured:\n{captured}"));
    assert!(line.contains("INFO"), "pane observation must be logged at INFO: {line}");
    assert!(
        line.contains("pane_dead=\"true\""),
        "log must include pane_dead: {line}"
    );
    assert!(
        line.contains("pane_dead_status=\"7\""),
        "log must include pane_dead_status: {line}"
    );
}

/// A live session by the same name but a DIFFERENT token — the "name
/// recycled onto another execution" hazard the design calls out. Nothing
/// may be signalled or killed, and the identity columns must survive so a
/// leaked-session sweep can reconcile the row later.
#[tokio::test]
async fn token_mismatch_refuses_to_touch_anything() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = seed_tmux_run(db, &work_item_id, "boss-1-example", "tok-ours", i64::from(pid));
    let identity = read_back_identity(db, &execution_id);

    let (tmux, runner) = fake_tmux([ok("BOSS_SPAWN_TOKEN=tok-someone-elses\n")]);
    let outcome = server_state
        .reap_tmux_worker_with(&tmux, &execution_id, &identity)
        .await;
    assert_eq!(outcome, TmuxTeardownOutcome::Refused);
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "show-environment",
            "-t",
            "boss-1-example",
            "BOSS_SPAWN_TOKEN"
        ]],
        "a mismatch must never issue kill-session",
    );

    assert!(
        child.try_wait().expect("try_wait on child").is_none(),
        "the bystander process must NOT have been signalled",
    );
    child.kill().expect("kill the test child");
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;

    assert_eq!(
        db.tmux_identity_for_execution(&execution_id).unwrap(),
        Some(identity),
        "identity columns must survive a refused teardown for a later reconcile pass",
    );

    let events = boss_dispatch_reader::read_current(&server_state.dispatch_event_root)
        .expect("read dispatch events")
        .events;
    let mismatch = events
        .iter()
        .find(|event| event.stage == boss_dispatch_events::Stage::TmuxTokenMismatch.as_str())
        .unwrap_or_else(|| panic!("expected tmux_token_mismatch, got: {events:?}"));
    assert_eq!(mismatch.execution_id, execution_id);
    assert_eq!(mismatch.outcome, boss_dispatch_events::Outcome::Error.as_str());
    assert_eq!(mismatch.details["tmux_session_name"], "boss-1-example");
    assert_eq!(mismatch.details["operation"], "teardown_preflight");
}

/// The session is already gone (or never carried a Boss token). Nothing to
/// signal or kill, but this is still a completed, idempotent teardown —
/// the identity columns clear.
#[tokio::test]
async fn absent_session_clears_identity_without_signalling() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = seed_tmux_run(db, &work_item_id, "boss-1-example", "tok-gone", i64::from(pid));
    let identity = read_back_identity(db, &execution_id);

    let (tmux, runner) = fake_tmux([failure("unknown variable: BOSS_SPAWN_TOKEN")]);
    let outcome = server_state
        .reap_tmux_worker_with(&tmux, &execution_id, &identity)
        .await;
    assert_eq!(outcome, TmuxTeardownOutcome::Reaped);
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "show-environment",
            "-t",
            "boss-1-example",
            "BOSS_SPAWN_TOKEN"
        ]],
        "an absent session must never issue kill-session",
    );

    assert!(
        child.try_wait().expect("try_wait on child").is_none(),
        "nothing to signal when the session was already gone",
    );
    child.kill().expect("kill the test child");
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;

    assert!(db.tmux_identity_for_execution(&execution_id).unwrap().is_none());

    let observation = db
        .tmux_pane_observation_for_execution(&execution_id)
        .unwrap()
        .expect("an absent session must still be recorded as SessionMissing, not left blank");
    assert_eq!(observation.kind, boss_work_db::TmuxPaneObservationKind::SessionMissing);
    assert_eq!(observation.pane_dead, None);
}

/// [`ServerState::tmux_for_run`] routing: a run recorded with the literal
/// legacy server label must resolve to a `-L boss` handle built from the
/// same executable and runner as the durable-socket handle, not the
/// durable socket itself. Reusing `socket_tmux`'s runner keeps a stubbed
/// handle stubbed; a real runner is still refused (see
/// [`tmux_for_run_refuses_legacy_label_with_the_real_runner`]).
#[tokio::test]
async fn tmux_for_run_routes_legacy_label_to_the_label_server() {
    let (server_state, _dir) = test_server_state();
    let (socket_tmux, _runner) = fake_tmux(Vec::<boss_tmux::CommandOutput>::new());

    let legacy = server_state
        .tmux_for_run(&socket_tmux, boss_tmux::SERVER_LABEL)
        .unwrap();
    assert_eq!(
        legacy.operator_prefix(),
        format!("tmux -L {}", boss_tmux::quote_for_shell(boss_tmux::SERVER_LABEL))
    );
    assert_eq!(legacy.program(), socket_tmux.program());

    let socket = server_state
        .tmux_for_run(&socket_tmux, boss_tmux::TEST_SOCKET_PATH)
        .unwrap();
    assert_eq!(
        socket.operator_prefix(),
        format!("tmux -S {}", boss_tmux::quote_for_shell(boss_tmux::TEST_SOCKET_PATH))
    );
}

/// A test process must refuse a legacy-label handle backed by the real
/// runner: `-L boss` addresses tmux's shared default socket and cannot
/// be scoped to a private path.
#[tokio::test]
async fn tmux_for_run_refuses_legacy_label_with_the_real_runner() {
    let (server_state, _dir) = test_server_state();
    let real = boss_tmux::Tmux::from_path_with_socket("/opt/homebrew/bin/tmux", boss_tmux::TEST_SOCKET_PATH).unwrap();
    let error = server_state.tmux_for_run(&real, boss_tmux::SERVER_LABEL).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("refusing to construct a legacy `-L boss` tmux handle"),
        "error was: {error:#}"
    );
}

/// The public [`ServerState::reap_tmux_worker`] wrapper: an execution with
/// no recorded tmux identity is a cheap no-op — critically, it must not
/// even attempt to resolve a real `Tmux`, so this test needs no tmux
/// binary on the host.
#[tokio::test]
async fn no_recorded_identity_refuses_local_teardown() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_old_execution(db, &work_item_id);

    assert_eq!(
        server_state.reap_tmux_worker(&execution_id).await,
        TmuxTeardownOutcome::Refused,
    );
    server_state.worker_registry.register_run_slot(&execution_id, 1);
    server_state
        .live_worker_states
        .register_spawn(1, &execution_id, "test", 0, None);
    let probe_id = server_state.queue_probe(execution_id.clone(), "still working?".into(), false);
    assert_eq!(
        server_state.release_worker_pane(&execution_id).await,
        PaneReleaseOutcome::NoLiveWorker
    );
    assert_eq!(server_state.worker_registry.slot_for_run(&execution_id), Some(1));
    assert!(server_state.live_worker_states.get(1).is_some());
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Queued)
    );
}

/// Exercise the sweep's production process-teardown hook across both a
/// transient tmux failure and a failed database clear after session absence.
#[tokio::test]
async fn pool_teardown_hook_recovers_from_refusal_and_failed_identity_clear() {
    use boss_engine_runtime::pool_claim_sweep::WorkerViewerDetach;
    let (state, _dir) = test_server_state();
    let db = state.work_db.as_ref();
    let product = create_product(db);
    let item = create_active_chore(db, &product, "teardown recovery");
    let exec = seed_tmux_run(db, &item, "boss-recovery-test", "tok-recovery", 999_999);
    let (tmux, runner) = fake_tmux([
        failure("temporary tmux failure"),
        failure("can't find session: boss-recovery-test"),
        failure("can't find session: boss-recovery-test"),
    ]);
    state.set_tmux_override_for_test(tmux);
    assert!(state.confirm_process_torn_down(&exec).await.is_err());
    assert!(db.tmux_identity_for_execution(&exec).unwrap().is_some());
    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_identity_clear BEFORE UPDATE OF tmux_spawn_token ON work_runs
         WHEN NEW.tmux_spawn_token IS NULL BEGIN SELECT RAISE(ABORT, 'clear unavailable'); END;",
        )
        .unwrap();
    assert!(state.confirm_process_torn_down(&exec).await.is_err());
    assert!(db.tmux_identity_for_execution(&exec).unwrap().is_some());
    db.connect()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_identity_clear")
        .unwrap();
    state.confirm_process_torn_down(&exec).await.unwrap();
    assert!(db.tmux_identity_for_execution(&exec).unwrap().is_none());
    assert_eq!(runner.calls().len(), 3, "absent session needs no kill");
}

/// Scripted tmux runner for Grok teardown tests: answers standard
/// `reap_tmux_worker_with` probes and fires an optional callback when
/// `send-keys ... Escape` arrives so tests can simulate Grok emitting
/// `turn_completed` in response to ACP `session/cancel`.
struct ScriptedGrokTmuxLog {
    calls: Vec<Vec<String>>,
    escape_at: Option<std::time::Instant>,
    kill_at: Option<std::time::Instant>,
}

struct ScriptedGrokTmuxRunner {
    token: String,
    log: std::sync::Mutex<ScriptedGrokTmuxLog>,
    on_escape: Option<Box<dyn Fn() + Send + Sync>>,
    pane_dead: bool,
    session_missing: bool,
}

impl ScriptedGrokTmuxRunner {
    fn new(token: impl Into<String>, on_escape: Option<Box<dyn Fn() + Send + Sync>>) -> Arc<Self> {
        Self::with_state(token, on_escape, false, false)
    }

    fn dead_pane(token: impl Into<String>) -> Arc<Self> {
        Self::with_state(token, None, true, false)
    }

    fn missing_session(token: impl Into<String>) -> Arc<Self> {
        Self::with_state(token, None, false, true)
    }

    fn with_state(
        token: impl Into<String>,
        on_escape: Option<Box<dyn Fn() + Send + Sync>>,
        pane_dead: bool,
        session_missing: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            token: token.into(),
            log: std::sync::Mutex::new(ScriptedGrokTmuxLog {
                calls: Vec::new(),
                escape_at: None,
                kill_at: None,
            }),
            on_escape,
            pane_dead,
            session_missing,
        })
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.log.lock().unwrap().calls.clone()
    }

    fn recorded_escape_at(&self) -> Option<std::time::Instant> {
        self.log.lock().unwrap().escape_at
    }

    fn recorded_kill_at(&self) -> Option<std::time::Instant> {
        self.log.lock().unwrap().kill_at
    }
}

#[async_trait::async_trait]
impl boss_tmux::CommandRunner for ScriptedGrokTmuxRunner {
    async fn run(
        &self,
        _program: &std::path::Path,
        args: &[std::ffi::OsString],
        _cwd: Option<&std::path::Path>,
    ) -> std::io::Result<boss_tmux::CommandOutput> {
        let argv: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        self.log.lock().unwrap().calls.push(argv.clone());
        if argv.iter().any(|a| a == "show-environment") {
            if self.session_missing {
                return Ok(failure("can't find session"));
            }
            return Ok(ok(&format!("BOSS_SPAWN_TOKEN={}\n", self.token)));
        }
        if argv.iter().any(|a| a == "#{pane_dead_status}") {
            return Ok(ok("0"));
        }
        if argv.iter().any(|a| a == "#{pane_dead}") {
            return Ok(ok(if self.pane_dead { "1" } else { "0" }));
        }
        if argv.iter().any(|a| a == "send-keys") && argv.last().map(String::as_str) == Some("Escape") {
            self.log.lock().unwrap().escape_at = Some(std::time::Instant::now());
            if let Some(cb) = &self.on_escape {
                cb();
            }
            return Ok(ok(""));
        }
        if argv.iter().any(|a| a == "if-shell" || a == "kill-session") {
            self.log.lock().unwrap().kill_at = Some(std::time::Instant::now());
        }
        Ok(ok(""))
    }

    async fn run_with_stdin(
        &self,
        program: &std::path::Path,
        args: &[std::ffi::OsString],
        cwd: Option<&std::path::Path>,
        _stdin: &[u8],
    ) -> std::io::Result<boss_tmux::CommandOutput> {
        self.run(program, args, cwd).await
    }
}

#[derive(Debug, Default, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
struct RunUsageFields {
    model: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    rounds: Option<i64>,
    usage_unavailable: Option<String>,
}

fn read_run_usage_fields(db: &WorkDb, execution_id: &str) -> RunUsageFields {
    let conn = db.connect().unwrap();
    conn.query_row(
        "SELECT model, input_tokens, output_tokens, cache_read_tokens, rounds, usage_unavailable
         FROM work_runs
         WHERE execution_id = ?1
         ORDER BY created_at DESC, rowid DESC
         LIMIT 1",
        [execution_id],
        |row| {
            Ok(RunUsageFields::builder()
                .maybe_model(row.get::<_, Option<String>>(0)?)
                .maybe_input_tokens(row.get(1)?)
                .maybe_output_tokens(row.get(2)?)
                .maybe_cache_read_tokens(row.get(3)?)
                .maybe_rounds(row.get(4)?)
                .maybe_usage_unavailable(row.get::<_, Option<String>>(5)?)
                .build())
        },
    )
    .unwrap()
}

fn grok_turn_completed_line(prompt_id: &str, stop_reason: &str) -> String {
    serde_json::json!({
        "timestamp": 1785193977,
        "method": "_x.ai/session/update",
        "params": {
            "sessionId": "sess-grok-1",
            "update": {
                "sessionUpdate": "turn_completed",
                "prompt_id": prompt_id,
                "stop_reason": stop_reason,
                "usage": {
                    "inputTokens": 13599,
                    "outputTokens": 33,
                    "totalTokens": 13632,
                    "cachedReadTokens": 13440,
                    "reasoningTokens": 24,
                    "modelCalls": 2,
                    "apiDurationMs": 1348,
                    "costUsdTicks": 45480000,
                    "modelUsage": {
                        "grok-4.6": {
                            "inputTokens": 13599,
                            "outputTokens": 33,
                            "totalTokens": 13632,
                            "cachedReadTokens": 13440,
                            "reasoningTokens": 24,
                            "modelCalls": 2,
                            "apiDurationMs": 1348,
                            "costUsdTicks": 45480000
                        }
                    },
                    "numTurns": 1
                }
            }
        }
    })
    .to_string()
}

/// A scripted Grok session that submits `boss propose done` mid-turn receives
/// `Escape` (ACP `session/cancel`) before reap, emits `turn_completed`, has its
/// usage captured before the session is destroyed, and completes promptly without
/// waiting out the full settle bound.
#[tokio::test]
async fn grok_propose_done_mid_turn_cancels_open_turn_captures_usage_and_reaps_promptly() {
    use boss_protocol::{FrontendRequest, ProposalKind, ProposalState};
    use std::io::Write;
    use std::time::{Duration, Instant};

    let (state, dir) = test_server_state_with_fakes_and_pr_detector(
        Arc::new(FixedPrDetector(boss_engine_completion::PrStatus::Fresh {
            url: "https://github.com/example/repo/pull/42".into(),
        })),
        None,
    );
    state.feature_flags.set("worker_proposals", true).unwrap();
    state.feature_flags.set("run_done_proposals_seam", true).unwrap();
    state.set_grok_teardown_settle_window_for_test(Duration::from_secs(10));

    let db = state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "grok mid-turn propose done");
    let execution_id = create_old_execution(db, &work_item_id);
    db.record_execution_launch_config(&execution_id, "grok", "grok-4.6", None)
        .unwrap();
    db.start_execution_run(
        &execution_id,
        "worker-1",
        "repo-1",
        "lease-1",
        "ws-1",
        dir.path().to_str().unwrap(),
    )
    .unwrap();
    let token = "tok-grok-mid-turn";
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::TEST_SOCKET_PATH, "boss-1-grok", token)
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, token, 4_194_303)
            .unwrap()
    );

    let session_dir = dir.path().join("grok-session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let events_path = session_dir.join("events.jsonl");
    let updates_path = session_dir.join("updates.jsonl");
    std::fs::write(
        &events_path,
        concat!(
            r#"{"type":"turn_started","session_id":"sess-grok-1","turn_number":0}"#,
            "\n",
            r#"{"type":"tool_started","tool_name":"run_terminal_command"}"#,
            "\n",
        ),
    )
    .unwrap();
    std::fs::write(
        &updates_path,
        concat!(
            r#"{"method":"session/update","params":{"sessionId":"sess-grok-1","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"implement fix"}}}}"#,
            "\n",
            r#"{"method":"session/update","params":{"sessionId":"sess-grok-1","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"run_terminal_command","rawInput":{"command":"boss propose done --outcome delivered"}}}}"#,
            "\n",
        ),
    )
    .unwrap();
    db.set_run_transcript_path_if_unset(&execution_id, updates_path.to_str().unwrap())
        .unwrap();

    let pool = state.execution_coordinator.worker_pool();
    assert_eq!(pool.claim_worker(&execution_id, None).await.unwrap(), "worker-1");
    state.worker_registry.register_run_slot(&execution_id, 1);
    let peer_pid = std::process::id() as libc::pid_t;
    state.worker_registry.register(peer_pid, &execution_id);
    state
        .live_worker_states
        .register_spawn(1, &execution_id, "grok", 0, None);

    let updates_for_cb = updates_path.clone();
    let events_for_cb = events_path.clone();
    let runner = ScriptedGrokTmuxRunner::new(
        token,
        Some(Box::new(move || {
            let updates = updates_for_cb.clone();
            let events = events_for_cb.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(25)).await;
                let mut f = std::fs::OpenOptions::new().append(true).open(&updates).unwrap();
                writeln!(f, "{}", grok_turn_completed_line("prompt-1", "cancelled")).unwrap();
                let mut ef = std::fs::OpenOptions::new().append(true).open(&events).unwrap();
                writeln!(ef, r#"{{"type":"turn_ended","outcome":"cancelled"}}"#).unwrap();
            });
        })),
    );
    let tmux =
        boss_tmux::Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH)
            .unwrap();
    state.set_tmux_override_for_test(tmux);

    let app_sink = make_session_sink();
    state
        .register_app_session("session-app-grok".into(), app_sink.clone())
        .await;
    let state_for_app = Arc::clone(&state);
    let app_responder = tokio::spawn(async move {
        while let Some(msg) = app_sink.next().await {
            if let boss_protocol::FrontendEvent::EngineRequest {
                request_id,
                request: boss_protocol::EngineToAppRequest::DetachWorkerPane(_),
            } = msg.payload
            {
                state_for_app
                    .deliver_app_response(
                        "session-app-grok",
                        &request_id,
                        boss_protocol::EngineToAppResponse::DetachWorkerPane {
                            result: Ok(boss_protocol::DetachWorkerPaneResult {}),
                        },
                    )
                    .await;
                break;
            }
        }
    });

    let sink = make_session_sink();
    let ctx = super::Dispatch::builder()
        .server_state(Arc::clone(&state))
        .work_db(Arc::clone(&state.work_db))
        .sink(sink.clone())
        .session_id("session-grok-test")
        .request_id("req-grok-1")
        .peer_pid(peer_pid)
        .recv_instant(Instant::now())
        .decode_ms(0.0)
        .build();
    let req = FrontendRequest::SubmitProposal {
        run_id: execution_id.clone(),
        kind: ProposalKind::RunDone,
        payload: serde_json::json!({"outcome": "delivered", "summary": "Shipped Grok PR"}),
        idempotency_key: None,
    };

    let started = Instant::now();
    let handler = crate::app::proposals::handle_submit_proposal(ctx, req);
    tokio::pin!(handler);
    let (response, handler_finished) = tokio::select! {
        response = sink.next() => (response.expect("handler must respond"), false),
        _ = &mut handler => (sink.next().await.expect("handler must respond"), true),
    };
    sink.complete_response_delivery(response.request_id.as_deref(), true);
    if !handler_finished {
        handler.await;
    }
    let _ = app_responder.await;
    let elapsed = started.elapsed();

    match response.payload {
        boss_protocol::FrontendEvent::ProposalSubmitted { proposal, .. } => {
            assert_eq!(proposal.state, ProposalState::Applied);
        }
        other => panic!("expected ProposalSubmitted, got {other:?}"),
    }

    let escape_at = runner.recorded_escape_at().expect("Escape timestamp recorded");
    let kill_at = runner.recorded_kill_at().expect("kill-session timestamp recorded");
    let settle_elapsed = kill_at.saturating_duration_since(escape_at);
    assert!(
        settle_elapsed < Duration::from_secs(5) && elapsed < Duration::from_secs(10),
        "teardown should finish promptly once turn_completed arrives, not wait out the 10s bound \
         (settle_elapsed={settle_elapsed:?}, total_elapsed={elapsed:?})"
    );
    assert!(
        state.work_db.get_execution(&execution_id).unwrap().status.is_terminal(),
        "execution must finalize synchronously"
    );
    assert!(
        db.tmux_identity_for_execution(&execution_id).unwrap().is_none(),
        "tmux identity must be cleared after reap"
    );
    assert_eq!(pool.idle_count().await, 1, "worker pool slot must be released");

    let calls = runner.calls();
    let esc_pos = calls
        .iter()
        .position(|args| args.iter().any(|a| a == "send-keys") && args.last().map(String::as_str) == Some("Escape"))
        .expect("Escape must be sent to cancel open Grok turn");
    let kill_pos = calls
        .iter()
        .position(|args| args.iter().any(|a| a == "if-shell" || a == "kill-session"))
        .expect("kill-session must still run");
    assert!(esc_pos < kill_pos, "Escape must precede kill-session: {calls:?}");

    assert_eq!(
        read_run_usage_fields(db, &execution_id),
        RunUsageFields::builder()
            .model("grok-4.6")
            .input_tokens(159)
            .output_tokens(33)
            .cache_read_tokens(13440)
            .rounds(2)
            .build(),
        "cancelled Grok turn_completed usage must be captured before reap"
    );
}

/// If Grok never emits `turn_completed` before the bounded settle window
/// expires, teardown still reaps the session and releases the slot promptly,
/// and records the specific `usage_unavailable` reason instead of the generic one.
#[tokio::test]
async fn grok_missing_turn_completed_reaps_after_bound_with_specific_reason() {
    use std::time::Duration;

    let buffer = boss_engine_runtime::test_support::log_capture::install();
    let log_start = buffer.lock().len();

    let (state, dir) = test_server_state();
    // The settle deadline covers driver lookup (spawn_blocking), the first
    // transcript poll, the interrupt send, and the wait for turn_completed.
    // 120ms is enough on an idle host and failed in CI when lookup queued
    // behind other blocking work, so Escape never left. Keep this well under
    // the 5s production bound while leaving room for that lookup.
    let settle_window = Duration::from_millis(500);
    state.set_grok_teardown_settle_window_for_test(settle_window);
    let db = state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "grok silent timeout");
    let execution_id = create_old_execution(db, &work_item_id);
    db.record_execution_launch_config(&execution_id, "grok", "grok-4.6", None)
        .unwrap();
    db.start_execution_run(
        &execution_id,
        "worker-1",
        "repo-1",
        "lease-1",
        "ws-1",
        dir.path().to_str().unwrap(),
    )
    .unwrap();
    let token = "tok-grok-timeout";
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::TEST_SOCKET_PATH, "boss-1-grok-tmo", token)
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, token, 4_194_303)
            .unwrap()
    );

    let session_dir = dir.path().join("grok-session-timeout");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("events.jsonl"),
        r#"{"type":"turn_started","session_id":"sess-grok-2","turn_number":0}"#.to_owned() + "\n",
    )
    .unwrap();
    let updates_path = session_dir.join("updates.jsonl");
    std::fs::write(
        &updates_path,
        r#"{"method":"session/update","params":{"sessionId":"sess-grok-2","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"run_terminal_command"}}}"#.to_owned() + "\n",
    )
    .unwrap();
    db.set_run_transcript_path_if_unset(&execution_id, updates_path.to_str().unwrap())
        .unwrap();

    let pool = state.execution_coordinator.worker_pool();
    assert_eq!(pool.claim_worker(&execution_id, None).await.unwrap(), "worker-1");
    state.worker_registry.register_run_slot(&execution_id, 1);

    let runner = ScriptedGrokTmuxRunner::new(token, None);
    let tmux =
        boss_tmux::Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH)
            .unwrap();
    state.set_tmux_override_for_test(tmux);

    let started = std::time::Instant::now();
    assert_eq!(
        state.release_worker_pane(&execution_id).await,
        PaneReleaseOutcome::Reaped
    );
    let elapsed = started.elapsed();
    let escape_at = runner
        .recorded_escape_at()
        .expect("Escape must be sent before the settle bound expires");
    let kill_at = runner.recorded_kill_at().expect("kill-session timestamp recorded");
    let waited_after_escape = kill_at.saturating_duration_since(escape_at);

    assert!(
        waited_after_escape >= Duration::from_millis(80),
        "teardown should wait out the bounded settle window after cancelling when turn_completed never arrives \
         (waited_after_escape={waited_after_escape:?}, elapsed={elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "teardown must stay far inside the 5s production settle bound even when Grok never responds \
         (elapsed={elapsed:?})"
    );
    assert!(db.tmux_identity_for_execution(&execution_id).unwrap().is_none());
    assert_eq!(pool.idle_count().await, 1, "slot must be released after bound");

    let calls = runner.calls();
    assert!(
        calls
            .iter()
            .any(|args| args.iter().any(|a| a == "send-keys") && args.last().map(String::as_str) == Some("Escape")),
        "Escape must have been sent: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|args| args.iter().any(|a| a == "if-shell" || a == "kill-session")),
        "session must still be killed after bound: {calls:?}"
    );

    assert_eq!(
        read_run_usage_fields(db, &execution_id),
        RunUsageFields::builder()
            .usage_unavailable(crate::app::tmux_teardown::GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON)
            .build(),
        "timed-out Grok teardown must replace the generic usage_unavailable reason"
    );

    let captured = String::from_utf8(buffer.lock()[log_start..].to_vec()).expect("utf8 log capture");
    assert!(
        captured.lines().any(|line| {
            line.contains("WARN")
                && line.contains(&execution_id)
                && line.contains(crate::app::tmux_teardown::GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON)
        }),
        "specific usage_unavailable reason must be logged at WARN; captured:\n{captured}"
    );
}

/// When a Grok turn already finished and emitted `turn_completed` before
/// teardown runs, no Escape is sent and the existing usage is ingested before reap.
#[tokio::test]
async fn grok_already_completed_turn_captures_usage_without_escape() {
    let (state, dir) = test_server_state();
    let db = state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "grok already completed");
    let execution_id = create_old_execution(db, &work_item_id);
    db.record_execution_launch_config(&execution_id, "grok", "grok-4.6", None)
        .unwrap();
    db.start_execution_run(
        &execution_id,
        "worker-1",
        "repo-1",
        "lease-1",
        "ws-1",
        dir.path().to_str().unwrap(),
    )
    .unwrap();
    let token = "tok-grok-done";
    assert!(
        db.record_tmux_spawn_intent_for_execution(
            &execution_id,
            boss_tmux::TEST_SOCKET_PATH,
            "boss-1-grok-done",
            token
        )
        .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, token, 4_194_303)
            .unwrap()
    );

    let session_dir = dir.path().join("grok-session-done");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("events.jsonl"),
        concat!(
            r#"{"type":"turn_started","session_id":"sess-grok-3","turn_number":0}"#,
            "\n",
            r#"{"type":"turn_ended","outcome":"completed"}"#,
            "\n",
        ),
    )
    .unwrap();
    let updates_path = session_dir.join("updates.jsonl");
    std::fs::write(
        &updates_path,
        format!("{}\n", grok_turn_completed_line("prompt-done", "end_turn")),
    )
    .unwrap();
    db.set_run_transcript_path_if_unset(&execution_id, updates_path.to_str().unwrap())
        .unwrap();

    let runner = ScriptedGrokTmuxRunner::new(token, None);
    let tmux =
        boss_tmux::Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH)
            .unwrap();
    state.set_tmux_override_for_test(tmux);

    assert_eq!(
        state.release_worker_pane(&execution_id).await,
        PaneReleaseOutcome::Reaped
    );
    let calls = runner.calls();
    assert!(
        !calls.iter().any(|args| args.iter().any(|a| a == "send-keys")),
        "an already-completed Grok turn must not send Escape: {calls:?}"
    );
    assert_eq!(
        read_run_usage_fields(db, &execution_id),
        RunUsageFields::builder()
            .model("grok-4.6")
            .input_tokens(159)
            .output_tokens(33)
            .cache_read_tokens(13440)
            .rounds(2)
            .build(),
    );
}

fn seed_completed_grok_run(
    state: &crate::app::ServerState,
    dir: &tempfile::TempDir,
    session_name: &str,
    token: &str,
) -> String {
    let db = state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, session_name);
    let execution_id = create_old_execution(db, &work_item_id);
    db.record_execution_launch_config(&execution_id, "grok", "grok-4.6", None)
        .unwrap();
    db.start_execution_run(
        &execution_id,
        "worker-1",
        "repo-1",
        "lease-1",
        "ws-1",
        dir.path().to_str().unwrap(),
    )
    .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::TEST_SOCKET_PATH, session_name, token)
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, token, 4_194_303)
            .unwrap()
    );

    let session_dir = dir.path().join(format!("{session_name}-session"));
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("events.jsonl"),
        concat!(
            r#"{"type":"turn_started","session_id":"sess-grok-3","turn_number":0}"#,
            "\n",
            r#"{"type":"turn_ended","outcome":"completed"}"#,
            "\n",
        ),
    )
    .unwrap();
    let updates_path = session_dir.join("updates.jsonl");
    std::fs::write(
        &updates_path,
        format!("{}\n", grok_turn_completed_line("prompt-done", "end_turn")),
    )
    .unwrap();
    db.set_run_transcript_path_if_unset(&execution_id, updates_path.to_str().unwrap())
        .unwrap();
    execution_id
}

fn expected_completed_grok_usage() -> RunUsageFields {
    RunUsageFields::builder()
        .model("grok-4.6")
        .input_tokens(159)
        .output_tokens(33)
        .cache_read_tokens(13440)
        .rounds(2)
        .build()
}

/// A Grok pane already dead (`#{pane_dead}=1`) still captures `turn_completed`
/// usage, never sends an interrupt, and finishes the reap.
#[tokio::test]
async fn grok_dead_pane_captures_usage_without_escape() {
    let (state, dir) = test_server_state();
    let execution_id = seed_completed_grok_run(&state, &dir, "boss-1-grok-dead", "tok-grok-dead");
    let runner = ScriptedGrokTmuxRunner::dead_pane("tok-grok-dead");
    let tmux =
        boss_tmux::Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH)
            .unwrap();
    state.set_tmux_override_for_test(tmux);

    assert_eq!(
        state.release_worker_pane(&execution_id).await,
        PaneReleaseOutcome::Reaped
    );
    let calls = runner.calls();
    assert!(
        !calls.iter().any(|args| args.iter().any(|a| a == "send-keys")),
        "a dead Grok pane must not send Escape: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|args| args.iter().any(|a| a == "if-shell" || a == "kill-session")),
        "dead-pane Grok teardown must still kill the session: {calls:?}"
    );
    assert_eq!(
        read_run_usage_fields(state.work_db.as_ref(), &execution_id),
        expected_completed_grok_usage(),
    );
}

/// A Grok session already gone still captures `turn_completed` usage that
/// landed before exit, never sends an interrupt, and finishes the reap.
#[tokio::test]
async fn grok_missing_session_captures_usage_without_escape() {
    let (state, dir) = test_server_state();
    let execution_id = seed_completed_grok_run(&state, &dir, "boss-1-grok-gone", "tok-grok-gone");
    let runner = ScriptedGrokTmuxRunner::missing_session("tok-grok-gone");
    let tmux =
        boss_tmux::Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH)
            .unwrap();
    state.set_tmux_override_for_test(tmux);

    assert_eq!(
        state.release_worker_pane(&execution_id).await,
        PaneReleaseOutcome::Reaped
    );
    let calls = runner.calls();
    assert!(
        !calls.iter().any(|args| args.iter().any(|a| a == "send-keys")),
        "a missing Grok session must not send Escape: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|args| args.iter().any(|a| a == "if-shell" || a == "kill-session")),
        "a missing Grok session must not issue kill-session: {calls:?}"
    );
    assert_eq!(
        read_run_usage_fields(state.work_db.as_ref(), &execution_id),
        expected_completed_grok_usage(),
    );
}

/// Claude and Codex teardowns must remain completely unchanged (no Escape or settle wait).
#[tokio::test]
async fn claude_and_codex_teardown_are_unchanged() {
    for (driver, model) in [("claude", "claude-opus"), ("codex", "gpt-5.6-terra")] {
        let (server_state, _dir) = test_server_state();
        let db = server_state.work_db.as_ref();
        let product_id = create_product(db);
        let work_item_id = create_active_chore(db, &product_id, driver);
        let execution_id = seed_tmux_run(db, &work_item_id, "boss-1-other", "tok-other", 4_194_303);
        db.record_execution_launch_config(&execution_id, driver, model, None)
            .unwrap();
        let identity = read_back_identity(db, &execution_id);

        let (tmux, runner) = fake_tmux([
            ok("BOSS_SPAWN_TOKEN=tok-other\n"),
            ok("0"),
            ok("BOSS_SPAWN_TOKEN=tok-other\n"),
            ok(""),
            ok(""),
        ]);
        let outcome = server_state
            .reap_tmux_worker_with(&tmux, &execution_id, &identity)
            .await;
        assert_eq!(outcome, TmuxTeardownOutcome::Reaped);
        assert!(
            !runner.calls().iter().any(|args| args.iter().any(|a| a == "send-keys")),
            "{driver} teardown must never send Escape"
        );
    }
}

/// A stalled DB write lock during Grok teardown must not hold back
/// `kill_session_verified` beyond the advertised settle bound. Contention is
/// introduced at the settle/ingest step on a genuine second SQLite connection,
/// after driver lookup has already succeeded.
#[tokio::test]
async fn grok_teardown_stalled_db_does_not_hold_back_kill() {
    use std::time::{Duration, Instant};

    let (state, dir) = test_server_state();
    state.set_grok_teardown_settle_window_for_test(Duration::from_millis(200));
    let db = state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "grok stalled db");
    let execution_id = seed_tmux_run(db, &work_item_id, "boss-1-grok-stalled", "tok-stalled", 4_194_303);
    db.record_execution_launch_config(&execution_id, "grok", "grok-4.6", None)
        .unwrap();
    let identity = read_back_identity(db, &execution_id);

    let session_dir = dir.path().join("grok-stalled");
    std::fs::create_dir_all(&session_dir).unwrap();
    let events_path = session_dir.join("events.jsonl");
    let updates_path = session_dir.join("updates.jsonl");
    std::fs::write(
        &events_path,
        concat!(
            r#"{"type":"turn_started","session_id":"sess-grok-stalled","turn_number":0}"#,
            "\n"
        ),
    )
    .unwrap();
    std::fs::write(
        &updates_path,
        concat!(
            r#"{"method":"session/update","params":{"sessionId":"sess-grok-stalled","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"run_terminal_command"}}}"#,
            "\n"
        ),
    )
    .unwrap();
    db.set_run_transcript_path_if_unset(&execution_id, updates_path.to_str().unwrap())
        .unwrap();

    let work_db = Arc::clone(&state.work_db);
    let (lock_now, lock_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (unlock_now, unlock_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (unlocked_tx, unlocked_rx) = std::sync::mpsc::sync_channel::<()>(1);
    std::thread::spawn(move || {
        let conn = work_db.connect_new().unwrap();
        if lock_rx.recv().is_err() {
            return;
        }
        conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        // Hold long enough for settle/ingest to observe SQLITE_BUSY, then
        // release so the detached persist and identity-clear can finish.
        let _ = unlock_rx.recv_timeout(Duration::from_millis(400));
        let _ = conn.execute_batch("ROLLBACK;");
        let _ = unlocked_tx.send(());
    });
    let runner = ScriptedGrokTmuxRunner::new(
        "tok-stalled",
        Some(Box::new(move || {
            // Signal without waiting: a blocking handshake here would stall
            // the current-thread runtime inside send-keys and prevent kill.
            let _ = lock_now.send(());
        })),
    );
    let tmux =
        boss_tmux::Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", runner.clone(), boss_tmux::TEST_SOCKET_PATH)
            .unwrap();

    let started = Instant::now();
    let _outcome = state.reap_tmux_worker_with(&tmux, &execution_id, &identity).await;

    assert!(
        runner.recorded_escape_at().is_some(),
        "Grok settle must send Escape even when ingest later stalls: {:?}",
        runner.calls()
    );
    let kill_at = runner.recorded_kill_at().expect("kill-session must still run");
    let kill_elapsed = kill_at.saturating_duration_since(started);
    assert!(
        runner
            .calls()
            .iter()
            .any(|args| args.iter().any(|a| a.contains("kill-session"))),
        "kill-session must still run: {:?}",
        runner.calls()
    );
    assert!(
        kill_elapsed < Duration::from_millis(1500),
        "stalled DB must not delay kill-session past the settle bound (kill_elapsed={kill_elapsed:?})"
    );

    let _ = unlock_now.send(());
    let _ = unlocked_rx.recv();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let fields = read_run_usage_fields(state.work_db.as_ref(), &execution_id);
        if fields.usage_unavailable.as_deref()
            == Some(crate::app::tmux_teardown::GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON)
            || fields.input_tokens.is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "eventual Grok usage or usage_unavailable must persist after the lock is released"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
