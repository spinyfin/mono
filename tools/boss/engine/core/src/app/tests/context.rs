// Behaviour tests for `FrontendRequest::GetWorkerContext` — `boss context`,
// dispatched into `app::context`.
//
// Attribution is exercised the same way `app::tests::proposals` does: this
// process's own pid is registered as the worker running the fixture's
// execution, so `lookup_with_ancestor_walk` resolves it exactly like a live
// worker session.

use super::*;
use boss_protocol::{
    AddDependencyInput, CreateAttentionInput, CreateProjectInput, CreateTaskInput, ProposalErrorCode, ProposalKind,
    WorkerContextBundle,
};

use crate::app::context;
use crate::work::SubmitWorkerProposalInput;

/// This process's own pid, standing in for the worker's socket peer —
/// `lookup_with_ancestor_walk` treats a pid as its own ancestor, so
/// registering `std::process::id()` makes this process look exactly like a
/// worker session, the same trick `t03`/`trust_authorization`/`proposals`
/// use.
fn self_pid() -> libc::pid_t {
    std::process::id() as libc::pid_t
}

/// A product, project, and two tasks in it (the caller's own task plus a
/// sibling that depends on it), with a `ready` execution for the caller's
/// own task and [`self_pid`] registered as the worker running it.
struct ProjectFixture {
    server_state: Arc<ServerState>,
    _dir: tempfile::TempDir,
    execution_id: String,
    own_task_id: String,
    sibling_task_id: String,
}

impl ProjectFixture {
    fn new() -> Self {
        let (server_state, dir) = test_server_state();
        let db = &server_state.work_db;

        let product = crate::test_support::create_test_product(db);
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("A project")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();
        let own_task = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(product.id.clone())
                    .project_id(project.id.clone())
                    .name("Own task")
                    .build(),
            )
            .unwrap();
        let sibling_task = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(product.id.clone())
                    .project_id(project.id.clone())
                    .name("Sibling task")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        // Sibling depends on the caller's own task, so the bundle's
        // `own_dependencies.dependents` and the sibling's own dependency
        // detail both have something to assert on.
        db.add_dependency(AddDependencyInput {
            dependent: sibling_task.id.clone(),
            prerequisite: own_task.id.clone(),
            relation: None,
        })
        .unwrap();

        db.create_attention(
            CreateAttentionInput::builder()
                .kind("followup")
                .association_task_id(own_task.id.clone())
                .proposed_name("Add retry to the client")
                .rationale("observed transient failures")
                .build(),
        )
        .unwrap();

        let execution = crate::test_support::create_execution_started_now(db, &own_task.id);
        server_state.worker_registry.register(self_pid(), execution.clone());

        Self {
            server_state,
            _dir: dir,
            execution_id: execution,
            own_task_id: own_task.id,
            sibling_task_id: sibling_task.id,
        }
    }
}

/// Build a per-request `Dispatch` the way `handle_frontend_connection` does
/// for a real socket frame, with `peer_pid` standing in for `SO_PEERCRED`.
fn dispatch_with_peer(state: &Arc<ServerState>, sink: &Arc<SessionSink>, peer_pid: Option<libc::pid_t>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .maybe_peer_pid(peer_pid)
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

/// The single response `handle_get_worker_context` enqueued.
async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

async fn call(state: &Arc<ServerState>, peer_pid: Option<libc::pid_t>, run_id: &str) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch_with_peer(state, &sink, peer_pid);
    context::handle_get_worker_context(
        ctx,
        FrontendRequest::GetWorkerContext {
            run_id: run_id.to_owned(),
        },
    )
    .await;
    sole_response(&sink).await
}

fn bundle(event: FrontendEvent) -> WorkerContextBundle {
    match event {
        FrontendEvent::WorkerContextResult { bundle } => *bundle,
        FrontendEvent::ProposalRejected { error } => panic!("expected a bundle, got rejection: {error}"),
        other => panic!("expected WorkerContextResult, got {other:?}"),
    }
}

#[tokio::test]
async fn project_task_bundle_carries_task_project_product_siblings_and_edges() {
    let fx = ProjectFixture::new();
    let bundle = bundle(call(&fx.server_state, Some(self_pid()), &fx.execution_id).await);

    assert_eq!(bundle.task.id, fx.own_task_id);
    assert!(bundle.project.is_some(), "a project task must carry its project");
    assert_eq!(bundle.product.id, bundle.task.product_id);

    assert_eq!(
        bundle.sibling_tasks.len(),
        1,
        "must see the one sibling, excluding itself"
    );
    let sibling = &bundle.sibling_tasks[0];
    assert_eq!(sibling.task.id, fx.sibling_task_id);
    assert_eq!(
        sibling.dependencies.prerequisites.len(),
        1,
        "the sibling depends on the caller's own task",
    );
    assert_eq!(sibling.dependencies.prerequisites[0].id, fx.own_task_id);

    assert_eq!(
        bundle.own_dependencies.dependents.len(),
        1,
        "the caller's own task gates the sibling",
    );
    assert_eq!(bundle.own_dependencies.dependents[0].id, fx.sibling_task_id);

    assert_eq!(
        bundle.attention_groups.len(),
        1,
        "the open followup group must be visible"
    );
}

#[tokio::test]
async fn chore_execution_has_no_project_and_no_siblings() {
    let (server_state, _dir) = test_server_state();
    let db = &server_state.work_db;
    let product = crate::test_support::create_test_product(db);
    let chore = crate::test_support::create_test_chore(db, product.id.clone(), "Cleanup");
    let execution = crate::test_support::create_ready_chore_execution(db, chore.id.clone());
    server_state.worker_registry.register(self_pid(), execution.id.clone());

    let bundle = bundle(call(&server_state, Some(self_pid()), &execution.id).await);

    assert_eq!(bundle.task.id, chore.id);
    assert!(bundle.project.is_none(), "a chore has no parent project");
    assert!(
        bundle.sibling_tasks.is_empty(),
        "a chore has no project to have siblings in"
    );
}

#[tokio::test]
async fn own_work_items_proposals_are_included_across_executions() {
    let fx = ProjectFixture::new();
    let outcome = fx
        .server_state
        .work_db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &fx.execution_id,
            work_item_id: &fx.own_task_id,
            kind: ProposalKind::Blocked,
            payload_json: r#"{"reason":"stuck"}"#,
            idempotency_key: "test-key",
        })
        .unwrap()
        .unwrap();

    let bundle = bundle(call(&fx.server_state, Some(self_pid()), &fx.execution_id).await);
    assert_eq!(bundle.proposals.len(), 1);
    assert_eq!(bundle.proposals[0].id, outcome.proposal.id);
}

#[tokio::test]
async fn unattributed_caller_is_rejected() {
    let (server_state, _dir) = test_server_state();
    // No worker registered at all — a plain shell, not a worker session.
    let event = call(&server_state, Some(self_pid()), "exec_missing").await;
    match event {
        FrontendEvent::ProposalRejected { error } => {
            assert_eq!(error.code, ProposalErrorCode::AttributionUnresolved);
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}
