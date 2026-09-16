use super::*;
use crate::test_support::{create_test_chore_manual, create_test_product_with_repo};
use crate::work::{CreateExecutionInput, PrOpenState, PrStateChecker, ReviewVerdictInput, WorkItemPatch};
use boss_protocol::{ReviewBatch, ReviewBatchMemberRole, ReviewBatchPhase, ReviewBatchStatus, WorkExecution};

struct OpenPr;
impl PrStateChecker for OpenPr {
    fn check(&self, _: &str) -> anyhow::Result<PrOpenState> {
        Ok(PrOpenState::Open)
    }
}

fn state(fanout: bool) -> (Arc<ServerState>, tempfile::TempDir) {
    let (state, dir) = super::super::tests::test_server_state_with_fakes();
    state.feature_flags.set("review_batch_fanout", fanout).unwrap();
    (state, dir)
}

fn seed(state: &ServerState, repo: &str, number: i64) -> String {
    let db = &state.work_db;
    let product = create_test_product_with_repo(db, repo, Some(&format!("https://github.com/{repo}")));
    let task = create_test_chore_manual(db, product.id, format!("explicit review {number}"));
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(format!("https://github.com/{repo}/pull/{number}")),
            ..Default::default()
        },
    )
    .unwrap();
    task.id
}

fn metadata(head: &str) -> serde_json::Value {
    serde_json::json!({
        "state": "OPEN", "headRefOid": head, "baseRefOid": "base",
        "files": [{"path": "tools/boss/engine/core/src/lib.rs"}],
        "additions": 0, "deletions": 0
    })
}

async fn request(
    state: &Arc<ServerState>,
    number: i64,
    repo: Option<&str>,
    metadata: anyhow::Result<serde_json::Value>,
) -> FrontendEvent {
    let (shutdown, _rx) = oneshot::channel();
    let sink = Arc::new(SessionSink::new(shutdown));
    let ctx = Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("review-test")
        .request_id("review-request")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build();
    handle_trigger_pr_review_with(
        ctx,
        FrontendRequest::TriggerPrReview {
            pr_number: number,
            repo: repo.map(str::to_owned),
        },
        &OpenPr,
        |_| async move { metadata },
    )
    .await;
    sink.next().await.unwrap().payload
}

fn triggered(event: FrontendEvent) -> WorkExecution {
    match event {
        FrontendEvent::PrReviewTriggered { execution, .. } => execution,
        other => panic!("expected successful review start, got {other:?}"),
    }
}

fn rejected(event: FrontendEvent, reason: &str) {
    match event {
        FrontendEvent::WorkError { message } => assert!(message.contains(reason), "{message}"),
        other => panic!("expected review error containing {reason:?}, got {other:?}"),
    }
}

fn batch(state: &ServerState, id: &str, head: &str) -> ReviewBatch {
    state
        .work_db
        .review_batch_for_target(id, ReviewBatchPhase::PreMerge, head)
        .unwrap()
        .unwrap()
}

fn record_review(state: &ServerState, id: &str, head: &str) {
    let db = &state.work_db;
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    WorkDb::insert_review_verdict_in_tx(
        &db.connect().unwrap(),
        &execution.id,
        id,
        &ReviewVerdictInput {
            head_sha: Some(head.to_owned()),
            findings_count: 0,
            revision_warranted: false,
            gate_outcome: crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
        },
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET review_cycle = 999, last_reviewed_sha = ?1 WHERE id = ?2",
            rusqlite::params![head, id],
        )
        .unwrap();
}

#[tokio::test]
async fn flag_on_creates_atomic_heterogeneous_batch_and_supervisor() {
    let (state, _dir) = state(true);
    let id = seed(&state, "example/repo", 42);
    let execution = triggered(request(&state, 42, None, Ok(metadata("head"))).await);
    let batch = batch(&state, &id, "head");
    assert_eq!(batch.generation, 1);
    let db = &state.work_db;
    let members = db.review_batch_members(&batch.id).unwrap();
    assert_eq!(members.len(), 3);
    assert_eq!(
        members.iter().map(|m| m.requested_driver.as_str()).collect::<Vec<_>>(),
        ["claude", "codex", "grok"]
    );
    assert!(members.iter().any(|m| m.execution_id.as_deref() == Some(&execution.id)));
    for member in &members {
        assert_eq!(member.created_at, batch.created_at);
        let execution_id = member.execution_id.as_deref().unwrap();
        let leaf = db.get_execution(execution_id).unwrap();
        assert_eq!(leaf.created_at, batch.created_at);
        assert_eq!(leaf.kind, ExecutionKind::PrReview);
        assert_eq!(leaf.status, ExecutionStatus::Ready);
        let payload = serde_json::json!({
            "batch_id": batch.id, "target_sha": "head",
            "report": {"batch_id": batch.id, "pr_url": batch.pr_url, "target_sha": "head",
                "phase": "pre_merge", "summary": "Clean", "coverage": {
                    "files_inspected": [], "files_omitted": [], "limitations": []}, "findings": []}
        })
        .to_string();
        db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
            execution_id,
            work_item_id: &id,
            kind: boss_protocol::ProposalKind::ReviewReport,
            payload_json: &payload,
            idempotency_key: execution_id,
        })
        .unwrap()
        .unwrap();
    }
    db.try_advance_review_batch_quorum(&batch.id).unwrap();
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Supervising
    );
    let members = db.review_batch_members(&batch.id).unwrap();
    assert_eq!(members.len(), 4);
    let supervisor = members
        .iter()
        .find(|m| m.role == ReviewBatchMemberRole::Supervisor)
        .unwrap();
    assert_eq!(
        db.get_execution(supervisor.execution_id.as_deref().unwrap())
            .unwrap()
            .status,
        ExecutionStatus::Ready
    );
}

#[tokio::test]
async fn explicit_start_ignores_prior_head_noop_and_cycle_limit() {
    for reviewed_head in ["earlier-head", "head"] {
        let (state, _dir) = state(true);
        let id = seed(&state, "example/repo", 42);
        record_review(&state, &id, reviewed_head);
        // Zero changed lines and an exhausted cycle count must not suppress an explicit request.
        triggered(request(&state, 42, None, Ok(metadata("head"))).await);
        assert_eq!(
            state
                .work_db
                .review_batch_members(&batch(&state, &id, "head").id)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(state.work_db.get_task_review_cycle_state(&id).unwrap().0, 999);
    }
}

#[tokio::test]
async fn completed_head_gets_new_generation_but_automatic_and_live_replays_do_not() {
    let (state, _dir) = state(true);
    let id = seed(&state, "example/repo", 42);
    let first_execution = triggered(request(&state, 42, None, Ok(metadata("head"))).await);
    let first = batch(&state, &id, "head");
    assert_eq!(
        triggered(request(&state, 42, None, Ok(metadata("head"))).await).id,
        first_execution.id
    );
    let db = &state.work_db;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE work_item_id = ?1",
            [&id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batch_members SET status = 'reported' WHERE batch_id = ?1",
            [&first.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'completed', completed_at = '123' WHERE id = ?1",
            [&first.id],
        )
        .unwrap();
    record_review(&state, &id, "head");
    let old_batch = db.review_batch(&first.id).unwrap().unwrap();
    let old_members = db.review_batch_members(&first.id).unwrap();
    let old_executions: Vec<_> = old_members
        .iter()
        .map(|m| db.get_execution(m.execution_id.as_deref().unwrap()).unwrap())
        .collect();
    let input = crate::completion::review_batch_input_from_metadata(db, &id, &first.pr_url, metadata("head")).unwrap();
    // The exact shared admission used by the automatic post-push enqueuer stays idempotent even after completion.
    match db
        .create_pre_merge_review_batch_for_pool(input.clone(), "https://github.com/example/repo", 16)
        .unwrap()
    {
        crate::work::ReviewBatchDispatch::ExistingBatch { batch, .. } => assert_eq!(batch, old_batch),
        other => panic!("automatic start must reuse a completed batch: {other:?}"),
    }
    triggered(request(&state, 42, None, Ok(metadata("head"))).await);
    let second = batch(&state, &id, "head");
    assert_ne!(second.id, first.id);
    assert_eq!(second.generation, 2);
    assert_eq!(db.review_batch_members(&second.id).unwrap().len(), 3);
    assert_eq!(db.review_batch(&first.id).unwrap().unwrap(), old_batch);
    assert_eq!(db.review_batch_members(&first.id).unwrap(), old_members);
    for before in old_executions {
        assert_eq!(
            serde_json::to_value(db.get_execution(&before.id).unwrap()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
    }
    match db
        .create_pre_merge_review_batch_for_pool(input, "https://github.com/example/repo", 16)
        .unwrap()
    {
        crate::work::ReviewBatchDispatch::ExistingBatch { batch, .. } => assert_eq!(batch, second),
        other => panic!("automatic start must reuse the latest generation: {other:?}"),
    }
    assert_eq!(db.review_batches_for_cycle_root(&id).unwrap().len(), 2);
}

#[tokio::test]
async fn capacity_failure_returns_work_error_without_any_execution() {
    let (state, _dir) = state(true);
    // The shared fixture configures one physical slot; admission reserves at
    // least four units so even a small pool can schedule a complete batch.
    let capacity = i64::from(state.review_pool_size).max(4);
    for number in 1..=capacity / 4 {
        seed(&state, "example/repo", number);
        triggered(request(&state, number, None, Ok(metadata("head"))).await);
    }
    assert_eq!(state.work_db.review_pool_reserved_units().unwrap().0, capacity);
    let id = seed(&state, "example/repo", 42);
    rejected(
        request(&state, 42, None, Ok(metadata("head"))).await,
        "reservation capacity exhausted",
    );
    assert!(state.work_db.review_batches_for_cycle_root(&id).unwrap().is_empty());
    assert!(state.work_db.list_executions(Some(&id)).unwrap().is_empty());
}

#[tokio::test]
async fn missing_metadata_and_closed_pr_never_fall_back() {
    let (state, _dir) = state(true);
    let id = seed(&state, "example/repo", 42);
    rejected(
        request(&state, 42, None, Err(anyhow::anyhow!("GitHub unavailable"))).await,
        "PR metadata unavailable: GitHub unavailable",
    );
    let mut closed = metadata("head");
    closed["state"] = "CLOSED".into();
    rejected(request(&state, 42, None, Ok(closed)).await, "PR is CLOSED");
    rejected(
        request(&state, 42, None, Ok(metadata(""))).await,
        "omitted immutable base or head SHA",
    );
    assert!(state.work_db.list_executions(Some(&id)).unwrap().is_empty());
    assert!(state.work_db.review_batches_for_cycle_root(&id).unwrap().is_empty());
}

#[tokio::test]
async fn flag_off_keeps_legacy_row_and_does_not_fetch_batch_metadata() {
    let (state, _dir) = state(false);
    let id = seed(&state, "example/repo", 42);
    let execution = triggered(request(&state, 42, None, Err(anyhow::anyhow!("must not fetch"))).await);
    assert_eq!(
        serde_json::to_value(&execution).unwrap(),
        serde_json::to_value(state.work_db.request_pr_review(&id, &OpenPr).unwrap()).unwrap()
    );
    assert_eq!(execution.kind, ExecutionKind::PrReview);
    assert_eq!(execution.status, ExecutionStatus::Ready);
    assert_eq!(state.work_db.list_executions(Some(&id)).unwrap().len(), 1);
    assert!(state.work_db.review_batches_for_cycle_root(&id).unwrap().is_empty());
    assert!(
        state
            .work_db
            .review_batch_member_for_execution(&execution.id)
            .unwrap()
            .is_none()
    );
    state.feature_flags.set("review_batch_fanout", true).unwrap();
    rejected(request(&state, 42, None, Ok(metadata("head"))).await, "legacy reviewer");
    assert_eq!(state.work_db.list_executions(Some(&id)).unwrap().len(), 1);
}

#[tokio::test]
async fn repo_substring_disambiguates_identical_pr_numbers() {
    let (state, _dir) = state(true);
    let first = seed(&state, "example/first", 42);
    let second = seed(&state, "example/second", 42);
    rejected(
        request(&state, 42, None, Ok(metadata("head"))).await,
        "ambiguous across 2 repos",
    );
    let execution = triggered(request(&state, 42, Some("second"), Ok(metadata("head"))).await);
    assert_eq!(execution.work_item_id, second);
    assert!(state.work_db.review_batches_for_cycle_root(&first).unwrap().is_empty());
    assert_eq!(state.work_db.review_batches_for_cycle_root(&second).unwrap().len(), 1);
}
