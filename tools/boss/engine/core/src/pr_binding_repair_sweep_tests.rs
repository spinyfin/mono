use std::sync::Mutex as StdMutex;

use boss_protocol::{
    CreateChoreInput, CreateExecutionInput, ExecutionKind, ExecutionStatus, FinishExecutionRunInput, TaskKind,
    TaskStatus, WorkItem,
};

use super::*;
use crate::test_support::{create_test_product_with_repo, open_db};
use crate::work::{FakePrStateChecker, PrOpenState};

const REPO: &str = "git@github.com:brianduff/flunge.git";
const ORIGIN: &str = "https://github.com/brianduff/flunge/pull/1757";
const OWN: &str = "https://github.com/brianduff/flunge/pull/1764";

/// A follow-up for origin #1757 whose completed implementation
/// execution was (wrongly) bound to #1757 — the incident row.
fn mis_bound_followup(db: &WorkDb, bound_pr_url: &str) -> (String, String, String) {
    let product = create_test_product_with_repo(db, "flunge", Some(REPO));
    let task = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Address post-merge review findings")
                .maybe_description(Some(format!("Found in post-merge review of {ORIGIN}.")))
                .autostart(false)
                .maybe_kind_override(Some(TaskKind::Followup))
                .maybe_origin_pr_number(Some(1757))
                .build(),
        )
        .unwrap();
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url(REPO)
                .build(),
        )
        .unwrap();
    let (exec, run) = db
        .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/workspaces/ws-1")
        .unwrap();
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&exec.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::Completed)
            .run_status("completed")
            .build(),
    )
    .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET pr_url = ?2, cube_lease_id = NULL, workspace_path = NULL WHERE id = ?1",
        rusqlite::params![exec.id, bound_pr_url],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET pr_url = ?2, status = 'todo', doc_branch = 'boss/exec_18dad2723a80ed10_23',
             doc_path = 'docs/designs/origin.md' WHERE id = ?1",
        rusqlite::params![task.id, bound_pr_url],
    )
    .unwrap();
    (product.id, task.id, exec.id)
}

struct MapVerifier(HashMap<u64, String>);

#[async_trait]
impl BranchVerifier for MapVerifier {
    async fn fetch_pr_head_ref(&self, _slug: &str, pr_number: u64) -> anyhow::Result<String> {
        self.0
            .get(&pr_number)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no head ref configured for PR {pr_number}"))
    }
    async fn fetch_pr_head_oid(&self, _: &str, _: u64) -> anyhow::Result<String> {
        Ok("oid".into())
    }
    async fn fetch_pr_head_oid_fresh(&self, _: &str, _: u64) -> anyhow::Result<String> {
        Ok("oid".into())
    }
    async fn fetch_diff_line_count(&self, _: &str, _: &str, _: &str) -> anyhow::Result<u64> {
        Ok(1)
    }
    async fn fetch_pr_base_ref(&self, _: &str, _: u64) -> anyhow::Result<String> {
        Ok("main".into())
    }
    async fn fetch_diff_signature(&self, _: &str, _: &str, _: &str) -> anyhow::Result<String> {
        Ok("sig".into())
    }
    async fn fetch_pr_title_and_body(&self, _: &str, _: u64) -> anyhow::Result<(String, String)> {
        Ok((String::new(), String::new()))
    }
}

struct FixedDetector(PrStatus, StdMutex<Vec<String>>);

#[async_trait]
impl PrDetector for FixedDetector {
    async fn detect_pr(&self, _repo: &str, expected_branch: &str) -> anyhow::Result<PrStatus> {
        self.1.lock().unwrap().push(expected_branch.to_owned());
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct RecordingDocRefresher(StdMutex<Vec<String>>);

#[async_trait]
impl DocPointerRefresher for RecordingDocRefresher {
    async fn scan(&self, _: &str, pr_url: &str) -> Option<crate::design_detector::PrScanResult> {
        self.0.lock().unwrap().push(pr_url.to_owned());
        Some(crate::design_detector::PrScanResult {
            doc_path: None,
            head_ref_name: None,
            base_ref_name: None,
        })
    }
}

fn task(db: &WorkDb, id: &str) -> boss_protocol::Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        other => panic!("expected task, got {other:?}"),
    }
}

#[tokio::test]
async fn followup_bound_to_its_origin_pr_is_rebound_from_its_branch_and_reviewed() {
    let (_dir, db) = open_db();
    let (_product, task_id, exec_id) = mis_bound_followup(&db, ORIGIN);
    let own_branch = expected_branch_name(&exec_id, &Default::default(), None);
    let verifier = MapVerifier(HashMap::from([(1757, "boss/exec_18dad2723a80ed10_23".to_owned())]));
    let detector = FixedDetector(PrStatus::Fresh { url: OWN.to_owned() }, StdMutex::default());
    let docs = RecordingDocRefresher::default();
    let mut attempts = HashMap::new();

    let outcome = run_one_pass(
        &db,
        &detector,
        &verifier,
        &FakePrStateChecker::always(PrOpenState::Open),
        &docs,
        &mut attempts,
    )
    .await;

    assert_eq!(outcome.rebound, 1, "{outcome:?}");
    assert_eq!(detector.1.lock().unwrap().as_slice(), [own_branch.as_str()]);
    let t = task(&db, &task_id);
    assert_eq!(t.pr_url.as_deref(), Some(OWN));
    assert_eq!(t.status, TaskStatus::InReview);
    assert!(t.description.contains("re-bound pr_url"));
    assert_eq!(db.get_execution(&exec_id).unwrap().pr_url.as_deref(), Some(OWN));
    assert_eq!(
        docs.0.lock().unwrap().as_slice(),
        [OWN],
        "doc pointer re-derived from the real PR"
    );
    assert!(
        db.list_executions(Some(&task_id))
            .unwrap()
            .iter()
            .any(|e| e.kind == ExecutionKind::PrReview && !e.status.is_terminal()),
        "a review of the re-bound PR must be requested",
    );
    // Idempotent: the row is no longer a candidate.
    assert!(db.list_pr_binding_repair_candidates().unwrap().is_empty());
}

#[tokio::test]
async fn no_pr_on_the_execution_branch_parks_the_row_visibly() {
    let (_dir, db) = open_db();
    let (_product, task_id, exec_id) = mis_bound_followup(&db, ORIGIN);
    let verifier = MapVerifier(HashMap::from([(1757, "boss/exec_18dad2723a80ed10_23".to_owned())]));
    let detector = FixedDetector(PrStatus::None, StdMutex::default());
    let mut attempts = HashMap::new();

    let outcome = run_one_pass(
        &db,
        &detector,
        &verifier,
        &FakePrStateChecker::always(PrOpenState::Open),
        &RecordingDocRefresher::default(),
        &mut attempts,
    )
    .await;

    assert_eq!(outcome.unresolved, 1, "{outcome:?}");
    let t = task(&db, &task_id);
    assert_eq!(t.pr_url, None, "the wrong binding must not survive");
    assert_eq!(t.status, TaskStatus::Blocked);
    assert_eq!(
        t.blocked_reason.as_deref(),
        Some(crate::work::BLOCKED_REASON_PR_BINDING_UNRESOLVED)
    );
    let detail = t.blocked_detail.unwrap_or_default();
    assert!(detail.contains(ORIGIN) && detail.contains(&expected_branch_name(&exec_id, &Default::default(), None)));
    assert_eq!(db.get_execution(&exec_id).unwrap().pr_url, None);
    assert!(
        db.list_attention_items_for_work_item(&task_id)
            .unwrap()
            .iter()
            .any(|a| a.kind == ATTENTION_KIND_PR_BINDING_REPAIR)
    );
}

#[tokio::test]
async fn correctly_bound_row_with_a_foreign_doc_branch_only_gets_its_doc_pointer_refreshed() {
    let (_dir, db) = open_db();
    // Already backfilled by hand to the right PR, but the doc pointer
    // still sits on the origin PR's branch — the two hand-repaired rows.
    let (_product, task_id, exec_id) = mis_bound_followup(&db, OWN);
    let own_branch = expected_branch_name(&exec_id, &Default::default(), None);
    let verifier = MapVerifier(HashMap::from([(1764, own_branch.clone())]));
    let detector = FixedDetector(PrStatus::None, StdMutex::default());
    let docs = RecordingDocRefresher::default();
    let mut attempts = HashMap::new();

    let outcome = run_one_pass(
        &db,
        &detector,
        &verifier,
        &FakePrStateChecker::always(PrOpenState::Open),
        &docs,
        &mut attempts,
    )
    .await;

    assert_eq!(outcome.doc_repaired, 1, "{outcome:?}");
    assert!(
        detector.1.lock().unwrap().is_empty(),
        "a correct binding is never re-detected"
    );
    assert_eq!(docs.0.lock().unwrap().as_slice(), [OWN]);
    let t = task(&db, &task_id);
    assert_eq!(t.pr_url.as_deref(), Some(OWN));
    assert_eq!(t.status, TaskStatus::Todo, "a correct binding leaves status alone");
    assert_eq!(
        db.task_doc_path(&task_id).unwrap(),
        None,
        "the doc path copied off the origin PR must be gone"
    );
    assert!(db.list_pr_binding_repair_candidates().unwrap().is_empty());
}

#[tokio::test]
async fn rebind_clears_the_foreign_doc_path_even_on_main() {
    let (_dir, db) = open_db();
    let (_product, task_id, _exec_id) = mis_bound_followup(&db, ORIGIN);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET doc_branch = 'main' WHERE id = ?1", [&task_id])
        .unwrap();
    let verifier = MapVerifier(HashMap::from([(1757, "boss/exec_18dad2723a80ed10_23".to_owned())]));
    let detector = FixedDetector(PrStatus::Fresh { url: OWN.to_owned() }, StdMutex::default());
    let mut attempts = HashMap::new();
    let outcome = run_one_pass(
        &db,
        &detector,
        &verifier,
        &FakePrStateChecker::always(PrOpenState::Open),
        &RecordingDocRefresher::default(),
        &mut attempts,
    )
    .await;
    assert_eq!(outcome.rebound, 1, "{outcome:?}");
    assert_eq!(db.task_doc_path(&task_id).unwrap(), None);
}

#[tokio::test]
async fn live_review_allows_only_the_doc_repair() {
    let (_dir, db) = open_db();
    let (_product, task_id, exec_id) = mis_bound_followup(&db, OWN);
    let own_branch = expected_branch_name(&exec_id, &Default::default(), None);
    db.request_pr_review(&task_id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    let verifier = MapVerifier(HashMap::from([(1764, own_branch)]));
    let detector = FixedDetector(PrStatus::None, StdMutex::default());
    let mut attempts = HashMap::new();
    let outcome = run_one_pass(
        &db,
        &detector,
        &verifier,
        &FakePrStateChecker::always(PrOpenState::Open),
        &RecordingDocRefresher::default(),
        &mut attempts,
    )
    .await;
    assert_eq!(outcome.doc_repaired, 1, "{outcome:?}");
    assert_eq!(db.task_doc_path(&task_id).unwrap(), None);
}

#[tokio::test]
async fn github_failures_are_retried_then_escalated_without_touching_the_row() {
    let (_dir, db) = open_db();
    let (_product, task_id, _exec_id) = mis_bound_followup(&db, ORIGIN);
    let verifier = MapVerifier(HashMap::new()); // every head-ref fetch errors
    let detector = FixedDetector(PrStatus::None, StdMutex::default());
    let mut attempts = HashMap::new();
    for pass in 1..=MAX_ATTEMPTS_PER_TASK + 1 {
        let outcome = run_one_pass(
            &db,
            &detector,
            &verifier,
            &FakePrStateChecker::always(PrOpenState::Open),
            &RecordingDocRefresher::default(),
            &mut attempts,
        )
        .await;
        let expected_inconclusive = usize::from(pass <= MAX_ATTEMPTS_PER_TASK);
        assert_eq!(outcome.inconclusive, expected_inconclusive, "pass {pass}: {outcome:?}");
    }
    let t = task(&db, &task_id);
    assert_eq!(
        t.pr_url.as_deref(),
        Some(ORIGIN),
        "an inconclusive pass never rewrites the row"
    );
    assert!(
        db.list_attention_items_for_work_item(&task_id)
            .unwrap()
            .iter()
            .any(|a| a.kind == ATTENTION_KIND_PR_BINDING_REPAIR)
    );
}
/// Second primary execution on `task_id` (a resume / bounce-back), newer
/// than the first and never pushed.
fn add_resumed_execution(db: &WorkDb, task_id: &str) -> String {
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task_id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Completed)
                .repo_remote_url(REPO)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = '9999-01-01T00:00:00Z' WHERE id = ?1",
            [&exec.id],
        )
        .unwrap();
    exec.id
}

#[tokio::test]
async fn resumed_row_bound_to_a_predecessor_executions_pr_is_left_untouched() {
    let (_dir, db) = open_db();
    let (_product, task_id, exec_a) = mis_bound_followup(&db, OWN);
    let branch_a = expected_branch_name(&exec_a, &Default::default(), None);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET doc_branch = ?2 WHERE id = ?1", [&task_id, &branch_a])
        .unwrap();
    let exec_b = add_resumed_execution(&db, &task_id);
    assert_ne!(exec_a, exec_b);
    let verifier = MapVerifier(HashMap::from([(1764, branch_a)]));
    let detector = FixedDetector(PrStatus::None, StdMutex::default());
    let docs = RecordingDocRefresher::default();
    let mut attempts = HashMap::new();

    assert!(
        db.list_pr_binding_repair_candidates().unwrap().is_empty(),
        "a doc pointer on the predecessor execution's branch is the row's own"
    );
    let outcome = run_one_pass(
        &db,
        &detector,
        &verifier,
        &FakePrStateChecker::always(PrOpenState::Open),
        &docs,
        &mut attempts,
    )
    .await;

    assert_eq!(outcome, PrBindingRepairOutcome::default());
    let t = task(&db, &task_id);
    assert_eq!(t.pr_url.as_deref(), Some(OWN));
    assert_eq!(t.status, TaskStatus::Todo);
    assert!(detector.1.lock().unwrap().is_empty());
}

#[tokio::test]
async fn resumed_row_whose_origin_binding_is_suspect_but_pr_is_a_predecessors_is_confirmed() {
    let (_dir, db) = open_db();
    // Bound to the origin PR number but its head is the predecessor's branch.
    let (_product, task_id, exec_a) = mis_bound_followup(&db, ORIGIN);
    add_resumed_execution(&db, &task_id);
    let verifier = MapVerifier(HashMap::from([(
        1757,
        expected_branch_name(&exec_a, &Default::default(), None),
    )]));
    let detector = FixedDetector(PrStatus::None, StdMutex::default());
    let mut attempts = HashMap::new();
    let outcome = run_one_pass(
        &db,
        &detector,
        &verifier,
        &FakePrStateChecker::always(PrOpenState::Open),
        &RecordingDocRefresher::default(),
        &mut attempts,
    )
    .await;
    assert_eq!(outcome.confirmed_ok + outcome.doc_repaired, 1, "{outcome:?}");
    assert_eq!(task(&db, &task_id).pr_url.as_deref(), Some(ORIGIN));
}

#[tokio::test]
async fn engine_closed_followup_bound_to_its_origin_pr_is_rebound_and_reopened() {
    let (_dir, db) = open_db();
    let (_product, task_id, _exec_id) = mis_bound_followup(&db, ORIGIN);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done', last_status_actor = 'engine', completed_at = 'now' WHERE id = ?1",
            [&task_id],
        )
        .unwrap();
    let verifier = MapVerifier(HashMap::from([(1757, "boss/exec_18dad2723a80ed10_23".to_owned())]));
    let detector = FixedDetector(PrStatus::Fresh { url: OWN.to_owned() }, StdMutex::default());
    let mut attempts = HashMap::new();
    let outcome = run_one_pass(
        &db,
        &detector,
        &verifier,
        &FakePrStateChecker::always(PrOpenState::Open),
        &RecordingDocRefresher::default(),
        &mut attempts,
    )
    .await;
    assert_eq!(outcome.rebound, 1, "{outcome:?}");
    let t = task(&db, &task_id);
    assert_eq!(t.pr_url.as_deref(), Some(OWN));
    assert_eq!(t.status, TaskStatus::InReview);
}

#[tokio::test]
async fn engine_archived_followup_bound_to_its_origin_pr_is_rebound_and_loses_archive_metadata() {
    let (_dir, db) = open_db();
    let (_product, task_id, _exec_id) = mis_bound_followup(&db, ORIGIN);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'archived', last_status_actor = 'engine', completed_at = 'now',
                 archived_by = 'engine', archived_at = 'then', archived_reason = 'merged' WHERE id = ?1",
            [&task_id],
        )
        .unwrap();
    let verifier = MapVerifier(HashMap::from([(1757, "boss/exec_18dad2723a80ed10_23".to_owned())]));
    let detector = FixedDetector(PrStatus::Fresh { url: OWN.to_owned() }, StdMutex::default());
    let mut attempts = HashMap::new();
    let outcome = run_one_pass(
        &db,
        &detector,
        &verifier,
        &FakePrStateChecker::always(PrOpenState::Open),
        &RecordingDocRefresher::default(),
        &mut attempts,
    )
    .await;
    assert_eq!(outcome.rebound, 1, "{outcome:?}");
    let t = task(&db, &task_id);
    assert_eq!(t.pr_url.as_deref(), Some(OWN));
    assert_eq!(t.status, TaskStatus::InReview);
    let archive: (Option<String>, Option<String>, Option<String>) = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT archived_by, archived_at, archived_reason FROM tasks WHERE id = ?1",
            [&task_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(archive, (None, None, None));
}

#[tokio::test]
async fn human_closed_row_bound_to_its_origin_pr_is_not_a_candidate() {
    let (_dir, db) = open_db();
    let (_product, task_id, _exec_id) = mis_bound_followup(&db, ORIGIN);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done', last_status_actor = 'human' WHERE id = ?1",
            [&task_id],
        )
        .unwrap();
    assert!(db.list_pr_binding_repair_candidates().unwrap().is_empty());
}

struct RetryDocScanner(std::sync::atomic::AtomicBool);
#[async_trait]
impl DocPointerRefresher for RetryDocScanner {
    async fn scan(&self, _: &str, _: &str) -> Option<crate::design_detector::PrScanResult> {
        self.0
            .swap(true, std::sync::atomic::Ordering::SeqCst)
            .then(|| crate::design_detector::PrScanResult {
                doc_path: Some("docs/designs/own.md".into()),
                head_ref_name: Some("main".into()),
                base_ref_name: Some("main".into()),
            })
    }
}

#[tokio::test]
async fn failed_doc_scan_preserves_signal_and_pointer_until_retry_succeeds() {
    for bound in [OWN, ORIGIN] {
        let (_dir, db) = open_db();
        let (_, task_id, exec_id) = mis_bound_followup(&db, bound);
        let verifier = MapVerifier(HashMap::from([
            (1764, expected_branch_name(&exec_id, &Default::default(), None)),
            (1757, "boss/exec_foreign".into()),
        ]));
        let detector = FixedDetector(PrStatus::Fresh { url: OWN.into() }, StdMutex::default());
        let docs = RetryDocScanner(std::sync::atomic::AtomicBool::new(false));
        let mut attempts = HashMap::new();
        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let first = run_one_pass(&db, &detector, &verifier, &checker, &docs, &mut attempts).await;
        assert_eq!(first.inconclusive, 1);
        assert_eq!(first.doc_repaired + first.rebound, 0);
        assert_eq!(task(&db, &task_id).pr_url.as_deref(), Some(bound));
        assert_eq!(
            db.task_doc_path(&task_id).unwrap().as_deref(),
            Some("docs/designs/origin.md")
        );
        assert_eq!(db.list_pr_binding_repair_candidates().unwrap().len(), 1);
        let second = run_one_pass(&db, &detector, &verifier, &checker, &docs, &mut attempts).await;
        assert_eq!(second.inconclusive, 0);
        assert_eq!(second.doc_repaired + second.rebound, 1);
        assert_eq!(
            db.task_doc_path(&task_id).unwrap().as_deref(),
            Some("docs/designs/own.md")
        );
        assert!(db.list_pr_binding_repair_candidates().unwrap().is_empty());
    }
}

#[tokio::test]
async fn closed_rows_sharing_legacy_pr_remain_untouched_across_passes() {
    for status in ["done", "archived"] {
        let (_dir, db) = open_db();
        let (_, first, _) = mis_bound_followup(&db, OWN);
        let (_, second, _) = mis_bound_followup(&db, OWN);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = ?1, last_status_actor = 'engine', completed_at = 'then',
             autostart = 1, origin_pr_number = NULL",
                [status],
            )
            .unwrap();
        let before = [task(&db, &first), task(&db, &second)];
        assert_eq!(db.list_pr_binding_repair_candidates().unwrap().len(), 2);
        let verifier = MapVerifier(HashMap::from([(1764, "live-status-toggle-ui".into())]));
        let detector = FixedDetector(PrStatus::None, StdMutex::default());
        let docs = RecordingDocRefresher::default();
        let mut attempts = HashMap::new();
        for _ in 0..=MAX_ATTEMPTS_PER_TASK {
            let outcome = run_one_pass(
                &db,
                &detector,
                &verifier,
                &FakePrStateChecker::always(PrOpenState::Merged),
                &docs,
                &mut attempts,
            )
            .await;
            assert_eq!(
                outcome,
                PrBindingRepairOutcome {
                    inconclusive: 2,
                    ..Default::default()
                }
            );
            for original in &before {
                assert_eq!(
                    serde_json::to_value(task(&db, &original.id)).unwrap(),
                    serde_json::to_value(original).unwrap()
                );
                assert!(db.list_attention_items_for_work_item(&original.id).unwrap().is_empty());
            }
        }
        assert!(attempts.is_empty(), "legacy heads are not GitHub failures");
        assert!(detector.1.lock().unwrap().is_empty());
        assert!(docs.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn closed_shared_binding_to_another_rows_execution_is_still_repaired() {
    let (_dir, db) = open_db();
    let (_, wrong, _) = mis_bound_followup(&db, ORIGIN);
    let (_, owner, owner_exec) = mis_bound_followup(&db, ORIGIN);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done', last_status_actor = 'engine',
         completed_at = 'then', origin_pr_number = NULL",
            [],
        )
        .unwrap();
    let verifier = MapVerifier(HashMap::from([(
        1757,
        expected_branch_name(&owner_exec, &Default::default(), None),
    )]));
    let detector = FixedDetector(PrStatus::Fresh { url: OWN.into() }, StdMutex::default());
    let outcome = run_one_pass(
        &db,
        &detector,
        &verifier,
        &FakePrStateChecker::always(PrOpenState::Open),
        &RecordingDocRefresher::default(),
        &mut HashMap::new(),
    )
    .await;
    assert_eq!(outcome.rebound, 1);
    assert_eq!(outcome.confirmed_ok, 1);
    assert_eq!(task(&db, &wrong).status, TaskStatus::InReview);
    assert_eq!(task(&db, &wrong).pr_url.as_deref(), Some(OWN));
    assert_eq!(task(&db, &owner).status, TaskStatus::Done);
    assert_eq!(task(&db, &owner).pr_url.as_deref(), Some(ORIGIN));
}

#[tokio::test]
async fn missing_primary_branches_cannot_prove_a_binding_wrong() {
    let (_dir, db) = open_db();
    let (_, id, _) = mis_bound_followup(&db, ORIGIN);
    let mut candidate = db.list_pr_binding_repair_candidates().unwrap().remove(0);
    candidate.own_branches.clear();
    let detector = FixedDetector(PrStatus::None, StdMutex::default());
    let mut outcome = PrBindingRepairOutcome::default();
    assert!(
        repair_candidate(
            &db,
            &detector,
            &MapVerifier(HashMap::new()),
            &FakePrStateChecker::always(PrOpenState::Open),
            &RecordingDocRefresher::default(),
            &candidate,
            &mut outcome,
        )
        .await
        .is_ok()
    );
    assert_eq!(outcome.inconclusive, 1);
    assert_eq!(task(&db, &id).pr_url.as_deref(), Some(ORIGIN));
    assert!(detector.1.lock().unwrap().is_empty());
}
