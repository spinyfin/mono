//! Snapshot tests: one representative item per eligible stuck shape.
//!
//! IDs and timestamps are normalized so the assertions pin the diagnosis
//! packet's wording rather than minted tokens.

use boss_engine_storage_values::attention_kinds::{
    ATTENTION_KIND_ABANDONED_BRANCH_NO_PR, NUDGE_BREAKER_ATTENTION_KIND,
};
use boss_protocol::{
    AddDependencyInput, CreateAttentionItemInput, CreateExecutionInput, ExecutionKind, ExecutionStatus,
    FinishExecutionRunInput, WorkItemPatch,
};
use boss_work_db::{
    CiRemediationInsertInput, ConflictResolutionInsertInput, PrPollStateInput,
    REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS, ReviewVerdictInput, WorkDb, WorkerFailureBasis,
    test_support::{create_test_chore_manual, create_test_product_named, open_db},
};
use regex::Regex;

use crate::{gather_progression_brief, render_progression_brief};

/// The stored-state half of the prompt, as production renders it after the
/// episode write; engine probes are covered in `evidence_tests`.
fn compose_progression_brief(db: &WorkDb, id: &str) -> anyhow::Result<String> {
    Ok(render_progression_brief(&gather_progression_brief(db, id)?))
}
use crate::golden;

const REQUIRED_SECTIONS: &[&str] = &[
    "# Progression diagnosis",
    "## Item",
    "## Ledger (last 3 status changes and the row that entered the stuck state)",
    "## Dependencies",
    "## Executions (item and revision chain)",
    "## Declared blocker (2.9)",
    "## PR",
    "## Open attentions",
    "## Operator questions",
    "## Progression history",
    "Current fingerprint:",
    "Prior fingerprint actions: ",
    "Remaining redispatch allowance: 1.",
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

    fn poll(&self, id: &str, head: &str, detail: &str) {
        self.db
            .update_task_pr_poll_state(
                id,
                PrPollStateInput {
                    ci_required_state: "fail",
                    review_required_state: "",
                    ci_required_detail: (detail != "[]").then_some(detail),
                    pr_mergeable_state: "MERGEABLE",
                    pr_head_sha: Some(head),
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
    let ids = Regex::new(r"\b([a-z]+_)[0-9a-f]{8,}(_[0-9a-f]+)?\b").unwrap();
    let fingerprint = Regex::new(r"(Current fingerprint: `)[0-9a-f]{16}(`)").unwrap();
    let text = timestamps.replace_all(text, "<TS>");
    let text = ids.replace_all(&text, "${1}ID");
    fingerprint.replace_all(&text, "${1}<FINGERPRINT>${2}").into_owned()
}

/// The packet up to the static verb catalogue; shape assertions pin its required tail.
fn variable_part(text: &str) -> &str {
    text.split("## Verb catalogue").next().unwrap_or(text)
}

/// Compare the normalized packet to its full expected text, in order.
fn assert_golden(name: &str, text: &str) {
    let actual = variable_part(text);
    let expected = golden::lookup(name);
    assert!(
        actual == expected,
        "{name}: packet differs from golden\n=====GOLDEN-BEGIN {name}\n{actual}=====GOLDEN-END"
    );
}

/// Stored `ci_required_detail` exactly as the merge poller writes it.
fn poller_detail(checks: &[(&str, &str)]) -> String {
    let items: Vec<serde_json::Value> = checks
        .iter()
        .map(|(name, conclusion)| serde_json::json!({"name": name, "conclusion": conclusion}))
        .collect();
    serde_json::to_string(&items).unwrap()
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
    assert_golden("s1", &text);
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
    assert_golden("s2", &text);
}

#[test]
fn s3_deliberate_park_verbatim_blocker() {
    let f = fixture();
    let id = start_then_fail(
        &f,
        "Parked",
        WorkerFailureBasis::DeclaredBlocked,
        "Should the API return 404 or 410 for retired ids? I need the owner to pick.",
    );
    f.sql(&format!(
        "UPDATE tasks SET status = 'active', blocked_reason = NULL WHERE id = '{id}'"
    ));
    assert!(f.db.bounce_deliberate_park_to_backlog(&id, "orphan_sweep", None));
    let text = f.brief(&id);
    assert_shape(
        "S3",
        &text,
        &[
            "stuck: S3_deliberate_park",
            "Producer: declared_blocked",
            "Engine sweep note (not the worker's words): The `orphan_sweep` sweep is holding",
            "Should the API return 404 or 410 for retired ids? I need the owner to pick.",
        ],
    );
    assert_golden("s3", &text);
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
    // The PR moved on after the diagnosis was taken.
    f.poll(&id, "ccc333", &poller_detail(&[]));
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
    assert_golden("s5", &text);
}

#[test]
fn s6_ci_failure_check_history_flags_unrelated() {
    let f = fixture();
    let id = f.chore("Exhausted CI");
    f.bind_pr(&id, 50);
    let other = f.chore("Sibling PR");
    f.bind_pr(&other, 51);
    let detail = poller_detail(&[("bazel-test", "FAILURE"), ("checkleft", "TIMED_OUT")]);
    let detail = detail.as_str();
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
    assert_golden("s6", &text);
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
    if matches!(basis, WorkerFailureBasis::DeclaredBlocked) {
        let payload = serde_json::json!({"outcome":"blocked", "summary":detail}).to_string();
        let applied =
            f.db.submit_worker_proposal(boss_work_db::SubmitWorkerProposalInput {
                execution_id: &execution.id,
                work_item_id: &id,
                kind: boss_protocol::ProposalKind::RunDone,
                payload_json: &payload,
                idempotency_key: "blocked",
            })
            .unwrap()
            .unwrap();
        assert_eq!(applied.proposal.state, boss_protocol::ProposalState::Applied);
    }
    f.db.record_worker_failure_with_basis(&execution.id, detail, basis)
        .unwrap();
    id
}

fn first_execution(f: &Fixture, id: &str) -> String {
    f.db.list_execution_diagnoses_for_chain(id).unwrap().remove(0).id
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
    assert_golden("s7_declared", &text);
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
            .execution_id(first_execution(&f, &id))
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
    assert_golden("s7_nudge", &text);
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
    assert_golden("s9", &text);
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
    assert_golden("s11", &text);
}

#[test]
fn s13_revision_chain_tail() {
    let f = fixture();
    let parent = f.chore("Parent green");
    f.bind_pr(&parent, 90);
    f.stick(&parent, "status = 'in_review', ci_required_state = 'success'");
    f.poll(&parent, "feed90", "[]");
    f.stick(&parent, "ci_required_state = 'success'");
    let attempt =
        f.db.insert_ci_remediation(CiRemediationInsertInput {
            product_id: f.product.clone(),
            work_item_id: parent.clone(),
            pr_url: "https://github.com/spinyfin/mono/pull/90".into(),
            pr_number: 90,
            head_branch: "boss/parent".into(),
            head_sha_at_trigger: "feed90".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: poller_detail(&[("parent-check", "FAILURE")]),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap();
    f.db.set_ci_remediation_log_excerpt(&attempt.id, "parent remediation evidence")
        .unwrap();
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
            "Parent PR (owned by chain root",
            "head_sha: feed90",
            "parent remediation evidence",
        ],
    );
    assert_golden("s13", &text);
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
    assert_golden("s14", &text);
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
    assert_golden("s16", &text);
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
    assert_golden("s22", &text);
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

fn progression_run(f: &Fixture, id: &str) -> String {
    let execution =
        f.db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(id)
                .kind(ExecutionKind::Progression)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    f.sql(&format!(
        "UPDATE work_executions SET status = 'completed', run_done_outcome = 'no_action' WHERE id = '{}'",
        execution.id
    ));
    execution.id
}

fn fingerprint_of(text: &str) -> String {
    Regex::new(r"Current fingerprint: `([0-9a-f]{16})`")
        .unwrap()
        .captures(text)
        .expect("fingerprint line")[1]
        .to_owned()
}

#[test]
fn completed_progression_run_leaves_the_fingerprint_unchanged() {
    let f = fixture();
    let id = start_then_fail(
        &f,
        "Stays stuck",
        WorkerFailureBasis::DeclaredBlocked,
        "needs a decision",
    );
    let before = compose_progression_brief(&f.db, &id).unwrap();
    progression_run(&f, &id);
    let after = compose_progression_brief(&f.db, &id).unwrap();
    assert_eq!(fingerprint_of(&before), fingerprint_of(&after));
    assert!(after.contains("Prior progression runs:\n- "), "{after}");
}

#[test]
fn ledger_keeps_status_rows_after_later_non_status_writes() {
    let f = fixture();
    let id = start_then_fail(
        &f,
        "Noisy audit",
        WorkerFailureBasis::DeclaredBlocked,
        "needs a decision",
    );
    for n in 0..5 {
        f.sql(&format!(
            "INSERT INTO task_status_audit (work_item_id, property, old_value, new_value, actor, basis, changed_at)
             VALUES ('{id}', 'autostart', '{n}', '{}', 'engine', 'noise', '2026-01-01T00:00:0{n}Z')",
            n + 1
        ));
    }
    let text = compose_progression_brief(&f.db, &id).unwrap();
    let ledger = text.split("## Dependencies").next().unwrap();
    assert!(ledger.contains("- status "), "{ledger}");
    assert!(ledger.contains("- blocked_reason "), "entry row missing:\n{ledger}");
    assert!(!ledger.contains("autostart"), "{ledger}");
}

#[test]
fn chain_evidence_on_the_parent_reaches_a_stuck_revision() {
    let f = fixture();
    let parent = f.chore("Parent with question");
    f.bind_pr(&parent, 70);
    let revision = f.chore("Stuck revision");
    f.sql(&format!(
        "UPDATE tasks SET kind = 'revision', parent_task_id = '{parent}',
                status = 'blocked', blocked_reason = 'execution_recovery_failed'
         WHERE id = '{revision}'"
    ));
    f.db.create_attention_item(
        CreateAttentionItemInput::builder()
            .work_item_id(&parent)
            .kind("abandoned_branch_no_pr")
            .title("Parent attention")
            .body_markdown("parent-only evidence")
            .build(),
    )
    .unwrap();
    let text = compose_progression_brief(&f.db, &revision).unwrap();
    assert!(
        text.contains(&format!("on={parent} kind=abandoned_branch_no_pr")),
        "{text}"
    );
    assert!(text.contains("parent-only evidence"), "{text}");
    assert!(text.contains("url: https://github.com/spinyfin/mono/pull/70"), "{text}");
}

#[test]
fn s6_green_sibling_is_evidence_the_failure_is_specific_to_this_pr() {
    let f = fixture();
    let id = f.chore("Red alone");
    f.bind_pr(&id, 62);
    let green = f.chore("Green sibling");
    f.bind_pr(&green, 63);
    let pending = f.chore("Pending sibling");
    f.bind_pr(&pending, 64);
    let other_fail = f.chore("Different failure");
    f.bind_pr(&other_fail, 65);
    f.poll(&id, "deadbeef", &poller_detail(&[("bazel-test", "FAILURE")]));
    f.poll(&other_fail, "feedface", &poller_detail(&[("checkleft", "FAILURE")]));
    f.db.update_task_pr_poll_state(
        &green,
        PrPollStateInput {
            ci_required_state: "success",
            review_required_state: "",
            ci_required_detail: None,
            pr_mergeable_state: "MERGEABLE",
            pr_head_sha: Some("cafebabe"),
            ..Default::default()
        },
    )
    .unwrap();
    f.stick(&id, "status = 'blocked', blocked_reason = 'ci_failure_exhausted'");
    let text = f.brief(&id);
    assert!(
        text.contains("no other PR shows it failing (not failing on 2 other PRs with stored state, unobserved on 1) → more likely specific to this PR"),
        "{text}"
    );
}

#[test]
fn pr_section_carries_the_ai_review_verdict() {
    let f = fixture();
    let id = f.chore("Reviewed");
    f.bind_pr(&id, 66);
    f.poll(&id, "deadbeef", "[]");
    f.stick(&id, "status = 'in_review', review_cycle = 2");
    let text = f.brief(&id);
    assert!(text.contains("### AI review"), "{text}");
    assert!(text.contains("- review_cycle: 2"), "{text}");
    assert!(
        text.contains("latest verdict: (none recorded; unknown, not clean)"),
        "{text}"
    );
}

impl Fixture {
    /// Record a `pr_review` verdict row for `item` the way a completed review
    /// pass leaves it.
    fn review_verdict(&self, item: &str, head: Option<&str>, warranted: bool, revision_task: Option<&str>) {
        let execution = self
            .db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(item)
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        WorkDb::insert_review_verdict_in_tx(
            &self.db.connect().unwrap(),
            &execution.id,
            item,
            &ReviewVerdictInput {
                head_sha: head.map(str::to_owned),
                findings_count: 3,
                revision_warranted: warranted,
                gate_outcome: REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS,
            },
        )
        .unwrap();
        if let Some(revision_task) = revision_task {
            self.db
                .set_review_verdict_revision_task_id(&execution.id, revision_task)
                .unwrap();
        }
    }
}

#[test]
fn ai_review_verdict_reports_head_freshness() {
    for (reviewed, expect) in [
        (Some("cafe01"), "matches_current_head=true"),
        (Some("old000"), "matches_current_head=false"),
        (None, "matches_current_head=unknown"),
    ] {
        let f = fixture();
        let id = f.chore("Reviewed");
        f.bind_pr(&id, 67);
        f.poll(&id, "cafe01", "[]");
        f.stick(&id, "status = 'in_review'");
        f.review_verdict(&id, reviewed, true, Some("rev-task-1"));
        let text = f.brief(&id);
        assert!(
            text.contains(&format!(
                "gate_outcome=completed_with_findings revision_warranted=true findings=3 revision_task=rev-task-1 reviewed_head={} {expect}",
                reviewed.unwrap_or("(unrecorded)")
            )),
            "{text}"
        );
        assert!(!text.contains("latest verdict: (none recorded"), "{text}");
    }
}

#[test]
fn stuck_revision_reads_the_chain_roots_review_verdict() {
    let f = fixture();
    let parent = f.chore("Reviewed parent");
    f.bind_pr(&parent, 68);
    f.poll(&parent, "beef68", "[]");
    f.stick(&parent, "status = 'in_review'");
    f.review_verdict(&parent, Some("beef68"), false, None);
    let revision = f.chore("Stuck revision");
    f.sql(&format!(
        "UPDATE tasks SET kind = 'revision', parent_task_id = '{parent}',
                status = 'blocked', blocked_reason = 'worker_failed'
         WHERE id = '{revision}'"
    ));
    f.review_verdict(&revision, Some("other00"), true, Some("not-this-one"));
    let text = f.brief(&revision);
    assert!(
        text.contains(
            "revision_warranted=false findings=3 revision_task=(none) reviewed_head=beef68 matches_current_head=true"
        ),
        "{text}"
    );
    assert!(!text.contains("not-this-one"), "{text}");
}

#[test]
fn s6_failing_only_here_is_attributed_unknown_not_this_pr() {
    let f = fixture();
    let id = f.chore("Red alone");
    f.bind_pr(&id, 60);
    let other = f.chore("Green sibling");
    f.bind_pr(&other, 61);
    f.poll(&id, "deadbeef", &poller_detail(&[("bazel-test", "FAILURE")]));
    f.stick(&id, "status = 'blocked', blocked_reason = 'ci_failure_exhausted'");
    let text = f.brief(&id);
    assert!(
        text.contains("`bazel-test` failed on this PR; no other PR shows it failing (not failing on 0 other PRs with stored state, unobserved on 1) → main not in stored snapshot → attribution unknown until engine probe."),
        "{text}"
    );
    assert!(!text.contains("likely this PR's change"), "{text}");
}

#[test]
fn redispatch_allowance_is_per_item_and_kind_and_resets_on_completed_work() {
    use boss_work_db::ExecutionDiagnosis;
    let f = fixture();
    f.db.set_metadata("progression.stuck_kind.redispatch", "S7_worker_failed")
        .unwrap();
    let run = |item: &str, kind: &str, status: &str, via: Option<&str>| {
        ExecutionDiagnosis::builder()
            .id("e")
            .work_item_id(item)
            .kind(kind)
            .status(status)
            .maybe_created_via(via)
            .created_at("t")
            .build()
    };
    let remaining = |runs: &[ExecutionDiagnosis], kind: &str| {
        let mut producer = run("target", "progression", "completed", None);
        producer.id = "redispatch".into();
        let mut history = vec![producer];
        history.extend_from_slice(runs);
        crate::gather::redispatch_remaining(&f.db, &history, "target", kind).unwrap()
    };
    f.db.set_metadata("progression.stuck_kind.foreign", "S7_worker_failed")
        .unwrap();
    let mut foreign = run("parent", "progression", "completed", None);
    foreign.id = "foreign".into();
    assert_eq!(
        remaining(
            &[
                foreign,
                run(
                    "target",
                    "revision_implementation",
                    "failed",
                    Some("progression:foreign")
                )
            ],
            "S7_worker_failed"
        ),
        1
    );
    let spent = run(
        "target",
        "chore_implementation",
        "failed",
        Some("progression:redispatch"),
    );
    assert_eq!(remaining(std::slice::from_ref(&spent), "S7_worker_failed"), 0);
    assert_eq!(remaining(std::slice::from_ref(&spent), "S6_ci_failure"), 1);
    for kind in ["chore_implementation", "revision_implementation", "task_implementation"] {
        assert_eq!(
            remaining(
                &[
                    spent.clone(),
                    run("target", kind, "completed", Some("progression:redispatch"))
                ],
                "S7_worker_failed"
            ),
            1
        );
        assert_eq!(
            remaining(
                &[spent.clone(), run("sibling", kind, "completed", None)],
                "S7_worker_failed"
            ),
            0
        );
        assert_eq!(
            remaining(
                &[run("precursor", kind, "failed", Some("progression:redispatch"))],
                "S7_worker_failed"
            ),
            1
        );
    }
    assert_eq!(
        remaining(
            &[spent, run("target", "progression", "completed", None)],
            "S7_worker_failed"
        ),
        0
    );
}

#[test]
fn prior_progression_episodes_are_persisted_once_and_compared() {
    let f = fixture();
    let id = f.chore("Episode history");
    let fingerprint = crate::gather_progression_brief(&f.db, &id).unwrap().fingerprint;
    for value in [&fingerprint, "another-episode"] {
        let execution =
            f.db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&id)
                    .kind(ExecutionKind::Progression)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        f.db.record_progression_episode(&execution.id, value, "S7_worker_failed")
            .unwrap();
        let proposal =
            f.db.submit_worker_proposal(boss_work_db::SubmitWorkerProposalInput {
                execution_id: &execution.id,
                work_item_id: &id,
                kind: boss_protocol::ProposalKind::RunDone,
                payload_json: r#"{"outcome":"no_changes_needed","summary":"inspected episode"}"#,
                idempotency_key: "inspection",
            })
            .unwrap()
            .unwrap();
        assert_eq!(proposal.proposal.state, boss_protocol::ProposalState::Applied);
        f.db.record_progression_episode(&execution.id, "must-not-overwrite", "S6_ci_failure")
            .unwrap();
        f.sql(&format!(
            "UPDATE work_executions SET status='completed' WHERE id='{}'",
            execution.id
        ));
    }
    let text = f.brief(&id);
    assert!(text.contains("matches_current=true"), "{text}");
    assert!(text.contains("matches_current=false"), "{text}");
    assert!(text.contains("fingerprint=another-episode"), "{text}");
    assert!(!text.contains("must-not-overwrite"));
    assert!(
        text.contains("action: run_done:") && text.contains("inspected episode"),
        "{text}"
    );
}

#[test]
fn declared_blocker_ignores_parent_sibling_and_answered_questions() {
    let f = fixture();
    let parent = start_then_fail(&f, "Parent", WorkerFailureBasis::DeclaredBlocked, "parent declaration");
    let sibling = start_then_fail(
        &f,
        "Sibling",
        WorkerFailureBasis::DeclaredBlocked,
        "sibling declaration",
    );
    let target = start_then_fail(&f, "Target", WorkerFailureBasis::DeclaredBlocked, "target declaration");
    for revision in [&sibling, &target] {
        f.stick(revision, &format!("kind='revision', parent_task_id='{parent}'"));
    }
    f.db.create_attention_item(
        CreateAttentionItemInput::builder()
            .execution_id(first_execution(&f, &sibling))
            .kind(NUDGE_BREAKER_ATTENTION_KIND)
            .title("Sibling nudge")
            .body_markdown("sibling nudge words")
            .build(),
    )
    .unwrap();
    let packet = crate::gather_progression_brief(&f.db, &target).unwrap();
    assert_eq!(packet.declared_blocker.as_ref().unwrap().verbatim, "target declaration");
    let make_question = |owner: &str, execution: &str, status, text: &str, time: &str| {
        boss_protocol::OperatorQuestionRecord::builder()
            .question(
                boss_protocol::OperatorQuestionView::builder()
                    .id(text)
                    .text(text)
                    .answer_type(boss_protocol::OperatorAnswerType::YesNo)
                    .explanation("explanation")
                    .asked_at(time)
                    .execution_id(execution)
                    .build(),
            )
            .work_item_id(owner)
            .status(status)
            .build()
    };
    let execution = first_execution(&f, &target);
    let questions = vec![
        make_question(
            &parent,
            "parent-exec",
            boss_protocol::OperatorQuestionStatus::Open,
            "parent question",
            "99",
        ),
        make_question(
            &target,
            &execution,
            boss_protocol::OperatorQuestionStatus::Answered,
            "answered question",
            "99",
        ),
        make_question(
            &target,
            "other-exec",
            boss_protocol::OperatorQuestionStatus::Open,
            "wrong execution",
            "99",
        ),
        make_question(
            &target,
            &execution,
            boss_protocol::OperatorQuestionStatus::Open,
            "newest own question",
            "20",
        ),
        make_question(
            &target,
            &execution,
            boss_protocol::OperatorQuestionStatus::Open,
            "older own question",
            "10",
        ),
    ];
    // Remove proposal evidence to exercise the question fallback independently.
    f.sql(&format!(
        "DELETE FROM worker_proposals WHERE execution_id='{execution}'"
    ));
    let blocker = crate::gather::declared_blocker_from(
        &f.db,
        &packet.item,
        &Some("declared_blocked".into()),
        &packet.executions,
        &packet.attentions,
        &questions,
    )
    .unwrap()
    .unwrap();
    assert_eq!(blocker.verbatim, "newest own question\n\nexplanation");
    assert_ne!(blocker.sweep_note.as_deref(), Some(blocker.verbatim.as_str()));
}

#[test]
fn episode_metadata_for_removed_executions_is_pruned_on_next_write() {
    let f = fixture();
    let id = f.chore("Episode pruning");
    f.db.set_metadata("progression.fingerprint.removed", "old").unwrap();
    f.db.set_metadata("progression.stuck_kind.removed", "S7_worker_failed")
        .unwrap();
    let execution =
        f.db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&id)
                .kind(ExecutionKind::Progression)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    f.db.record_progression_episode(&execution.id, "kept", "S7_worker_failed")
        .unwrap();
    assert_eq!(f.db.get_metadata("progression.fingerprint.removed").unwrap(), None);
    assert_eq!(f.db.get_metadata("progression.stuck_kind.removed").unwrap(), None);
    assert_eq!(
        f.db.get_metadata(&format!("progression.fingerprint.{}", execution.id))
            .unwrap()
            .as_deref(),
        Some("kept")
    );
}
