//! Architectural-mismatch answers go through `handle_operator_question` and
//! must dispatch like `bossctl agents launch`: through the pause and outside
//! the focus set, including after a writer wait or a capacity refusal.

use super::*;
use crate::app::work_items::handle_operator_question;
use boss_engine_dispatch::coordinator::DispatchPauseOrigin;
use boss_engine_runtime::test_support::{create_test_chore_manual, create_test_product, insert_host_capability};
use boss_execution_contracts::{
    CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
};
use boss_protocol::{
    ArchitecturalMismatchEvidence, CONFLICT_MISMATCH_CHOICE_FORCE_MERGE, CONFLICT_MISMATCH_CHOICE_REWRITE,
    CREATED_VIA_MERGE_CONFLICT_PREFIX, CreateProjectInput, CreateRevisionInput, CreateTaskInput, DispatchFocus,
    DispatchFocusTargetKind, DispatchPauseScope, EffortLevel, ExecutionStatus, FrontendRequest, MismatchBasis,
    OperatorAnswer, PauseReason, ProposalKind, RunDoneOutcome, RunDoneProposalPayload, WorkExecution, WorkItemPatch,
};
use boss_work_db::{
    ConflictResolutionInsertInput, FakePrStateChecker, PrOpenState, SubmitWorkerProposalInput,
};

const PR_URL: &str = "https://github.com/spinyfin/mono/pull/3405";

/// Like [`boss_engine_runtime::test_support::AlwaysSucceedsCube`], and also
/// implements `goto_workspace` so a conflict revision with a PR URL can
/// finish `force_dispatch` without a live cube.
struct AnswerDispatchCube;

boss_execution_contracts::stub_cube_client! { AnswerDispatchCube {
    async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<CubeRepoHandle> {
        Ok(CubeRepoHandle {
            repo_id: "test-repo".to_owned(),
        })
    }
    async fn lease_workspace(
        &self,
        _repo_id: &str,
        _task: &str,
        prefer_workspace_id: Option<&str>,
        _allow_dirty: bool,
        _exclude_workspace_ids: &[&str],
    ) -> anyhow::Result<CubeWorkspaceLease> {
        let workspace_id = prefer_workspace_id
            .map(str::to_owned)
            .unwrap_or_else(|| "test-workspace-001".to_owned());
        Ok(CubeWorkspaceLease {
            lease_id: "test-lease-1".to_owned(),
            workspace_path: std::path::PathBuf::from(format!("/tmp/{workspace_id}")),
            workspace_id,
            dirty_verified: None,
        })
    }
    async fn create_change(
        &self,
        _workspace_path: &std::path::Path,
        _title: &str,
    ) -> anyhow::Result<CubeChangeHandle> {
        Ok(CubeChangeHandle {
            change_id: "test-change-1".to_owned(),
        })
    }
    async fn goto_workspace(&self, _workspace_path: &std::path::Path, _pr: u64) -> anyhow::Result<()> {
        Ok(())
    }
    async fn release_workspace(&self, _lease_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn workspace_status(
        &self,
        workspace_path: &std::path::Path,
    ) -> anyhow::Result<CubeWorkspaceStatus> {
        Ok(CubeWorkspaceStatus::builder()
            .workspace_id("test-workspace-001")
            .workspace_path(workspace_path.to_path_buf())
            .state("leased")
            .lease_id("test-lease-1")
            .holder("boss/0")
            .task("test task")
            .leased_at_epoch_s(1_700_000_000)
            .lease_expires_at_epoch_s(1_700_001_800)
            .build())
    }
    async fn heartbeat_lease(&self, _lease_id: &str, _ttl_seconds: Option<u64>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn force_release_lease(&self, _lease_id: &str, _reason: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn list_workspaces(&self) -> anyhow::Result<Vec<CubeWorkspaceStatus>> {
        Ok(vec![])
    }
    async fn list_repos(&self) -> anyhow::Result<Vec<CubeRepoSummary>> {
        Ok(vec![])
    }
}}

struct Parked {
    _dir: tempfile::TempDir,
    state: Arc<ServerState>,
    revision_id: String,
    focused_project_id: String,
}

fn evidence() -> ArchitecturalMismatchEvidence {
    ArchitecturalMismatchEvidence {
        basis: MismatchBasis::SubsystemDeleted,
        conflicting_commits: "all six commits of the PR".into(),
        main_changes: "the review-queue subsystem was deleted outright".into(),
        attempted_port: "mapped every touched symbol onto main and found no queue to extend".into(),
        contradiction: "Main deleted the review queue entirely, so the PR's new queue priority has nowhere to live."
            .into(),
        recommendation: "Rewrite the priority around the dispatcher.".into(),
    }
}

fn parked_mismatch() -> Parked {
    let (state, dir) = test_server_state_with_overrides(ServerStateOverrides {
        cube_client: Some(Arc::new(AnswerDispatchCube)),
        execution_runner: Some(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsRunner)),
        pr_state_checker: Some(Arc::new(FakePrStateChecker::always(PrOpenState::Open))),
        ..Default::default()
    });
    insert_host_capability(&state.work_db, "local", "driver=claude", "auto");
    let db = &state.work_db;
    let product = create_test_product(db);
    let focused = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Focused".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let chore = create_test_chore_manual(db, product.id.clone(), "PR chore");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            pr_url: Some(PR_URL.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&chore.id])
        .unwrap();
    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: PR_URL.into(),
            pr_number: 3405,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })
        .unwrap()
        .unwrap();
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(chore.id.clone())
                .description("Resolve merge conflict against main")
                .created_via(format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}{}", crz.id))
                .effort_level(EffortLevel::Trivial)
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision.id)
        .unwrap();
    let execution_id = db.list_executions(Some(&revision.id)).unwrap()[0].id.clone();
    db.start_execution_run(&execution_id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    db.record_conflict_architectural_mismatch(&crz.id, Some(&evidence()))
        .unwrap()
        .expect("live attempt");
    let payload = serde_json::to_string(&RunDoneProposalPayload {
        outcome: RunDoneOutcome::Blocked,
        summary: "Architectural mismatch".into(),
        question: None,
        queue_ci_evidence: None,
    })
    .unwrap();
    db.submit_worker_proposal(SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &revision.id,
        kind: ProposalKind::RunDone,
        payload_json: &payload,
        idempotency_key: "declare-blocked",
    })
    .unwrap()
    .unwrap();
    let question = db
        .execution_operator_question(&execution_id)
        .unwrap()
        .expect("the engine asks on the worker's behalf");
    db.record_worker_awaiting_operator_answer(&execution_id, "worker declared blocked", &question)
        .unwrap()
        .expect("live execution parks the revision");
    Parked {
        _dir: dir,
        state,
        revision_id: revision.id,
        focused_project_id: focused.id,
    }
}

fn hold_pause_and_focus(parked: &Parked) {
    let mut focus = DispatchFocus::default();
    focus.insert(DispatchFocusTargetKind::Project, parked.focused_project_id.clone());
    parked.state.execution_coordinator.restore_dispatch_focus(focus);
    parked.state.execution_coordinator.pause_dispatch_scoped(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::NewWork,
        PauseReason::new("test: operator pause").unwrap(),
    );
}

async fn answer(parked: &Parked, choice: &str) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = Dispatch::builder()
        .server_state(parked.state.clone())
        .work_db(parked.state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-mismatch-answer")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build();
    handle_operator_question(
        ctx,
        FrontendRequest::AnswerOperatorQuestion {
            id: parked.revision_id.clone(),
            answer: OperatorAnswer::MultipleChoice {
                value: choice.to_owned(),
            },
        },
    )
    .await;
    sink.close();
    sink.next().await.expect("handler must send a response").payload
}

fn latest_execution(state: &ServerState, work_item_id: &str) -> WorkExecution {
    state
        .work_db
        .list_executions(Some(work_item_id))
        .unwrap()
        .into_iter()
        .max_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)))
        .expect("execution")
}

async fn wait_until_not_ready(state: &ServerState, execution_id: &str) {
    for _ in 0..300 {
        let status = state.work_db.get_execution(execution_id).unwrap().status;
        if status != ExecutionStatus::Ready {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("execution {execution_id} stayed ready");
}

#[tokio::test]
async fn force_answer_dispatches_through_the_pause_and_outside_the_focus_set() {
    let parked = parked_mismatch();
    hold_pause_and_focus(&parked);
    parked.state.execution_coordinator.drain_ready_queue_for_test().await;
    let response = answer(&parked, CONFLICT_MISMATCH_CHOICE_FORCE_MERGE).await;
    assert!(
        matches!(response, FrontendEvent::WorkItemUpdated { .. }),
        "{response:?}"
    );
    let exec = latest_execution(&parked.state, &parked.revision_id);
    wait_until_not_ready(&parked.state, &exec.id).await;
    assert_ne!(exec.status, ExecutionStatus::WaitingDependency);
    assert_ne!(
        parked.state.work_db.get_execution(&exec.id).unwrap().status,
        ExecutionStatus::Ready
    );
}

#[tokio::test]
async fn rewrite_answer_dispatches_the_port_through_the_pause_and_outside_the_focus_set() {
    let parked = parked_mismatch();
    hold_pause_and_focus(&parked);
    let response = answer(&parked, CONFLICT_MISMATCH_CHOICE_REWRITE).await;
    assert!(
        matches!(response, FrontendEvent::WorkItemUpdated { .. }),
        "{response:?}"
    );
    let port_id: String = parked
        .state
        .work_db
        .connect()
        .unwrap()
        .query_row(
            "SELECT revision_task_id FROM conflict_resolutions
             WHERE revision_task_id IS NOT NULL AND revision_task_id != ?1",
            [&parked.revision_id],
            |row| row.get(0),
        )
        .expect("port revision");
    assert_ne!(port_id, parked.revision_id);
    let exec = latest_execution(&parked.state, &port_id);
    wait_until_not_ready(&parked.state, &exec.id).await;
    assert_ne!(
        parked.state.work_db.get_execution(&exec.id).unwrap().status,
        ExecutionStatus::Ready
    );
}

#[tokio::test]
async fn force_answer_keeps_admission_through_a_preflight_refusal_then_drains() {
    let parked = parked_mismatch();
    hold_pause_and_focus(&parked);
    parked
        .state
        .execution_coordinator
        .set_dispatch_preflight_block(Some("test: local dispatch unavailable".into()));
    let _ = answer(&parked, CONFLICT_MISMATCH_CHOICE_FORCE_MERGE).await;
    let exec = latest_execution(&parked.state, &parked.revision_id);
    assert_eq!(exec.status, ExecutionStatus::Ready);
    assert!(
        parked
            .state
            .work_db
            .execution_has_operator_forced_admission(&exec.id)
            .unwrap()
    );
    parked.state.execution_coordinator.set_dispatch_preflight_block(None);
    parked.state.execution_coordinator.drain_ready_queue_for_test().await;
    wait_until_not_ready(&parked.state, &exec.id).await;
}

#[tokio::test]
async fn force_answer_behind_a_writer_dispatches_once_the_row_is_ready() {
    let parked = parked_mismatch();
    let product_id = parked
        .state
        .work_db
        .get_work_item(&parked.revision_id)
        .unwrap()
        .product_id()
        .to_owned();
    let writer = parked
        .state
        .work_db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product_id)
                .project_id(parked.focused_project_id.clone())
                .name("Live writer")
                .autostart(false)
                .build(),
        )
        .unwrap();
    parked
        .state
        .work_db
        .connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&writer.id])
        .unwrap();
    boss_work_db::work_dependencies::insert_edge(
        &parked.state.work_db.connect().unwrap(),
        &parked.revision_id,
        &writer.id,
        boss_work_db::work_dependencies::RELATION_BLOCKS,
        "1",
    )
    .unwrap();
    hold_pause_and_focus(&parked);
    let _ = answer(&parked, CONFLICT_MISMATCH_CHOICE_FORCE_MERGE).await;
    let exec = latest_execution(&parked.state, &parked.revision_id);
    assert_eq!(exec.status, ExecutionStatus::WaitingDependency);
    assert!(
        parked
            .state
            .work_db
            .execution_has_operator_forced_admission(&exec.id)
            .unwrap()
    );
    parked
        .state
        .work_db
        .connect()
        .unwrap()
        .execute("UPDATE work_executions SET status = 'ready' WHERE id = ?1", [&exec.id])
        .unwrap();
    parked.state.execution_coordinator.drain_ready_queue_for_test().await;
    wait_until_not_ready(&parked.state, &exec.id).await;
}
