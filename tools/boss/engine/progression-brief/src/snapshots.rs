//! Snapshot tests: one representative item per eligible stuck shape.
//!
//! IDs and timestamps are normalized so the assertions pin the diagnosis
//! packet's wording rather than minted tokens.

use boss_engine_storage_values::attention_kinds::{
    ATTENTION_KIND_ABANDONED_BRANCH_NO_PR, NUDGE_BREAKER_ATTENTION_KIND, RUN_DONE_BLOCKED_ATTENTION_KIND,
};
use boss_protocol::{
    AddDependencyInput, CreateAttentionItemInput, CreateExecutionInput, ExecutionKind, ExecutionStatus,
    FinishExecutionRunInput, WorkItemPatch,
};
use boss_work_db::{
    CiRemediationInsertInput, ConflictResolutionInsertInput, PrPollStateInput, WorkDb, WorkerFailureBasis,
    test_support::{create_test_chore_manual, create_test_product_named, open_db},
};
use regex::Regex;

use crate::compose_progression_brief;

const REQUIRED_SECTIONS: &[&str] = &[
    "# Progression diagnosis",
    "## Item",
    "## Ledger (last 3 status changes)",
    "## Dependencies",
    "## Executions (item and revision chain)",
    "## Declared blocker (2.9)",
    "## PR",
    "## Open attentions",
    "## Operator questions",
    "## Progression history",
    "Current fingerprint:",
    "Prior fingerprint actions: none recorded",
    "Remaining redispatch allowance: 1 (no prior redispatch recorded)",
    "## Verb catalogue",
    "## When you may ask the owner",
    "The action the item needs is on the human-only list",
    "Do not restate a worker's free-text blocker as a question without first classifying it",
];

struct Fixture {
    _dir: tempfile::TempDir,
    db: WorkDb,
    product: String,
}

fn fixture() -> Fixture {
    let (dir, db) = open_db();
    let product = create_test_product_named(&db, "ProgressionBrief").id;
    Fixture { _dir: dir, db, product }
}

impl Fixture {
    fn chore(&self, name: &str) -> String {
        create_test_chore_manual(&self.db, &self.product, name).id
    }

    fn sql(&self, sql: &str) {
        self.db.connect().unwrap().execute_batch(sql).unwrap();
    }

    fn stick(&self, id: &str, assignments: &str) {
        self.sql(&format!("UPDATE tasks SET {assignments} WHERE id = '{id}'"));
    }

    fn bind_pr(&self, id: &str, number: i64) {
        let url = format!("https://github.com/spinyfin/mono/pull/{number}");
        self.db
            .update_work_item(
                id,
                WorkItemPatch {
                    pr_url: Some(url),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    fn brief(&self, id: &str) -> String {
        normalize(&compose_progression_brief(&self.db, id).unwrap())
    }
}

fn normalize(text: &str) -> String {
    let timestamps = Regex::new(r"\b1[0-9]{9}\b").unwrap();
    let ids = Regex::new(r"\b([a-z]+_)[0-9a-f]{8,}\b").unwrap();
    let fingerprint = Regex::new(r"(Current fingerprint: `)[0-9a-f]{16}(`)").unwrap();
    let text = timestamps.replace_all(text, "<TS>");
    let text = ids.replace_all(&text, "$1ID");
    fingerprint.replace_all(&text, "${1}<FINGERPRINT>${2}").into_owned()
}

fn assert_shape(name: &str, text: &str, needles: &[&str]) {
    for section in REQUIRED_SECTIONS {
        assert!(
            text.contains(section),
            "{name}: missing required section {section:?}\n{text}"
        );
    }
    for needle in needles {
        assert!(text.contains(needle), "{name}: missing {needle:?}\n{text}");
    }
}

#[test]
fn s1_dispatch_failure() {
    let f = fixture();
    let id = f.chore("Lease failed");
    f.sql(&format!("UPDATE tasks SET status = 'active' WHERE id = '{id}'"));
    assert!(
        f.db.bounce_dispatch_failed_to_backlog(&id, "cube_workspace_lease_failed", "cube refused the lease")
            .unwrap()
    );
    let text = f.brief(&id);
    assert_shape(
        "S1",
        &text,
        &[
            "stuck: S1_dispatch_failure",
            "dispatch_failed_reason: cube_workspace_lease_failed",
            "dispatch_failed_error: cube refused the lease",
            "Declared blocker (2.9)\n\n(none)",
        ],
    );
}

#[test]
fn s2_churn_guard() {
    let f = fixture();
    let id = f.chore("Churned");
    f.sql(&format!("UPDATE tasks SET status = 'active' WHERE id = '{id}'"));
    assert!(
        f.db.bounce_dispatch_failed_to_backlog(&id, "churn_guard", "three unproductive runs")
            .unwrap()
    );
    let text = f.brief(&id);
    assert_shape(
        "S2",
        &text,
        &[
            "stuck: S2_churn_guard",
            "dispatch_failed_reason: churn_guard",
            "three unproductive runs",
        ],
    );
}

#[test]
fn s3_deliberate_park_verbatim_blocker() {
    let f = fixture();
    let id = f.chore("Parked");
    f.sql(&format!("UPDATE tasks SET status = 'active' WHERE id = '{id}'"));
    assert!(f.db.bounce_deliberate_park_to_backlog(&id, "the worker said it needs a human on the API shape", None));
    let text = f.brief(&id);
    assert_shape(
        "S3",
        &text,
        &[
            "stuck: S3_deliberate_park",
            "Producer: deliberate_park",
            "the worker said it needs a human on the API shape",
        ],
    );
}

#[test]
fn s5_merge_conflict_with_diagnosis() {
    let f = fixture();
    let id = f.chore("Conflicted");
    f.bind_pr(&id, 41);
    f.stick(&id, "status = 'blocked', blocked_reason = 'merge_conflict'");
    let attempt =
        f.db.insert_conflict_resolution(
            ConflictResolutionInsertInput::builder()
                .product_id(&f.product)
                .work_item_id(&id)
                .pr_url("https://github.com/spinyfin/mono/pull/41")
                .pr_number(41)
                .head_branch("boss/conflicted")
                .base_branch("main")
                .base_sha_at_trigger("aaa111")
                .head_sha_before("bbb222")
                .build(),
        )
        .unwrap()
        .expect("conflict row");
    let diagnosis = serde_json::json!({
        "schema_version": 1,
        "base_sha": "aaa111",
        "head_sha": "bbb222",
        "files": [{"path": "src/lib.rs", "shape": "content", "marker_count": 2}],
        "error": null
    });
    f.db.set_conflict_resolution_diagnosis(&attempt.id, &diagnosis.to_string())
        .unwrap();
    let text = f.brief(&id);
    assert_shape(
        "S5",
        &text,
        &[
            "stuck: S5_merge_conflict",
            "blocked_reason: merge_conflict",
            "### Conflict resolutions",
            "src/lib.rs (content)",
        ],
    );
}

#[test]
fn s6_ci_failure_check_history_flags_unrelated() {
    let f = fixture();
    let id = f.chore("Exhausted CI");
    f.bind_pr(&id, 50);
    let other = f.chore("Sibling PR");
    f.bind_pr(&other, 51);
    let detail = r#"[{"name":"bazel-test","conclusion":"failure"}]"#;
    f.db.update_task_pr_poll_state(
        &id,
        PrPollStateInput {
            ci_required_state: "fail",
            review_required_state: "",
            ci_required_detail: Some(detail),
            pr_mergeable_state: "MERGEABLE",
            pr_head_sha: Some("deadbeef"),
            ..Default::default()
        },
    )
    .unwrap();
    f.db.update_task_pr_poll_state(
        &other,
        PrPollStateInput {
            ci_required_state: "fail",
            review_required_state: "",
            ci_required_detail: Some(detail),
            pr_mergeable_state: "MERGEABLE",
            pr_head_sha: Some("cafebabe"),
            ..Default::default()
        },
    )
    .unwrap();
    f.stick(&id, "status = 'blocked', blocked_reason = 'ci_failure_exhausted'");
    let attempt =
        f.db.insert_ci_remediation(CiRemediationInsertInput {
            product_id: f.product.clone(),
            work_item_id: id.clone(),
            pr_url: "https://github.com/spinyfin/mono/pull/50".into(),
            pr_number: 50,
            head_branch: "boss/ci".into(),
            head_sha_at_trigger: "deadbeef".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: detail.into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("ci row");
    f.db.set_ci_remediation_log_excerpt(&attempt.id, "FAIL: tools/boss/engine/core/src/lib.rs unused import")
        .unwrap();
    let text = f.brief(&id);
    assert_shape(
        "S6",
        &text,
        &[
            "stuck: S6_ci_failure",
            "blocked_reason: ci_failure_exhausted",
            "### CI remediations",
            "FAIL: tools/boss/engine/core/src/lib.rs unused import",
            "`bazel-test` failed on this PR and on 1 other open/recent PRs (Sibling PR) → likely unrelated or flaky.",
        ],
    );
}

fn start_then_fail(f: &Fixture, name: &str, basis: WorkerFailureBasis, detail: &str) -> String {
    let id = f.chore(name);
    f.sql(&format!("UPDATE tasks SET status = 'active' WHERE id = '{id}'"));
    let execution =
        f.db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (execution, run) =
        f.db.start_execution_run(&execution.id, "agent-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
            .unwrap();
    f.db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&execution.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::WaitingHuman)
            .run_status("completed")
            .build(),
    )
    .unwrap();
    f.db.record_worker_failure_with_basis(&execution.id, detail, basis)
        .unwrap();
    id
}

#[test]
fn s7_declared_blocked_verbatim() {
    let f = fixture();
    let id = start_then_fail(
        &f,
        "Declared",
        WorkerFailureBasis::DeclaredBlocked,
        "need a decision on the retry budget",
    );
    f.db.create_attention_item(
        CreateAttentionItemInput::builder()
            .work_item_id(&id)
            .kind(RUN_DONE_BLOCKED_ATTENTION_KIND)
            .title("Worker declared blocked")
            .body_markdown("need a decision on the retry budget")
            .build(),
    )
    .unwrap();
    let text = f.brief(&id);
    assert_shape(
        "S7 declared",
        &text,
        &[
            "stuck: S7_worker_failed",
            "failure_basis: declared_blocked",
            "Producer: declared_blocked",
            "need a decision on the retry budget",
        ],
    );
}

#[test]
fn s7_nudge_breaker_verbatim() {
    let f = fixture();
    let id = start_then_fail(
        &f,
        "Nudged",
        WorkerFailureBasis::NudgeBreaker,
        "nudge loop bounded after silence",
    );
    f.db.create_attention_item(
        CreateAttentionItemInput::builder()
            .work_item_id(&id)
            .kind(NUDGE_BREAKER_ATTENTION_KIND)
            .title("Nudge breaker")
            .body_markdown("nudge loop bounded after silence")
            .build(),
    )
    .unwrap();
    let text = f.brief(&id);
    assert_shape(
        "S7 nudge",
        &text,
        &[
            "stuck: S7_worker_failed",
            "failure_basis: nudge_breaker",
            "Producer: nudge_breaker",
            "nudge loop bounded after silence",
        ],
    );
}

#[test]
fn s9_execution_recovery() {
    let f = fixture();
    let id = f.chore("Bookmark failed");
    f.stick(&id, "status = 'blocked', blocked_reason = 'execution_recovery_failed'");
    let text = f.brief(&id);
    assert_shape(
        "S9",
        &text,
        &[
            "stuck: S9_execution_recovery",
            "blocked_reason: execution_recovery_failed",
        ],
    );
}

#[test]
fn s11_pr_binding() {
    let f = fixture();
    let id = f.chore("Unbound PR");
    f.stick(&id, "status = 'blocked', blocked_reason = 'pr_binding_unresolved'");
    let text = f.brief(&id);
    assert_shape(
        "S11",
        &text,
        &["stuck: S11_pr_binding", "blocked_reason: pr_binding_unresolved"],
    );
}

#[test]
fn s13_revision_chain_tail() {
    let f = fixture();
    let parent = f.chore("Parent green");
    f.bind_pr(&parent, 90);
    f.stick(&parent, "status = 'in_review', ci_required_state = 'success'");
    let revision = f.chore("Dead revision");
    f.sql(&format!(
        "UPDATE tasks SET kind = 'revision', parent_task_id = '{parent}',
                status = 'blocked', blocked_reason = 'worker_failed'
         WHERE id = '{revision}'"
    ));
    f.db.add_dependency(AddDependencyInput {
        dependent: parent.clone(),
        prerequisite: revision.clone(),
        relation: Some("blocks".into()),
    })
    .ok();
    let text = f.brief(&revision);
    assert_shape(
        "S13",
        &text,
        &[
            "stuck: S13_revision_chain",
            "blocked_reason: worker_failed",
            "kind: revision",
        ],
    );
}

#[test]
fn s14_worker_recovery() {
    let f = fixture();
    let id = f.chore("Recovery exhausted");
    f.stick(&id, "status = 'active'");
    f.db.create_attention_item(
        CreateAttentionItemInput::builder()
            .work_item_id(&id)
            .kind("worker_recovery_exhausted")
            .title("Recovery exhausted")
            .body_markdown("three API-error resumes failed")
            .build(),
    )
    .unwrap();
    let text = f.brief(&id);
    assert_shape(
        "S14",
        &text,
        &[
            "stuck: S14_worker_recovery",
            "kind=worker_recovery_exhausted",
            "three API-error resumes failed",
        ],
    );
}

#[test]
fn s16_review_churn() {
    let f = fixture();
    let id = f.chore("Review churned");
    f.bind_pr(&id, 16);
    f.stick(&id, "status = 'in_review'");
    f.db.file_churn_guard_parked_attention(&id, "pr_review_recovery", 4, &[], "terminal pr_review executions");
    let text = f.brief(&id);
    assert_shape("S16", &text, &["stuck: S16_review_churn", "kind=churn_guard_parked"]);
}

#[test]
fn s22_abandoned_branch() {
    let f = fixture();
    let id = f.chore("Orphan branch");
    f.stick(&id, "status = 'active', autostart = 1");
    f.db.create_attention_item(
        CreateAttentionItemInput::builder()
            .work_item_id(&id)
            .kind(ATTENTION_KIND_ABANDONED_BRANCH_NO_PR)
            .title("Branch has no PR")
            .body_markdown("pushed boss/orphan-branch more than 24h ago")
            .build(),
    )
    .unwrap();
    let text = f.brief(&id);
    assert_shape(
        "S22",
        &text,
        &[
            "stuck: S22_abandoned_branch",
            "pr_url: (none)",
            "(no PR bound)",
            "kind=abandoned_branch_no_pr",
            "pushed boss/orphan-branch more than 24h ago",
        ],
    );
}

#[test]
fn fingerprint_is_stable_for_the_same_inputs() {
    use crate::compute_stuck_fingerprint;
    let a = compute_stuck_fingerprint(
        "S6_ci_failure",
        Some("ci_failure_exhausted"),
        Some("abc"),
        Some("exec_1"),
    );
    let b = compute_stuck_fingerprint(
        "S6_ci_failure",
        Some("ci_failure_exhausted"),
        Some("abc"),
        Some("exec_1"),
    );
    let c = compute_stuck_fingerprint(
        "S6_ci_failure",
        Some("ci_failure_exhausted"),
        Some("def"),
        Some("exec_1"),
    );
    assert_eq!(a, b);
    assert_eq!(a.len(), 16);
    assert_ne!(a, c);
}
