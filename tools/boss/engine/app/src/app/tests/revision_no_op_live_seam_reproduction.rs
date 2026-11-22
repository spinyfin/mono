//! Live reproduction of the run-done-proposal-seam-unavailable incident,
//! driven through the **real** Stop/event-dispatch boundary rather than by
//! calling a completion-handler method directly.
//!
//! The unit tests in `completion::tests::t15` cover the same scenario
//! (head lookup failure with independently verified mergeability) but call
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
use boss_engine_completion::{BranchVerifier, REVISION_NO_OP_ATTENTION_KIND};
use boss_engine_runtime::test_support::declare_no_changes_needed;
use boss_work_db::{FakePrStateChecker, PrOpenState};

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
fn seed_parked_revision(server_state: &Arc<ServerState>, pr_url: &str, slot_id: u8) -> (String, tempfile::TempDir) {
    let work_db = &server_state.work_db;
    let product =
        boss_engine_runtime::test_support::create_test_product_named(work_db, "live-seam-reproduction-product");
    let parent =
        boss_engine_runtime::test_support::create_test_chore_manual(work_db, product.id.clone(), "Parent chore");
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
                .created_via("test")
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .prefer_is_soft(true)
                .pr_url(pr_url)
                .build(),
        )
        .unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (execution, run) = work_db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-live-seam",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    boss_engine_runtime::test_support::finish_run_worker_pane_alive(work_db, &execution.id, &run.id, None);
    // Mirror `on_execution_started`'s dispatch-time snapshot so the
    // SHA-delta gate has a baseline to (fail to) compare against.
    work_db.set_execution_pr_head_before(&execution.id, "before").unwrap();
    boss_engine_runtime::test_support::preserve_test_workspace(work_db, &execution.id);
    register_working_worker(server_state, &execution.id, slot_id);
    (execution.id, workspace)
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
) -> boss_engine_runtime::events_socket::IncomingHookEvent {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sock");
    let listener = boss_engine_runtime::events_socket::bind_events_socket(&path).unwrap();
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
    let incoming = boss_engine_runtime::events_socket::handle_connection(
        stream,
        &boss_engine_driver::DriverRegistry::default(),
        &server_state.work_db,
    )
    .await
    .expect("a revision Stop must survive per-connection driver resolution")
    .expect("a revision Stop must decode, not be filtered");
    client.await.unwrap();
    incoming
}

/// A revision worker whose `boss propose done --outcome no-changes-needed`
/// declaration was recorded, with GitHub unreachable for the SHA-delta gate,
/// must reach a terminal state, release its cube lease, free its worker
/// slot, and leave a durable failure when mergeability is unverifiable — driven
/// through the real events socket and the real production fan-out, not
/// through a direct call to `WorkerCompletionHandler::on_stop`. The
/// `run_done_proposals_seam` flag is left off: the no-op declaration is read
/// from its durable stamp regardless of it.
#[tokio::test]
async fn revision_no_op_declaration_fails_closed_through_the_real_dispatch_path() {
    let (server_state, _dir) = test_server_state_with_fakes_and_branch_verifier(Arc::new(UnreachableBranchVerifier));

    let pr = "https://github.com/spinyfin/mono/pull/1613";
    let slot_id = 5;
    let (execution_id, _workspace) = seed_parked_revision(&server_state, pr, slot_id);
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
    let stop = Box::pin(stop_hook_through_the_events_socket(&server_state, &execution_id)).await;
    // Boxed so the fan-out's large generated future lives on the heap rather
    // than inline in this test's state machine on the small test-thread stack.
    Box::pin(dispatch_worker_event_fanout(&server_state, &stop)).await;

    let execution = server_state.work_db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Failed,
        "a declared no-op must fail closed when GitHub mergeability cannot be verified",
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
    let boss_work_db::WorkItem::Task(task) = server_state.work_db.get_work_item(&execution.work_item_id).unwrap()
    else {
        panic!("revision missing")
    };
    assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    let detail = task
        .blocked_detail
        .expect("mergeability failure must be durably recorded");
    assert!(detail.contains("mergeability could not be verified"));
    assert!(detail.contains("probe failed"));
    assert!(items.iter().all(|item| item.kind != REVISION_NO_OP_ATTENTION_KIND));
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
    let (execution_id, _workspace) = seed_parked_revision(&server_state, pr, slot_id);
    super::tmux_stub::install_teardown(&server_state, &execution_id, 4_194_304);
    write_legacy_marker_transcript(&server_state.work_db, &std::env::temp_dir(), &execution_id);
    assert_eq!(
        server_state.work_db.execution_run_done_outcome(&execution_id).unwrap(),
        None
    );

    let stop = Box::pin(stop_hook_through_the_events_socket(&server_state, &execution_id)).await;
    // Boxed so the fan-out's large generated future lives on the heap rather
    // than inline in this test's state machine on the small test-thread stack.
    Box::pin(dispatch_worker_event_fanout(&server_state, &stop)).await;

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

/// Codex's actual terminal envelope must complete an applied no-op even when
/// a description-only revision leaves its parent's CI red.
#[tokio::test]
async fn codex_applied_no_op_with_changed_description_releases_the_live_slot() {
    use boss_engine_completion::tests::{FixedStateProbe, StubBranchVerifier};
    use boss_engine_driver::{AgentDriver, CodexDriver, ProgressSessionConfig, ProgressStreamSource};
    let dir = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        boss_engine_runtime::config::WorkConfig::builder()
            .cwd(dir.path().to_path_buf())
            .db_path(dir.path().join("state.db"))
            .build(),
        None,
    ));
    let verifier = StubBranchVerifier::ok("boss/parent");
    verifier.set_head_oid(Ok("before".into())).await;
    verifier
        .set_body(Ok("description with verification evidence".into()))
        .await;
    let state = ServerState::new_arc_with_app_pid_and_merge_probe(
        cfg,
        None,
        None,
        ServerStateOverrides {
            cube_client: Some(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsCube)),
            execution_runner: Some(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsRunner)),
            branch_verifier: Some(verifier),
            merge_probe: Some(Arc::new(FixedStateProbe(
                boss_observation_contracts::PrLifecycleState::Open(
                    boss_observation_contracts::OpenPrStatus::ci_failing(vec![]),
                ),
            ))),
            ..Default::default()
        },
    )
    .unwrap();
    let (execution_id, _workspace) = seed_parked_revision(&state, "https://github.com/spinyfin/mono/pull/3639", 2);
    super::tmux_stub::install_teardown(&state, &execution_id, 4_194_305);
    state
        .work_db
        .set_execution_pr_body_before(&execution_id, "original description")
        .unwrap();
    declare_no_changes_needed(&state.work_db, &execution_id);
    let driver = CodexDriver::default();
    let mut session = driver
        .progress_session(&ProgressSessionConfig {
            source: ProgressStreamSource::AgentJsonlFile,
            ..Default::default()
        })
        .unwrap();
    session
        .normalize_progress_events(&serde_json::json!({
            "type": "session_meta", "payload": {"id": "no-op-revision"}
        }))
        .unwrap();
    let events = session
        .normalize_progress_events(&serde_json::json!({
            "type": "event_msg", "payload": {"type": "task_complete"}
        }))
        .unwrap();
    assert!(!events.is_empty());
    for event in events {
        let incoming = boss_engine_runtime::events_socket::IncomingHookEvent::resolve(
            &driver,
            event,
            Some(execution_id.clone()),
            None,
            None,
        );
        Box::pin(dispatch_worker_event_fanout(&state, &incoming)).await;
    }
    assert_eq!(
        state.work_db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert!(
        state.live_worker_states.get(2).is_none(),
        "the idle worker must release its slot"
    );
    let execution = state.work_db.get_execution(&execution_id).unwrap();
    assert!(execution.cube_lease_id.is_none());
    assert!(
        state
            .work_db
            .latest_run_turn_boundary_for_execution(&execution_id)
            .unwrap()
            .is_some()
    );
    let boss_work_db::WorkItem::Task(task) = state.work_db.get_work_item(&execution.work_item_id).unwrap() else {
        panic!("revision missing");
    };
    assert_eq!(task.status, boss_protocol::TaskStatus::Done);
}

/// Stack the Stop dispatch path may have in use when it calls out to GitHub:
/// an eighth of the 2 MiB a test thread or a tokio worker gets by default.
///
/// The route uses roughly 120 KiB in an unoptimised arm64 build. This leaves
/// room for platform differences while detecting a callee future built in a
/// caller frame, which can add hundreds of KiB.
const STOP_DISPATCH_STACK_BUDGET: usize = 256 * 1024;

/// Size the dispatch future itself may have. It holds two references and a
/// pointer to the staged fan-out; any callee awaited in place would add its
/// whole state machine to this.
const STOP_DISPATCH_FUTURE_BUDGET: usize = 256;

/// How deep the dispatching thread's stack is each time the dispatch path
/// asks GitHub about a PR.
struct StackAtGithubCalls {
    dispatch_thread: std::thread::ThreadId,
    start: std::sync::atomic::AtomicUsize,
    deepest: std::sync::atomic::AtomicUsize,
}

impl StackAtGithubCalls {
    fn on_this_thread() -> Self {
        Self {
            dispatch_thread: std::thread::current().id(),
            start: std::sync::atomic::AtomicUsize::new(0),
            deepest: std::sync::atomic::AtomicUsize::new(usize::MAX),
        }
    }

    /// Only a call made on the dispatching thread says anything about that
    /// thread's stack.
    fn record(&self) {
        if std::thread::current().id() == self.dispatch_thread {
            let position = boss_engine_utils::heap_future::stack_position();
            let start = self.start.load(std::sync::atomic::Ordering::Relaxed);
            assert_ne!(start, 0, "capture the stack baseline before dispatch");
            let depth = start.saturating_sub(position);
            assert!(
                depth <= STOP_DISPATCH_STACK_BUDGET,
                "the Stop dispatch path was {depth} bytes of stack deep when it probed GitHub; the budget is \
                 {STOP_DISPATCH_STACK_BUDGET}. A function between `dispatch_worker_event_fanout` and the \
                 mergeability probe is building the futures it awaits in its own stack frame again. Await \
                 them through `boss_engine_utils::heap_future::on_heap`; `Box::pin(..).await` does not \
                 keep them out of the frame.",
            );
            self.deepest.fetch_min(position, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Bytes of stack between `start` and the deepest recorded call, or
    /// `None` when no call was recorded.
    fn deepest_below(&self, start: usize) -> Option<usize> {
        let deepest = self.deepest.load(std::sync::atomic::Ordering::Relaxed);
        (deepest != usize::MAX).then(|| start.saturating_sub(deepest))
    }
}

#[test]
#[should_panic(expected = "the Stop dispatch path was")]
fn github_probe_rejects_excess_stack_depth_synchronously() {
    let calls = StackAtGithubCalls::on_this_thread();
    calls.start.store(usize::MAX, std::sync::atomic::Ordering::Relaxed);
    calls.record();
}

/// Fails every mergeability probe, like [`UnreachableBranchVerifier`], and
/// records how deep the stack was when asked.
struct UnreachableMergeProbe(Arc<StackAtGithubCalls>);

#[async_trait]
impl boss_engine_merge_poller::MergeProbe for UnreachableMergeProbe {
    async fn probe(&self, _pr_url: &str) -> Result<boss_engine_merge_poller::PrLifecycleProbe> {
        self.0.record();
        Err(anyhow::anyhow!("error connecting to api.github.com"))
    }
}

/// Guards the Stop dispatch path against stack exhaustion.
///
/// A stack overflow aborts the test process, which takes every other test in
/// the shard down with it and names none of the code responsible. This test
/// takes the same route through the real dispatch path and fails with a
/// number long before that: the declared no-op reaches the mergeability probe
/// under the fan-out, the Stop handler, the revision no-op check and the
/// conflict-delivery gate, so a future that goes back to being built in any
/// of those frames shows up in the depth recorded there.
#[tokio::test]
async fn stop_dispatch_reaches_github_within_its_stack_budget() {
    let github_calls = Arc::new(StackAtGithubCalls::on_this_thread());
    let dir = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        boss_engine_runtime::config::WorkConfig::builder()
            .cwd(dir.path().to_path_buf())
            .db_path(dir.path().join("state.db"))
            .build(),
        None,
    ));
    let server_state = ServerState::new_arc_with_app_pid_and_merge_probe(
        cfg,
        None,
        None,
        ServerStateOverrides {
            cube_client: Some(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsCube)),
            execution_runner: Some(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsRunner)),
            branch_verifier: Some(Arc::new(UnreachableBranchVerifier)),
            merge_probe: Some(Arc::new(UnreachableMergeProbe(github_calls.clone()))),
            ..Default::default()
        },
    )
    .unwrap();
    let slot_id = 7;
    let (execution_id, _workspace) =
        seed_parked_revision(&server_state, "https://github.com/spinyfin/mono/pull/1613", slot_id);
    super::tmux_stub::install_teardown(&server_state, &execution_id, 4_194_306);
    declare_no_changes_needed(&server_state.work_db, &execution_id);
    let stop = stop_hook_through_the_events_socket(&server_state, &execution_id).await;

    let dispatch = dispatch_worker_event_fanout(&server_state, &stop);
    let dispatch_future_size = std::mem::size_of_val(&dispatch);
    assert!(
        dispatch_future_size <= STOP_DISPATCH_FUTURE_BUDGET,
        "the Stop dispatch future is {dispatch_future_size} bytes; the budget is \
         {STOP_DISPATCH_FUTURE_BUDGET}. Something it awaits is stored inline in it. Await it through \
         `boss_engine_utils::heap_future::on_heap`.",
    );
    let start = boss_engine_utils::heap_future::stack_position();
    github_calls.start.store(start, std::sync::atomic::Ordering::Relaxed);
    dispatch.await;

    assert_eq!(
        server_state.work_db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Failed,
        "the run must have taken the fail-closed route this guard measures",
    );
    let depth = github_calls
        .deepest_below(start)
        .expect("the fail-closed route asks GitHub whether the PR is mergeable");
    // Write directly so successful CI runs retain the platform measurement.
    use std::io::Write as _;
    writeln!(
        std::io::stderr(),
        "Stop dispatch on {}-{}: probe depth {depth}/{STOP_DISPATCH_STACK_BUDGET} bytes \
         ({} bytes headroom); future {dispatch_future_size}/{STOP_DISPATCH_FUTURE_BUDGET} bytes",
        std::env::consts::OS,
        std::env::consts::ARCH,
        STOP_DISPATCH_STACK_BUDGET - depth,
    )
    .unwrap();
}
