//! Coverage for [`ServerState::shutdown_workers`]: app-hosted workers are
//! preserved for rollback/drain; tmux workers survive with identity
//! columns intact so boot-time adoption can re-attach them.

use super::tmux_stub::{fake_tmux, ok};
use super::*;
use crate::live_worker_state::LiveSpawnRouting;
use crate::test_support::*;

const SESSION: &str = "boss-1-shutdown";
const TOKEN: &str = "tok-shutdown";

fn seed_tmux_hosted_execution(db: &WorkDb, work_item_id: &str, pane_pid: i64) -> String {
    let execution_id = create_old_execution(db, work_item_id);
    db.start_execution_run_on_host_with_tmux_hosting(
        &execution_id,
        "worker-1",
        "repo-1",
        "lease-1",
        "ws-1",
        "/tmp/ws",
        "local",
        true,
    )
    .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::SERVER_LABEL, SESSION, TOKEN)
            .unwrap(),
        "intent write must find the just-started run row",
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, TOKEN, pane_pid)
            .unwrap(),
        "creation write must find the intent row it just wrote",
    );
    execution_id
}

fn register_tmux_live_worker(server_state: &ServerState, execution_id: &str, slot_id: u8, shell_pid: i32) {
    server_state
        .worker_registry
        .register_tmux_run_slot(execution_id, slot_id, SESSION);
    server_state.live_worker_states.register_spawn_with_capabilities(
        slot_id,
        execution_id.to_owned(),
        "claude-opus-4-7",
        shell_pid,
        None,
        true,
        LiveSpawnRouting::new_with_hosting(Some("main".into()), "chore_implementation", true),
    );
}

fn assert_still_adoptable(db: &WorkDb, execution_id: &str) {
    let identity = db
        .tmux_identity_for_execution(execution_id)
        .unwrap()
        .expect("shutdown must not null tmux identity columns for a surviving session");
    assert_eq!(identity.session_name, SESSION);
    let adoptable = db.list_adoptable_tmux_runs().unwrap();
    assert!(
        adoptable.iter().any(|handle| handle.execution_id == execution_id),
        "surviving tmux identity must still match TMUX_RUN_ADOPTABLE_PREDICATE, got {adoptable:?}"
    );
}

/// Historical app-owned workers remain available for rollback/drain.
#[tokio::test]
async fn shutdown_workers_preserves_a_historical_worker_without_identity() {
    let (server_state, _dir) = test_server_state();
    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    server_state.worker_registry.register_run_slot("run-app", 1);
    server_state
        .live_worker_states
        .register_spawn(1, "run-app", "claude-opus-4-7", pid, None);
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;
    server_state.shutdown_workers().await;
    assert_eq!(server_state.worker_registry.slot_for_run("run-app"), Some(1));
    assert!(
        child.try_wait().unwrap().is_none(),
        "shutdown cannot kill a worker without verified ownership"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), app_sink.next())
            .await
            .is_err()
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

/// A tmux-hosted worker must survive engine shutdown: no pane release, no
/// session kill, no shell signal, and identity columns left in the state
/// boot-time adoption already accepts.
#[tokio::test]
async fn shutdown_workers_leaves_tmux_hosted_session_and_shell_intact() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = seed_tmux_hosted_execution(db, &work_item_id, i64::from(pid));
    register_tmux_live_worker(&server_state, &execution_id, 3, pid);

    // Empty script: any tmux invocation (show-environment / kill-session)
    // panics, which is the assertion that shutdown did not reap.
    let (tmux, runner) = fake_tmux([]);
    server_state.set_tmux_override_for_test(tmux);

    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;

    server_state.shutdown_workers().await;

    assert!(
        runner.calls().is_empty(),
        "shutdown must not talk to tmux for a hosted worker, got {:?}",
        runner.calls(),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), app_sink.next())
            .await
            .ok()
            .flatten()
            .is_none(),
        "shutdown must not send DetachWorkerPane for a tmux-hosted worker",
    );
    assert_eq!(
        server_state.worker_registry.slot_for_run(&execution_id),
        Some(3),
        "shutdown must not take the tmux-hosted slot mapping",
    );
    assert_eq!(server_state.live_worker_states.snapshot().len(), 1);
    assert_still_adoptable(db, &execution_id);

    assert!(
        child.try_wait().expect("try_wait on child").is_none(),
        "the tmux-hosted worker's shell must NOT have been signalled",
    );
    child.kill().expect("kill the test child");
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;
}

/// Mixed shutdown preserves both tmux and historical workers.
#[tokio::test]
async fn shutdown_workers_preserves_tmux_and_historical_workers() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut tmux_child = spawn_group_leader_sleeper();
    let tmux_pid = tmux_child.id() as i32;
    let execution_id = seed_tmux_hosted_execution(db, &work_item_id, i64::from(tmux_pid));
    register_tmux_live_worker(&server_state, &execution_id, 3, tmux_pid);

    let mut app_child = spawn_group_leader_sleeper();
    let app_pid = app_child.id() as i32;
    server_state.worker_registry.register_run_slot("run-app", 1);
    server_state
        .live_worker_states
        .register_spawn(1, "run-app", "claude-opus-4-7", app_pid, None);

    let (tmux, runner) = fake_tmux([]);
    server_state.set_tmux_override_for_test(tmux);

    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;

    server_state.shutdown_workers().await;

    assert!(
        tokio::time::timeout(Duration::from_millis(20), app_sink.next())
            .await
            .is_err()
    );
    assert_eq!(server_state.worker_registry.slot_for_run("run-app"), Some(1));
    assert!(runner.calls().is_empty(), "tmux-hosted session must not be reaped");
    assert_still_adoptable(db, &execution_id);

    assert!(
        app_child.try_wait().unwrap().is_none(),
        "historical worker must survive for rollback/drain"
    );
    app_child.kill().unwrap();
    app_child.wait().unwrap();

    assert!(
        tmux_child.try_wait().expect("try_wait on tmux child").is_none(),
        "tmux-hosted shell must survive",
    );
    tmux_child.kill().expect("kill the tmux test child");
    let _ = tokio::task::spawn_blocking(move || tmux_child.wait()).await;
}

/// Durable `tmux_hosted = 1` is enough even when the in-memory live-state
/// stamp is missing — the fallback if a registry entry was recorded on the
/// legacy app-hosted path.
#[tokio::test]
async fn shutdown_workers_survives_when_only_the_durable_tmux_hosted_bit_is_set() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = seed_tmux_hosted_execution(db, &work_item_id, i64::from(pid));
    // Deliberately the legacy app-hosted registry + unstamped live state.
    server_state.worker_registry.register_run_slot(&execution_id, 2);
    server_state
        .live_worker_states
        .register_spawn(2, execution_id.clone(), "claude-opus-4-7", pid, None);

    let (tmux, runner) = fake_tmux([]);
    server_state.set_tmux_override_for_test(tmux);

    server_state.shutdown_workers().await;

    assert!(runner.calls().is_empty());
    assert_still_adoptable(db, &execution_id);
    assert!(
        child.try_wait().expect("try_wait on child").is_none(),
        "durable tmux_hosted=1 must skip signalling even without a live-state stamp",
    );
    child.kill().expect("kill the test child");
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;
}

/// Genuine worker termination still reaps the tmux session. Shutdown is the
/// only path that skips teardown; `release_worker_pane` must keep destroying
/// the session and clearing identity.
#[tokio::test]
async fn release_worker_pane_still_reaps_a_tmux_hosted_worker() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");

    let mut child = spawn_group_leader_sleeper();
    let pid = child.id() as i32;
    let execution_id = seed_tmux_hosted_execution(db, &work_item_id, i64::from(pid));
    register_tmux_live_worker(&server_state, &execution_id, 4, pid);

    let (tmux, runner) = fake_tmux([
        ok(&format!("BOSS_SPAWN_TOKEN={TOKEN}\n")),
        ok("0"), // Pane-dead observation before the kill's token recheck.
        ok(&format!("BOSS_SPAWN_TOKEN={TOKEN}\n")),
        ok(""),
    ]);
    server_state.set_tmux_override_for_test(tmux);

    let outcome = server_state.release_worker_pane(&execution_id).await;
    assert_eq!(outcome, PaneReleaseOutcome::Reaped);
    let observation = db
        .tmux_pane_observation_for_execution(&execution_id)
        .unwrap()
        .expect("release must retain the observation taken before signalling the pane");
    assert_eq!(observation.kind, crate::work::TmuxPaneObservationKind::Alive);
    assert_eq!(observation.pane_dead, Some(false));
    assert!(
        db.tmux_identity_for_execution(&execution_id).unwrap().is_none(),
        "genuine termination must still clear tmux identity columns",
    );
    assert!(
        db.list_adoptable_tmux_runs()
            .unwrap()
            .iter()
            .all(|handle| handle.execution_id != execution_id),
        "a reaped session must not remain adoptable",
    );
    assert!(
        runner
            .calls()
            .iter()
            .any(|call| call.windows(2).any(|pair| pair == ["kill-session", "-t"])),
        "genuine termination must issue kill-session, got {:?}",
        runner.calls(),
    );

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "genuine termination must still signal the tmux pane pid",
    );
}
