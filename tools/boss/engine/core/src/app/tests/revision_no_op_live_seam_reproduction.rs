//! Live reproduction of the run-done-proposal-seam-unavailable incident,
//! driven through the **real** Stop/event-dispatch boundary rather than by
//! calling a completion-handler method directly.
//!
//! The unit tests in `completion::tests::t15` cover the same scenario
//! (`revision_no_op_survives_unavailable_github_for_every_driver`) but call
//! `WorkerCompletionHandler::on_stop` on a bare handler wired to test
//! doubles. That leaves the actual integration boundary the original
//! incident occurred at unverified: a real driver's final response is
//! captured by the `boss-event` shim, sent over the engine's Unix events
//! socket, resolved to a driver and fanned out by the production dispatch
//! path (`events_socket::handle_connection` +
//! `worker_events::dispatch_worker_event_fanout`), and only THEN reaches
//! `WorkerCompletionHandler::on_stop`.
//!
//! These tests exercise that whole path against a real, in-process
//! `ServerState` (mirroring `answer_agent_lifecycle.rs`'s
//! `stop_hook_through_the_events_socket` pattern): a real `UnixListener`
//! decodes a real Stop hook payload, real per-connection driver resolution
//! runs, and the real fan-out drives the real completion handler. The
//! GitHub branch verifier is stubbed to fail every call, standing in for the
//! incident evidence (`error connecting to api.github.com`) the original
//! coordinator trace recorded. Everything else — socket wiring, driver
//! resolution, event fan-out, the completion handler, the DB, the cube
//! lease release, the attention-item recording — is the real production
//! code.
//!
//! There are two halves. With the worker's `boss propose done --outcome
//! no-changes-needed` declaration recorded, the run terminalizes. Without
//! it — which is exactly what a genuinely unreachable engine socket
//! produces: no proposal record, no `run_done_outcome` — the run is NEVER
//! closed as a no-op by inference, even when the transcript carries the
//! retired `NO_CHANGES_NEEDED` text marker.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use boss_protocol::{CreateExecutionInput, CreateRevisionInput, ExecutionKind, ExecutionStatus};

use super::*;
use crate::app::worker_events::dispatch_worker_event_fanout;
use crate::completion::{BranchVerifier, REVISION_NO_OP_ATTENTION_KIND};
use crate::work::{FakePrStateChecker, PrOpenState};

/// Always fails every call — standing in for a genuinely unreachable
/// GitHub, the same evidence (`error connecting to api.github.com`) the
/// incident's coordinator trace recorded for the SHA-delta gate.
struct UnreachableBranchVerifier;

#[async_trait]
impl BranchVerifier for UnreachableBranchVerifier {
    async fn fetch_pr_head_ref(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
        Err(anyhow::anyhow!("error connecting to api.github.com"))
    }

    async fn fetch_pr_head_oid(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
        Err(anyhow::anyhow!("error connecting to api.github.com"))
    }

    async fn fetch_pr_head_oid_fresh(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
        Err(anyhow::anyhow!("error connecting to api.github.com"))
    }

    async fn fetch_diff_line_count(&self, _repo_slug: &str, _base: &str, _head: &str) -> Result<u64> {
        Err(anyhow::anyhow!("error connecting to api.github.com"))
    }

    async fn fetch_pr_base_ref(&self, _repo_slug: &str, _pr_number: u64) -> Result<String> {
        Err(anyhow::anyhow!("error connecting to api.github.com"))
    }

    async fn fetch_diff_signature(&self, _repo_slug: &str, _base: &str, _head: &str) -> Result<String> {
        Err(anyhow::anyhow!("error connecting to api.github.com"))
    }

    async fn fetch_pr_title_and_body(&self, _repo_slug: &str, _pr_number: u64) -> Result<(String, String)> {
        Err(anyhow::anyhow!("error connecting to api.github.com"))
    }
}

/// Seed a `revision_implementation` execution bound to a parent PR,
/// occupying `slot_id` as a live worker mid-turn — the same shape
/// `completion::tests::revision_fixture` builds for the handler-level
/// tests, but against a real `ServerState`'s `WorkDb` so it is reachable
/// through the real dispatch path.
fn seed_parked_revision(server_state: &Arc<ServerState>, pr_url: &str, slot_id: u8) -> String {
    let work_db = &server_state.work_db;
    let product = crate::test_support::create_test_product_named(work_db, "live-seam-reproduction-product");
    let parent = crate::test_support::create_test_chore_manual(work_db, product.id.clone(), "Parent chore");
    work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![parent.id, pr_url],
        )
        .unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = work_db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Live-seam reproduction finding")
                .build(),
            &checker,
        )
        .unwrap();
    let execution = work_db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .prefer_is_soft(true)
                .pr_url(pr_url)
                .build(),
        )
        .unwrap();
    let workspace = std::env::temp_dir();
    let (execution, run) = work_db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-live-seam",
            workspace.to_str().unwrap(),
        )
        .unwrap();
    crate::test_support::finish_run_worker_pane_alive(work_db, &execution.id, &run.id, None);
    // Mirror `on_execution_started`'s dispatch-time snapshot so the
    // SHA-delta gate has a baseline to (fail to) compare against.
    work_db.set_execution_pr_head_before(&execution.id, "before").unwrap();
    register_working_worker(server_state, &execution.id, slot_id);
    execution.id
}

/// Write a final message carrying the retired `NO_CHANGES_NEEDED` text
/// marker the way a real Claude session leaves it on disk, and register the
/// path the way `PaneSpawnRunner` does.
fn write_legacy_marker_transcript(db: &WorkDb, workspace_path: &Path, execution_id: &str) {
    let obj = serde_json::json!({
        "type": "assistant",
        "message": { "content": [{"type": "text", "text": "The finding needs no change.\nNO_CHANGES_NEEDED"}] }
    });
    let transcript_path = workspace_path.join(format!("transcript-{execution_id}.jsonl"));
    std::fs::write(&transcript_path, format!("{obj}\n")).unwrap();
    db.set_run_transcript_path_if_unset(execution_id, transcript_path.to_str().unwrap())
        .unwrap();
}

/// Deliver a Claude `Stop` hook payload for `execution_id` the way the
/// `boss-event` shim does — a JSON write to the engine's events socket —
/// and return the decoded event. Mirrors
/// `answer_agent_lifecycle::stop_hook_through_the_events_socket`: going
/// through `handle_connection` rather than synthesising an
/// `IncomingHookEvent` directly is the point.
async fn stop_hook_through_the_events_socket(
    server_state: &Arc<ServerState>,
    execution_id: &str,
) -> crate::events_socket::IncomingHookEvent {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sock");
    let listener = crate::events_socket::bind_events_socket(&path).unwrap();
    let payload = format!(
        r#"{{"hook_event_name":"Stop","session_id":"live-seam-sess-1","stop_hook_active":false,"_boss_run_id":"{execution_id}"}}"#
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
    .expect("a revision Stop must survive per-connection driver resolution");
    client.await.unwrap();
    incoming
}

/// A revision worker whose `boss propose done --outcome no-changes-needed`
/// declaration was recorded, with GitHub unreachable for the SHA-delta gate,
/// must reach a terminal state, release its cube lease, free its worker
/// slot, and leave a durable declined-finding attention record — driven
/// through the real events socket and the real production fan-out, not
/// through a direct call to `WorkerCompletionHandler::on_stop`. The
/// `run_done_proposals_seam` flag is left off: the no-op declaration is read
/// from its durable stamp regardless of it.
#[tokio::test]
async fn revision_no_op_declaration_terminalizes_through_the_real_dispatch_path() {
    let (server_state, _dir) = test_server_state_with_fakes_and_branch_verifier(Arc::new(UnreachableBranchVerifier));

    let pr = "https://github.com/spinyfin/mono/pull/1613";
    let slot_id = 5;
    let execution_id = seed_parked_revision(&server_state, pr, slot_id);
    super::tmux_stub::install_teardown(&server_state, &execution_id, 4_194_303);
    declare_no_changes_needed(&server_state.work_db, &execution_id);

    // Before: the row says live and the pool says claimed.
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_live()
    );
    assert!(server_state.live_worker_states.get(slot_id).is_some());

    // The driver's turn boundary, carried the whole production way: the
    // shim's JSON over the events socket, through per-connection driver
    // resolution, into the fan-out.
    let stop = stop_hook_through_the_events_socket(&server_state, &execution_id).await;
    dispatch_worker_event_fanout(&server_state, &stop).await;

    let execution = server_state.work_db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Completed,
        "a declared no-op must reach a terminal state even with GitHub unreachable",
    );
    assert!(
        execution.cube_lease_id.is_none(),
        "the cube lease must be released with the row, not left held",
    );
    assert!(
        server_state.live_worker_states.get(slot_id).is_none(),
        "the worker slot must come back",
    );

    let items = server_state.work_db.list_attention_items(&execution_id).unwrap();
    let declined = items
        .iter()
        .find(|item| item.kind == REVISION_NO_OP_ATTENTION_KIND)
        .expect("declined-finding record must be filed through the real dispatch path");
    assert!(declined.body_markdown.contains("not independently verified"));
}

/// The converse, and the retirement guard: when `boss propose done` never
/// reached the engine (no proposal record, no `run_done_outcome`), a
/// transcript that ends with the retired `NO_CHANGES_NEEDED` line must NOT
/// close the revision. The run stays live, holding its lease and slot, with
/// no declined-finding record — the failure stays visible to the existing
/// stale-worker / run-done-missing handling instead of being inferred as a
/// quiet success.
#[tokio::test]
async fn revision_without_declaration_is_never_closed_as_a_no_op_by_inference() {
    let (server_state, _dir) = test_server_state_with_fakes_and_branch_verifier(Arc::new(UnreachableBranchVerifier));

    let pr = "https://github.com/spinyfin/mono/pull/1613";
    let slot_id = 6;
    let execution_id = seed_parked_revision(&server_state, pr, slot_id);
    super::tmux_stub::install_teardown(&server_state, &execution_id, 4_194_304);
    write_legacy_marker_transcript(&server_state.work_db, &std::env::temp_dir(), &execution_id);
    assert_eq!(
        server_state.work_db.execution_run_done_outcome(&execution_id).unwrap(),
        None
    );

    let stop = stop_hook_through_the_events_socket(&server_state, &execution_id).await;
    dispatch_worker_event_fanout(&server_state, &stop).await;

    let execution = server_state.work_db.get_execution(&execution_id).unwrap();
    assert!(
        execution.status.is_live(),
        "the retired marker must not terminalize a run; got {:?}",
        execution.status,
    );
    assert!(execution.cube_lease_id.is_some(), "the lease stays held");
    assert!(
        server_state
            .work_db
            .list_attention_items(&execution_id)
            .unwrap()
            .iter()
            .all(|item| item.kind != REVISION_NO_OP_ATTENTION_KIND),
        "no declined-finding record may be filed for an undeclared run",
    );
}

/// Stamp the durable declaration `boss propose done --outcome
/// no-changes-needed` leaves on the execution row (what `apply_run_done`
/// writes).
fn declare_no_changes_needed(db: &WorkDb, execution_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = 'no_changes_needed', run_done_declared_at = '1' WHERE id = ?1",
            [execution_id],
        )
        .unwrap();
}
