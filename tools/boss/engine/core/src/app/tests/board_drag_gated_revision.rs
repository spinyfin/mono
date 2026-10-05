//! Regression: dragging a `blocked / worker_failed` revision card to Doing
//! must take the same engine path as `bossctl work start`, and must apply
//! the same dependency rule.
//!
//! The operator-reported shape: a CI-fix revision on a PR whose preceding
//! review-findings revision sits in `in_review` (its commits are pushed,
//! the PR is open and quiescent). `gating_prereqs_for` is kind-aware —
//! `in_review` satisfies a `revision` dependent, because waiting for `done`
//! would deadlock the CI fix behind a PR that cannot merge until that very
//! fix lands. `bossctl work start` honoured that and dispatched; the kanban
//! drop refused with a client-side `done`/`archived`-only mirror of the
//! rule before the engine ever saw the gesture. The app no longer
//! pre-checks; these tests pin the engine contract the board drop now
//! relies on: a startable row dispatches, a genuinely gated one is refused
//! with a `work_error` that names the gate.

use super::*;
use crate::app::work_items::handle_move_work_item_on_board;
use crate::test_support::{create_spawned_execution, create_test_chore_manual, create_test_product_with_repo};
use crate::work::{PrOpenState, StaticPrStateChecker};
use boss_protocol::{
    BoardColumn, BoardDropTarget, CreateRevisionInput, ExecutionKind, FrontendRequest, Task, TaskStatus, WorkItem,
    WorkItemPatch,
};

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>, request_id: &str) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id(request_id)
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

/// An ordinary (no pause-bypass) drag onto the Doing column — the shape
/// every kanban drop sends.
fn drop_on_doing(id: &str) -> FrontendRequest {
    FrontendRequest::MoveWorkItemOnBoard {
        id: id.to_owned(),
        target: BoardDropTarget::new(BoardColumn::Doing, None),
        bypass_dispatch_pause: false,
        observed_pause_since_epoch_s: None,
    }
}

fn task_row(state: &ServerState, id: &str) -> Task {
    match state.work_db.get_work_item(id).expect("get work item") {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        other => panic!("expected a task row for {id}, got {other:?}"),
    }
}

fn set_status(state: &ServerState, id: &str, status: &str) {
    state
        .work_db
        .update_work_item(
            id,
            WorkItemPatch {
                status: Some(status.to_owned()),
                ..Default::default()
            },
        )
        .unwrap_or_else(|err| panic!("set {id} to {status}: {err:#}"));
}

fn create_manual_revision(state: &ServerState, parent_id: &str, description: &str) -> Task {
    state
        .work_db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id)
                .description(description)
                .autostart(false)
                .build(),
            &StaticPrStateChecker(PrOpenState::Open),
        )
        .unwrap_or_else(|err| panic!("create revision {description:?}: {err:#}"))
}

/// Seed the operator's chain and return `(root, review_findings, ci_fix)`:
///
/// - `root`: an `in_review` chore with an open PR.
/// - `review_findings`: the first revision on that PR, now `in_review`
///   (its commits are pushed).
/// - `ci_fix`: the next revision, auto-gated on `review_findings` at
///   creation, whose worker then failed — `blocked / worker_failed`,
///   `autostart = false`, with the chain-tail `blocks` edge still in place.
fn seed_ci_fix_behind_in_review_revision(state: &Arc<ServerState>) -> (Task, Task, Task) {
    let product = create_test_product_with_repo(
        &state.work_db,
        "BoardDragGatedRevision",
        Some("git@example.com:board/drag-gated-revision.git"),
    );
    let root = create_test_chore_manual(&state.work_db, product.id.clone(), "Chain root chore");
    state
        .work_db
        .update_work_item(
            &root.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some("https://github.com/example/repo/pull/1".to_owned()),
                ..Default::default()
            },
        )
        .expect("move chain root to in_review with a PR");

    // First revision: no chain tail yet, so it is born `todo`.
    let review_findings = create_manual_revision(state, &root.id, "Address review findings");
    assert_eq!(review_findings.status, TaskStatus::Todo);

    // Second revision while the first is still open: auto-gated on it.
    let ci_fix = create_manual_revision(state, &root.id, "Fix failing CI");
    assert_eq!(
        ci_fix.status,
        TaskStatus::Blocked,
        "a revision created behind an open sibling must be chain-tail gated"
    );
    assert_eq!(ci_fix.blocked_reason.as_deref(), Some("dependency"));

    // The review-findings worker pushes its commits: the row advances to
    // `in_review`. That satisfies the revision rule, so the engine's
    // cascade releases the CI fix back to `todo` — the edge itself stays.
    set_status(state, &review_findings.id, "in_review");
    let ci_fix = task_row(state, &ci_fix.id);
    assert_eq!(
        ci_fix.status,
        TaskStatus::Todo,
        "an in_review prerequisite must release a revision dependent; got {ci_fix:?}"
    );
    assert_eq!(
        state.work_db.gating_prereqs_for(&ci_fix.id).expect("gating prereqs"),
        Vec::<String>::new(),
        "an in_review sibling revision must not gate the next writer on the PR"
    );

    // The CI-fix worker runs and fails: `blocked / worker_failed`,
    // `autostart = false` — exactly the operator's row.
    let failed_execution = create_spawned_execution(&state.work_db, &ci_fix.id, 999_999);
    state
        .work_db
        .record_worker_failure(&failed_execution, "worker exited before producing a PR")
        .expect("record worker failure");
    let ci_fix = task_row(state, &ci_fix.id);
    assert_eq!(ci_fix.status, TaskStatus::Blocked);
    assert_eq!(ci_fix.blocked_reason.as_deref(), Some("worker_failed"));
    assert!(!ci_fix.autostart, "a worker failure parks the row with autostart off");

    (root, task_row(state, &review_findings.id), ci_fix)
}

/// The operator's gesture: drag the `blocked / worker_failed` CI-fix
/// revision from Backlog to Doing. The engine must accept it — the only
/// prerequisite is an `in_review` sibling, which satisfies a revision —
/// reset the failure block, land the card in Doing, and dispatch a fresh
/// `revision_implementation` execution, exactly as `bossctl work start`
/// did for the same row.
#[tokio::test]
async fn blocked_worker_failed_revision_dropped_on_doing_dispatches() {
    let (server_state, _dir) = test_server_state_with_fakes();
    let (_root, review_findings, ci_fix) = seed_ci_fix_behind_in_review_revision(&server_state);
    assert_eq!(review_findings.status, TaskStatus::InReview);
    let executions_before = server_state
        .work_db
        .list_executions(Some(&ci_fix.id))
        .expect("list executions");
    assert!(
        executions_before.iter().all(|e| e.status.is_terminal()),
        "precondition: the failed attempt must be terminal so the drop has to mint a new one; got {executions_before:?}"
    );

    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink, "req-drop-ci-fix");
    handle_move_work_item_on_board(ctx, drop_on_doing(&ci_fix.id)).await;

    match sole_response(&sink).await {
        FrontendEvent::WorkItemUpdated { item } => {
            let (WorkItem::Task(t) | WorkItem::Chore(t)) = item else {
                panic!("expected a task row in the drop response");
            };
            assert_eq!(t.id, ci_fix.id);
            assert_eq!(t.status, TaskStatus::Active, "the drop must land the card in Doing");
            assert_eq!(
                t.blocked_reason, None,
                "entering Doing clears the worker_failed block the way an explicit start does"
            );
        }
        other => panic!("expected WorkItemUpdated for a startable revision, got: {other:?}"),
    }

    let executions = server_state
        .work_db
        .list_executions(Some(&ci_fix.id))
        .expect("list executions");
    let fresh: Vec<_> = executions.iter().filter(|e| !e.status.is_terminal()).collect();
    assert_eq!(
        fresh.len(),
        1,
        "the drop must dispatch exactly one new execution for the revision; got {executions:?}"
    );
    assert_eq!(
        fresh[0].kind,
        ExecutionKind::RevisionImplementation,
        "a revision row dispatches a revision_implementation execution"
    );
    assert!(
        executions_before.iter().all(|before| before.id != fresh[0].id),
        "the failed attempt must not be resurrected; a new row is minted"
    );
}

/// The refusal half of the contract: a revision whose prerequisite is
/// genuinely unsatisfied (here, a sibling still `blocked`) must be refused
/// by the engine with a `work_error` that names the gate — so the app can
/// show the operator why the card bounced — and must not move or dispatch.
/// Same handler, same rule, same message family as an explicit start.
#[tokio::test]
async fn gated_revision_dropped_on_doing_is_refused_with_the_gate_named() {
    let (server_state, _dir) = test_server_state_with_fakes();
    let (root, _review_findings, ci_fix) = seed_ci_fix_behind_in_review_revision(&server_state);

    // A third revision created while the CI fix is still blocked: the CI
    // fix is the chain tail, so this one is gated on it. `blocked` does not
    // satisfy anything, so this row is genuinely stuck until the CI fix
    // reaches `in_review` (or is archived).
    let second_findings = create_manual_revision(&server_state, &root.id, "Second review-findings pass");
    assert_eq!(second_findings.status, TaskStatus::Blocked);
    assert_eq!(second_findings.blocked_reason.as_deref(), Some("dependency"));
    assert_eq!(
        server_state
            .work_db
            .gating_prereqs_for(&second_findings.id)
            .expect("gating prereqs"),
        vec![ci_fix.id.clone()],
        "the third revision must be gated on the blocked CI fix"
    );

    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink, "req-drop-gated-revision");
    handle_move_work_item_on_board(ctx, drop_on_doing(&second_findings.id)).await;

    match sole_response(&sink).await {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("gated by"),
                "the refusal must say the row is gated, got: {message}"
            );
            assert!(
                message.contains(&ci_fix.id),
                "the refusal must name the prerequisite holding the row, got: {message}"
            );
        }
        other => panic!("expected a WorkError naming the gate, got: {other:?}"),
    }

    let after = task_row(&server_state, &second_findings.id);
    assert_eq!(
        after.status,
        TaskStatus::Blocked,
        "a refused drop must not move the card"
    );
    assert_eq!(after.blocked_reason.as_deref(), Some("dependency"));
    let executions = server_state
        .work_db
        .list_executions(Some(&second_findings.id))
        .expect("list executions");
    assert!(
        executions.iter().all(|e| e.status.is_terminal()),
        "a refused drop must not dispatch anything; got {executions:?}"
    );
}

/// The revision rule is kind-specific. A non-revision dependent (a chore)
/// gated on an `in_review` prerequisite stays gated: `in_review` means
/// "awaiting merge", and ordinary work waits for `done`. The same handler
/// refuses the drop and names the gate.
#[tokio::test]
async fn chore_gated_on_in_review_prerequisite_dropped_on_doing_is_refused() {
    let (server_state, _dir) = test_server_state_with_fakes();
    let product = create_test_product_with_repo(
        &server_state.work_db,
        "BoardDragGatedChore",
        Some("git@example.com:board/drag-gated-chore.git"),
    );
    let prerequisite = create_test_chore_manual(&server_state.work_db, product.id.clone(), "Prerequisite chore");
    set_status(&server_state, &prerequisite.id, "in_review");
    let dependent = create_test_chore_manual(&server_state.work_db, product.id.clone(), "Dependent chore");
    server_state
        .work_db
        .add_dependency(boss_protocol::AddDependencyInput {
            dependent: dependent.id.clone(),
            prerequisite: prerequisite.id.clone(),
            relation: None,
        })
        .expect("add blocks edge");
    let dependent = task_row(&server_state, &dependent.id);
    assert_eq!(dependent.status, TaskStatus::Blocked);
    assert_eq!(
        server_state
            .work_db
            .gating_prereqs_for(&dependent.id)
            .expect("gating prereqs"),
        vec![prerequisite.id.clone()],
        "in_review satisfies a revision dependent only; a chore stays gated"
    );

    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink, "req-drop-gated-chore");
    handle_move_work_item_on_board(ctx, drop_on_doing(&dependent.id)).await;

    match sole_response(&sink).await {
        FrontendEvent::WorkError { message } => {
            assert!(message.contains("gated by"), "got: {message}");
            assert!(message.contains(&prerequisite.id), "got: {message}");
        }
        other => panic!("expected a WorkError naming the gate, got: {other:?}"),
    }
    assert_eq!(task_row(&server_state, &dependent.id).status, TaskStatus::Blocked);
}
