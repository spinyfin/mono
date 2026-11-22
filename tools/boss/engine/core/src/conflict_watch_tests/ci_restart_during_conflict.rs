//! An explicit start of a CI-fix revision whose attempt merge-conflict
//! detection superseded, made while that conflict is still being resolved.
//!
//! Conflict pre-empts CI (design §Q1): [`on_conflict_detected`] abandons any
//! active `ci_remediations` attempt and closes its revision on every tick
//! that reaches the supersede step. An explicit start re-opens a retired
//! attempt, so a start accepted in that window was undone by the very next
//! tick — the attempt abandoned again and the revision archived before a
//! worker could act on the owner's answer. The start is refused with the
//! reason instead, and the revision stays resumable.

use super::helpers::*;
use crate::conflict_remediation::{ConflictRemediationJob, ConflictRemediationQueue, RemediationDisposition};

/// Stands in for a mechanical ladder that has not concluded: the conflict
/// attempt stays `pending` with no revision, the state from which every
/// detection tick re-enters the supersede step.
struct UnfinishedLadder;

#[async_trait]
impl ConflictRemediationJob for UnfinishedLadder {
    async fn run(
        &self,
        _candidate: PendingMergeCheck,
        _attempt: crate::work::ConflictResolution,
        _probe: PrLifecycleProbe,
    ) -> RemediationDisposition {
        RemediationDisposition::RetryAfterCooldown
    }
}

/// One merge-poller detection pass that still sees the PR CONFLICTING, in
/// the mode the production detection path runs in.
async fn conflicting_tick(db: &WorkDb, queue: &ConflictRemediationQueue, product: &str, chore: &str, pr: &str) {
    let publisher = RecordingPublisher::default();
    on_conflict_detected_with(
        db,
        &publisher,
        ConflictRemediationMode::Deferred(queue),
        &open_checker(),
        &candidate(product, chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        ConflictDeferralArm::Off,
    )
    .await;
}

/// A `pending` CI-fix attempt for `chore` and the revision it spawned.
fn ci_fix_revision(db: &WorkDb, product: &str, chore: &str, pr: &str) -> (String, String) {
    let attempt = db
        .insert_ci_remediation(crate::work::CiRemediationInsertInput {
            product_id: product.to_owned(),
            work_item_id: chore.to_owned(),
            pr_url: pr.to_owned(),
            pr_number: 3079,
            head_branch: "feature".to_owned(),
            head_sha_at_trigger: "head-ci".to_owned(),
            attempt_kind: "fix".to_owned(),
            consumes_budget: 1,
            failed_checks: "[]".to_owned(),
            failure_kind: "pr_branch_ci".to_owned(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("fresh insert");
    let revision = db
        .create_revision(
            crate::work::CreateRevisionInput::builder()
                .parent_task_id(chore.to_owned())
                .description("Fix failing CI")
                .created_via(format!("{}{}", crate::work::CREATED_VIA_CI_FIX_PREFIX, attempt.id))
                .build(),
            &open_checker(),
        )
        .unwrap();
    db.set_ci_remediation_revision_task_id(&attempt.id, &revision.id)
        .unwrap();
    (attempt.id, revision.id)
}

fn ci_attempt_state(db: &WorkDb, attempt_id: &str) -> (String, Option<String>) {
    let attempt = db.get_ci_remediation(attempt_id).unwrap().expect("attempt row");
    (attempt.status, attempt.failure_reason)
}

fn explicit_start(db: &WorkDb, revision: &str) -> anyhow::Result<crate::work::WorkExecution> {
    db.request_execution(
        crate::work::RequestExecutionInput::builder()
            .work_item_id(revision)
            .explicit_start(true)
            .build(),
    )
}

fn runnable_executions(db: &WorkDb, revision: &str) -> Vec<String> {
    db.list_executions(Some(revision))
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.status, ExecutionStatus::Ready | ExecutionStatus::Running))
        .map(|e| e.id)
        .collect()
}

/// The full sequence: a CI-fix worker is live when the PR starts conflicting,
/// asks the owner a question, and the owner answers Yes while the conflict is
/// still being resolved. The answer must not cost the revision; once the
/// conflict is resolved an explicit start resumes it.
#[tokio::test]
async fn yes_answer_during_conflict_resolution_keeps_the_ci_fix_revision_resumable() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3079";
    let (product, chore) = make_in_review(&db, "C-ci-restart-during-conflict", pr);
    let (ci_attempt, revision) = ci_fix_revision(&db, &product, &chore, pr);
    let queue = ConflictRemediationQueue::new(Arc::new(UnfinishedLadder));

    // A worker is live on the CI-fix revision.
    db.reconcile_product_executions(&product).unwrap();
    let first = db.list_executions(Some(&revision)).unwrap();
    assert_eq!(first.len(), 1, "{first:?}");
    db.start_execution_run(&first[0].id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws-1")
        .unwrap();

    // The PR starts conflicting: conflict pre-empts CI, but the live worker
    // keeps its revision.
    conflicting_tick(&db, &queue, &product, &chore, pr).await;
    assert_eq!(
        ci_attempt_state(&db, &ci_attempt),
        ("abandoned".to_owned(), Some("superseded_by_conflict".to_owned())),
    );
    let conflict_attempt = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("conflict attempt");
    assert_eq!(conflict_attempt.revision_task_id, None, "the ladder has not concluded");
    assert_eq!(task(&db, &revision).status, TaskStatus::Active);

    // The worker ends its run on a question for the owner.
    let question = boss_protocol::OperatorQuestion {
        text: "Approve the file-count exception?".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
        explanation: "Fixing CI needs more files than the limit allows.".into(),
    };
    crate::work::insert_question(&db.connect().unwrap(), &first[0].id, "proposal", &question).unwrap();
    db.record_worker_awaiting_operator_answer(&first[0].id, "needs a decision", &question)
        .unwrap()
        .expect("live execution parks the revision");
    conflicting_tick(&db, &queue, &product, &chore, pr).await;
    db.reconcile_product_executions(&product).unwrap();
    let parked = task(&db, &revision);
    assert_eq!(parked.status, TaskStatus::Blocked);
    assert_eq!(parked.blocked_reason.as_deref(), Some("awaiting_operator_answer"));

    // The owner answers Yes while the conflict is still being resolved.
    let outcome = db
        .answer_operator_question(&revision, boss_protocol::OperatorAnswer::YesNo { value: true })
        .unwrap();
    conflicting_tick(&db, &queue, &product, &chore, pr).await;
    db.reconcile_product_executions(&product).unwrap();

    let after = task(&db, &revision);
    assert_ne!(
        after.status,
        TaskStatus::Archived,
        "answering Yes must not get the revision archived by the next conflict tick",
    );
    assert!(after.deleted_at.is_none(), "the revision must stay on the board");
    assert!(
        after.description.contains("**Answer:** Yes"),
        "the answer must be recorded on the revision: {}",
        after.description,
    );
    assert!(
        !outcome.minted_execution,
        "the start is refused while conflict resolution owns the PR, so nothing is minted",
    );
    assert_eq!(runnable_executions(&db, &revision), Vec::<String>::new());
    assert_eq!(
        ci_attempt_state(&db, &ci_attempt),
        ("abandoned".to_owned(), Some("superseded_by_conflict".to_owned())),
        "the superseded attempt must not be re-opened while the conflict attempt is active",
    );
    assert_eq!(after.status, TaskStatus::Blocked);
    assert_eq!(
        after.blocked_reason.as_deref(),
        Some(crate::work::BLOCKED_REASON_SPAWNING_ATTEMPT_RETIRED),
    );

    // A drag to Doing in the same window is refused, and says why.
    let refused = explicit_start(&db, &revision).expect_err("start must be refused during conflict resolution");
    let message = format!("{refused:#}");
    assert!(
        message.contains("merge-conflict resolution") && message.contains(&conflict_attempt.id),
        "the refusal must name the conflict attempt that pre-empts it: {message}",
    );
    assert_eq!(runnable_executions(&db, &revision), Vec::<String>::new());
    assert_eq!(ci_attempt_state(&db, &ci_attempt).0, "abandoned");

    // Once the conflict is resolved, the same explicit start resumes the
    // revision and the run survives the reconciler.
    db.mark_conflict_resolution_succeeded(&conflict_attempt.id, Some("head-resolved"))
        .unwrap()
        .expect("conflict attempt retires");
    let resumed = explicit_start(&db, &revision).expect("start is accepted once the conflict is resolved");
    assert_eq!(resumed.status, ExecutionStatus::Ready);
    assert_eq!(ci_attempt_state(&db, &ci_attempt), ("pending".to_owned(), None));
    db.reconcile_product_executions(&product).unwrap();
    assert_eq!(runnable_executions(&db, &revision), vec![resumed.id.clone()]);
    let resumed_task = task(&db, &revision);
    assert_eq!(resumed_task.status, TaskStatus::Todo);
    assert_eq!(resumed_task.blocked_reason, None);
}

/// Conflict detection abandons the CI attempt it pre-empts. The app shows
/// "ci failing" for as long as it holds that attempt as running, so the
/// abandoned row has to be published, carrying the cleared card state.
#[tokio::test]
async fn conflict_superseding_a_ci_attempt_publishes_the_abandoned_row() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3080";
    let (product, chore) = make_in_review(&db, "C-conflict-supersedes-ci", pr);
    let (ci_attempt, _revision) = ci_fix_revision(&db, &product, &chore, pr);
    let queue = ConflictRemediationQueue::new(Arc::new(UnfinishedLadder));
    let publisher = RecordingPublisher::default();

    on_conflict_detected_with(
        &db,
        &publisher,
        ConflictRemediationMode::Deferred(&queue),
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        ConflictDeferralArm::Off,
    )
    .await;

    let typed = publisher.typed_events.lock().await.clone();
    let pushed = typed
        .iter()
        .find_map(|(_, event)| match event {
            FrontendEvent::CiRemediationAbandoned { attempt, .. } if attempt.id == ci_attempt => Some(attempt.clone()),
            _ => None,
        })
        .expect("the superseded CI attempt must be published as abandoned");
    assert_eq!(pushed.status, "abandoned");
    assert_eq!(pushed.extra.get("card_state").map(String::as_str), Some("clear"));
}
