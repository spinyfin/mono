//! End-to-end lifecycle of an `answer_agent` execution: from the reply it
//! posts mid-session, through the hook event its driver's turn boundary emits,
//! to the execution going terminal and its worker slot coming back.
//!
//! This is the seam the strand actually lived in. The finalizer
//! (`completion::finalize_answer_agent`) was always correct and always tested —
//! but its trigger arrives over the events socket, and an `answer_agent`
//! execution binds to a `work_comments.id` rather than a task, so the socket's
//! driver resolution found nothing and dropped the event before the completion
//! subsystem ever saw it. Per-layer tests all passed while the run held slot 8
//! for 71 minutes. These tests drive the layers together.

use super::*;

use crate::app::worker_events::dispatch_worker_event_fanout;
use crate::protocol::WorkerEvent;

/// Seed a `question` comment mid-answer with its `running` run and a
/// pane-parked (`waiting_human`) `answer_agent` execution occupying `slot_id`
/// — the exact state the observed strand was found in. Returns
/// `(comment_id, run_id, execution_id)`.
fn seed_parked_answer_agent(server_state: &Arc<ServerState>, slot_id: u8) -> (String, String, String) {
    let work_db = &server_state.work_db;
    let product = crate::test_support::create_test_product_with_repo(
        work_db,
        "answer-agent-product",
        Some("git@github.com:spinyfin/mono.git"),
    );
    let comment = work_db
        .create_comment(boss_protocol::CreateCommentInput {
            artifact_kind: "pr_doc".to_owned(),
            artifact_id: "pr_doc:git@github.com:spinyfin/mono.git:main:docs/design.md".to_owned(),
            doc_version: "v1".to_owned(),
            anchor: boss_protocol::CommentAnchor {
                exact: "the quoted text".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "why does this retry three times?".to_owned(),
            author: "operator".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    work_db
        .set_comment_intent(&comment.id, boss_protocol::INTENT_QUESTION, 0.9)
        .unwrap();
    work_db.transition_comment_to_answering(&comment.id).unwrap();
    let run = work_db
        .create_answer_agent_run(
            &comment.id,
            &comment.artifact_kind,
            &comment.artifact_id,
            &comment.doc_version,
            0,
        )
        .unwrap();
    let execution = work_db
        .create_answer_agent_execution(&comment.id, &product.repo_remote_url.clone().unwrap())
        .unwrap();
    let workspace = std::env::temp_dir();
    let (execution, spawn_run) = work_db
        .start_execution_run(
            &execution.id,
            &crate::coordinator::worker_id_for_slot(slot_id),
            "mono",
            "lease-1",
            "mono-agent-038",
            workspace.to_str().unwrap(),
        )
        .unwrap();
    crate::test_support::finish_run_worker_pane_alive(work_db, &execution.id, &spawn_run.id, Some("spawned"));
    register_working_worker(server_state, &execution.id, slot_id);
    (comment.id, run.id, execution.id)
}

/// The reply the agent posts with `boss comment reply`, as
/// `CommentsPostAnswer` leaves the DB.
fn post_the_reply(server_state: &Arc<ServerState>, comment_id: &str, run_id: &str) {
    let work_db = &server_state.work_db;
    work_db
        .complete_answer_agent_run(
            run_id,
            boss_protocol::ANSWER_AGENT_RUN_STATUS_REPLIED,
            Some("here you go"),
            None,
        )
        .unwrap();
    work_db
        .create_comment_thread_entry(
            comment_id,
            boss_protocol::THREAD_ENTRY_KIND_ANSWER,
            "engine",
            "here you go",
            None,
            None,
        )
        .unwrap();
    work_db.transition_comment_to_answered(comment_id).unwrap();
}

/// Deliver a Claude `Stop` hook payload for `execution_id` the way the
/// `boss-event` shim does — a JSON write to the engine's events socket — and
/// return the decoded event. Panics (failing the test) if the connection is
/// rejected, which is exactly how the strand began: an unresolvable driver
/// meant `handle_connection` returned `UnresolvedDriver` and the accept loop
/// logged a warning instead of fanning anything out.
async fn stop_hook_through_the_events_socket(
    server_state: &Arc<ServerState>,
    execution_id: &str,
) -> crate::events_socket::IncomingHookEvent {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sock");
    let listener = crate::events_socket::bind_events_socket(&path).unwrap();
    let payload = format!(
        r#"{{"hook_event_name":"Stop","session_id":"answer-sess-1","stop_hook_active":false,"_boss_run_id":"{execution_id}"}}"#
    );
    let path_owned = path.clone();
    let client = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut stream = std::os::unix::net::UnixStream::connect(&path_owned).unwrap();
        stream.write_all(payload.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let (stream, _) = listener.accept().await.unwrap();
    let incoming = crate::events_socket::handle_connection(
        stream,
        &crate::driver::DriverRegistry::default(),
        &server_state.work_db,
    )
    .await
    .expect("an answer-agent Stop must survive per-connection driver resolution");
    client.await.unwrap();
    incoming
}

/// The whole delivered-answer path, driven through the production fan-out:
/// the reply lands, the worker's turn ends, and the execution must be terminal
/// with its slot back — with no human in the loop and no timeout involved.
#[tokio::test]
async fn a_delivered_answer_completes_the_execution_and_frees_its_slot_on_the_turn_boundary() {
    let (server_state, _dir) = test_server_state();
    let slot_id = 8;
    let (comment_id, run_id, execution_id) = seed_parked_answer_agent(&server_state, slot_id);
    post_the_reply(&server_state, &comment_id, &run_id);

    // Before: the row says live and the pool says claimed — they agree.
    assert_eq!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .as_str(),
        "running",
    );
    assert!(server_state.live_worker_states.get(slot_id).is_some());

    // The driver's turn boundary, carried the whole production way: the shim's
    // JSON over the events socket, through per-connection driver resolution,
    // into the fan-out. Going through `handle_connection` rather than
    // synthesising an `IncomingHookEvent` is the point — resolution is where
    // this event used to be dropped.
    let stop = stop_hook_through_the_events_socket(&server_state, &execution_id).await;
    dispatch_worker_event_fanout(&server_state, &stop).await;

    // After: both sides moved, together.
    let execution = server_state.work_db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status.as_str(),
        "completed",
        "the answer landed and the turn ended — the execution must not stay parked",
    );
    assert!(
        execution.cube_lease_id.is_none(),
        "the cube lease must be released with the row, not left held",
    );
    assert!(
        server_state.live_worker_states.get(slot_id).is_none(),
        "the worker slot must come back; a terminal row with a claimed slot is the state \
         the row/pool agreement rule forbids",
    );

    // The reply itself is untouched — this is a lifecycle fix, not a rewrite
    // of what the agent produced.
    let run = server_state.work_db.get_answer_agent_run(&run_id).unwrap().unwrap();
    assert_eq!(run.status, boss_protocol::ANSWER_AGENT_RUN_STATUS_REPLIED);
    assert_eq!(
        server_state.work_db.get_comment(&comment_id).unwrap().unwrap().status,
        "answered",
    );
}

/// The no-reply counterpart, through the same fan-out: an agent whose session
/// ends without ever answering must also reach a terminal execution, not park.
#[tokio::test]
async fn an_agent_that_never_replied_still_completes_on_the_turn_boundary() {
    let (server_state, _dir) = test_server_state();
    let slot_id = 8;
    let (comment_id, run_id, execution_id) = seed_parked_answer_agent(&server_state, slot_id);

    let stop = crate::events_socket::IncomingHookEvent::for_test(
        WorkerEvent::Stop {
            session_id: "answer-sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
        Some(execution_id.clone()),
        None,
    );
    dispatch_worker_event_fanout(&server_state, &stop).await;

    assert_eq!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .as_str(),
        "completed",
    );
    assert!(server_state.live_worker_states.get(slot_id).is_none());
    let run = server_state.work_db.get_answer_agent_run(&run_id).unwrap().unwrap();
    assert_eq!(run.status, crate::work::ANSWER_AGENT_RUN_STATUS_FAILED);
    assert_eq!(
        server_state.work_db.get_comment(&comment_id).unwrap().unwrap().status,
        "answer_failed",
        "an unanswered question must still leave the 'in flight' state, but must never read as answered",
    );
}
