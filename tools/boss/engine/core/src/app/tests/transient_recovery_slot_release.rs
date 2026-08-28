//! Transient-recovery must tear the pane down through
//! [`crate::app::ServerState::release_worker_pane`] and only free the
//! pool claim once that teardown is confirmed.
//!
//! The previous `release_slot` called `release_worker_and_kick` without
//! asking the app to drop the pane, so the next dispatch claimed a slot
//! the app still hosted and died `SlotBusy`. That is a different path
//! from the ack-gated `release_worker_pane` itself — this sweep never
//! called it.

use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;

use super::*;
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::test_support::*;
use crate::transient_error::RecoveryPolicy;
use crate::transient_recovery::{NoopWorkerNudger, RecoveryContext, run_one_pass};
use boss_protocol::RequestExecutionInput;

const SOCKET_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: The socket connection was closed unexpectedly."}]}}"#;
const AUTH_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: 401 authentication_error: invalid x-api-key"}]}}"#;

struct SeededWorker {
    server: Arc<ServerState>,
    /// Keep the on-disk DB alive for as long as `server` is used.
    _dir: tempfile::TempDir,
    exec_id: String,
    sink: Arc<SessionSink>,
}

/// A running, past-grace worker on slot 1 with a trailing transcript
/// error, a pool claim, a worker-registry mapping, and an Idle live-state
/// entry — the shape transient-recovery inspects.
async fn seed_stalled_worker(error_line: &str, register_session: bool) -> SeededWorker {
    let (server, dir) = test_server_state();
    let db = server.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "stalled chore");

    let transcript_path = dir.path().join("t.jsonl");
    {
        let mut f = std::fs::File::create(&transcript_path).unwrap();
        writeln!(f, "{error_line}").unwrap();
    }

    let execution = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .preferred_workspace_id("mono-agent-007")
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &execution.id,
        "worker-1",
        "repo-1",
        "lease-1",
        "mono-agent-007",
        "/tmp/mono-agent-007",
    )
    .unwrap();
    db.set_run_transcript_path_if_unset(&execution.id, transcript_path.to_str().unwrap())
        .unwrap();
    let old_started = boss_engine_utils::epoch_time::now_epoch_secs().saturating_sub(600);
    db.force_started_at_for_test(&execution.id, old_started).unwrap();

    let pool = server.execution_coordinator.worker_pool();
    let claimed = pool
        .claim_worker(&execution.id, None)
        .await
        .expect("test pool has a free slot");
    assert_eq!(claimed, "worker-1");
    register_idle_worker(&server, &execution.id, 1);

    let sink = make_session_sink();
    if register_session {
        server.register_app_session("session-app".into(), sink.clone()).await;
    }

    SeededWorker {
        exec_id: execution.id,
        server,
        _dir: dir,
        sink,
    }
}

/// Like [`seed_stalled_worker`], but skips `worker_registry.register_run_slot`
/// so `release_worker_pane` has no run→slot mapping for this run and falls
/// through to `reap_untracked_worker_process`, which — with no durable
/// shell pid recorded — answers `NoLiveWorker` without ever touching
/// `live_worker_states` or the pool claim itself.
async fn seed_stalled_worker_without_slot_mapping(error_line: &str) -> SeededWorker {
    use boss_protocol::{WorkerActivity, WorkerEvent};

    let (server, dir) = test_server_state();
    let db = server.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "stalled chore");

    let transcript_path = dir.path().join("t.jsonl");
    {
        let mut f = std::fs::File::create(&transcript_path).unwrap();
        writeln!(f, "{error_line}").unwrap();
    }

    let execution = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .preferred_workspace_id("mono-agent-007")
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &execution.id,
        "worker-1",
        "repo-1",
        "lease-1",
        "mono-agent-007",
        "/tmp/mono-agent-007",
    )
    .unwrap();
    db.set_run_transcript_path_if_unset(&execution.id, transcript_path.to_str().unwrap())
        .unwrap();
    let old_started = boss_engine_utils::epoch_time::now_epoch_secs().saturating_sub(600);
    db.force_started_at_for_test(&execution.id, old_started).unwrap();

    let pool = server.execution_coordinator.worker_pool();
    let claimed = pool
        .claim_worker(&execution.id, None)
        .await
        .expect("test pool has a free slot");
    assert_eq!(claimed, "worker-1");

    // Live-state entry only — deliberately no `worker_registry.register_run_slot`
    // call, so this run has a live-state entry but no run→slot mapping.
    server
        .live_worker_states
        .register_spawn(1, execution.id.clone(), "claude-opus-4-7", 0, None);
    server.live_worker_states.apply_event(
        1,
        &WorkerEvent::Stop {
            session_id: "test-sess".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );
    assert_eq!(
        server.live_worker_states.get(1).unwrap().activity,
        WorkerActivity::Idle,
        "seed precondition: slot must be Idle",
    );

    SeededWorker {
        exec_id: execution.id,
        server,
        _dir: dir,
        sink: make_session_sink(),
    }
}

async fn run_recovery(server: &Arc<ServerState>) -> crate::transient_recovery::TransientRecoveryOutcome {
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let policy = RecoveryPolicy::default();
    let cx = RecoveryContext {
        work_db: server.work_db.as_ref(),
        live_states: &server.live_worker_states,
        coordinator: server.execution_coordinator.clone(),
        dispatch_events: sink.as_ref(),
        policy: &policy,
        nudger: &NoopWorkerNudger,
        reaper: server.as_ref(),
    };
    run_one_pass(
        &cx,
        &mut HashSet::new(),
        boss_engine_utils::epoch_time::now_epoch_secs(),
    )
    .await
}

async fn confirm_next_pane_release(server: Arc<ServerState>, sink: Arc<SessionSink>) {
    let env = sink.next().await.expect("pane-release EngineRequest enqueued");
    let FrontendEvent::EngineRequest {
        request_id, request, ..
    } = env.payload
    else {
        panic!("expected an EngineRequest for the pane release, got {:?}", env.payload);
    };
    assert!(
        matches!(request, EngineToAppRequest::ReleaseWorkerPane(_)),
        "transient-recovery must ask the app to tear the pane down, got {request:?}",
    );
    server
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ReleaseWorkerPane {
                result: Ok(crate::protocol::ReleaseWorkerPaneResult {}),
            },
        )
        .await;
}

/// Regression: an unconfirmed orphan+respawn must **hold** the pool claim.
/// Against the previous `release_slot` this fails — it freed the slot
/// unconditionally, so the next `claim_worker` succeeded and the subsequent
/// `SpawnWorkerPane` died `SlotBusy`.
#[tokio::test(start_paused = true)]
async fn unconfirmed_orphan_respawn_holds_the_pool_claim() {
    let seeded = seed_stalled_worker(SOCKET_ERROR_LINE, true).await;
    let pool = seeded.server.execution_coordinator.worker_pool();

    let outcome = run_recovery(&seeded.server).await;
    assert_eq!(outcome.resumed, 1, "socket error with no nudger must orphan+respawn");
    assert_eq!(outcome.escalated, 0);

    assert_eq!(
        pool.idle_count().await,
        0,
        "unconfirmed pane teardown must not advertise the slot as free",
    );
    assert!(
        pool.claimed_execution_ids().await.contains(&seeded.exec_id),
        "the orphaned execution must still hold the claim",
    );
    assert!(
        pool.claim_worker("exec-fresh", None).await.is_none(),
        "next dispatch must not obtain the still-hosted slot (that is the SlotBusy collision)",
    );
}

/// Same hold for the escalate-to-human call site.
#[tokio::test(start_paused = true)]
async fn unconfirmed_escalate_holds_the_pool_claim() {
    let seeded = seed_stalled_worker(AUTH_ERROR_LINE, true).await;
    let pool = seeded.server.execution_coordinator.worker_pool();

    let outcome = run_recovery(&seeded.server).await;
    assert_eq!(outcome.escalated, 1, "auth error must escalate, not resume");
    assert_eq!(outcome.resumed, 0);

    assert_eq!(pool.idle_count().await, 0);
    assert!(pool.claimed_execution_ids().await.contains(&seeded.exec_id));
    assert!(
        pool.claim_worker("exec-fresh", None).await.is_none(),
        "next dispatch must not obtain the still-hosted slot",
    );
}

/// An absent app session is **not** confirmation the pane is gone. The
/// process (and the pane) can outlive the websocket; treating
/// `NotRegistered` as a free signal reproduces the same SlotBusy poison.
#[tokio::test(start_paused = true)]
async fn absent_app_session_is_unconfirmed_and_holds_the_claim() {
    let seeded = seed_stalled_worker(SOCKET_ERROR_LINE, false).await;
    let pool = seeded.server.execution_coordinator.worker_pool();

    let outcome = run_recovery(&seeded.server).await;
    assert_eq!(outcome.resumed, 1);
    assert_eq!(
        pool.idle_count().await,
        0,
        "no app session must not be treated as confirmation the pane is gone",
    );
    assert!(pool.claim_worker("exec-fresh", None).await.is_none());
}

#[tokio::test]
async fn confirmed_orphan_respawn_frees_the_slot_promptly() {
    let seeded = seed_stalled_worker(SOCKET_ERROR_LINE, true).await;
    let pool = seeded.server.execution_coordinator.worker_pool();
    let ack = tokio::spawn(confirm_next_pane_release(seeded.server.clone(), seeded.sink.clone()));

    let outcome = run_recovery(&seeded.server).await;
    ack.await.expect("ack task");
    assert_eq!(outcome.resumed, 1);

    assert_eq!(
        pool.idle_count().await,
        1,
        "a confirmed pane teardown must free the slot promptly",
    );
    let reclaimed = pool
        .claim_worker("exec-fresh", None)
        .await
        .expect("slot must be free after confirmed teardown");
    assert_eq!(reclaimed, "worker-1");
}

/// Regression: when `release_worker_pane` finds no run→slot mapping for the
/// candidate (`PaneReleaseOutcome::NoLiveWorker`), the reaper must still
/// drop the orphaned live-state entry itself. Left in place, that entry is
/// exactly the shape `pool_claim_sweep` skips by design — a live-state
/// entry still backing the claim — so a leftover entry here would leak the
/// pool claim forever instead of letting the sweep reconcile it.
#[tokio::test]
async fn no_slot_mapping_drops_the_orphaned_live_state_entry() {
    let seeded = seed_stalled_worker_without_slot_mapping(SOCKET_ERROR_LINE).await;

    let outcome = run_recovery(&seeded.server).await;
    assert_eq!(outcome.resumed, 1, "socket error with no nudger must orphan+respawn");

    assert!(
        seeded.server.live_worker_states.get(1).is_none(),
        "a NoLiveWorker pane-release answer must not leave an orphaned live-state entry behind",
    );
}

#[tokio::test]
async fn confirmed_escalate_frees_the_slot_promptly() {
    let seeded = seed_stalled_worker(AUTH_ERROR_LINE, true).await;
    let pool = seeded.server.execution_coordinator.worker_pool();
    let ack = tokio::spawn(confirm_next_pane_release(seeded.server.clone(), seeded.sink.clone()));

    let outcome = run_recovery(&seeded.server).await;
    ack.await.expect("ack task");
    assert_eq!(outcome.escalated, 1);

    assert_eq!(pool.idle_count().await, 1);
    let reclaimed = pool
        .claim_worker("exec-fresh", None)
        .await
        .expect("slot must be free after confirmed teardown");
    assert_eq!(reclaimed, "worker-1");
}
