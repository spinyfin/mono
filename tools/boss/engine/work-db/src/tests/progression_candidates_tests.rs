//! The `progression_candidates` views: one test per clause of design section 2.1,
//! plus the sweep's mint, the grace-waiver producers, and the way a progression
//! run stays out of the queries that reason about an item's own work.
use super::*;
use boss_engine_storage_values::progression::{
    CREATED_VIA_PROGRESSION_SWEEP, METADATA_KEY_STUCK_AFTER_SECS, ProgressionSettings,
};
use boss_protocol::StuckBucket;

const HOUR: i64 = 3600;

fn now() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_secs()
}

fn observed_live_pid(_: Option<i64>) -> bool {
    false
}

struct Fixture {
    db: WorkDb,
    product: String,
}

fn fixture() -> Fixture {
    let db = WorkDb::open(temp_db_path("progression-candidates")).unwrap();
    let product = create_test_product_named(&db, "Progression").id;
    Fixture { db, product }
}

impl Fixture {
    fn chore(&self, name: &str) -> String {
        create_test_chore_manual(&self.db, &self.product, name).id
    }

    /// Put an item into a stuck state and make it `hours` old in the ledger.
    fn stick(&self, id: &str, assignments: &str, hours: i64) {
        self.sql(&format!("UPDATE tasks SET {assignments} WHERE id = '{id}'"));
        self.age(id, hours * HOUR);
    }

    /// Back-date everything the ledger has recorded for the item.
    fn age(&self, id: &str, secs: i64) {
        self.sql(&format!(
            "UPDATE task_status_audit SET changed_at = CAST(changed_at AS INTEGER) - {secs}
             WHERE work_item_id = '{id}'"
        ));
        self.sql(&format!(
            "UPDATE tasks SET progression_grace_waived_at = CAST(progression_grace_waived_at AS INTEGER) - {secs}
             WHERE id = '{id}' AND progression_grace_waived_at IS NOT NULL"
        ));
    }

    fn sql(&self, sql: &str) {
        self.db.connect().unwrap().execute_batch(sql).unwrap();
    }

    /// A stuck chore: `blocked: merge_conflict`, stuck for three hours (past the
    /// two-hour placeholder grace).
    fn stuck_chore(&self, name: &str) -> String {
        let id = self.chore(name);
        self.stick(&id, "status = 'blocked', blocked_reason = 'merge_conflict'", 3);
        id
    }

    fn execution(&self, id: &str, kind: ExecutionKind, status: ExecutionStatus) -> String {
        self.db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(id)
                    .kind(kind)
                    .status(status)
                    .build(),
            )
            .unwrap()
            .id
    }

    fn clauses(&self, id: &str) -> ProgressionClauses {
        self.db.progression_clauses(id).unwrap().expect("open task")
    }

    fn is_candidate(&self, id: &str) -> bool {
        self.db
            .list_progression_candidates(None)
            .unwrap()
            .iter()
            .any(|c| c.work_item_id == id)
    }

    /// The candidate list and the clause view must never disagree.
    fn assert_candidate(&self, id: &str, expected: bool) {
        let name: String = self
            .db
            .connect()
            .unwrap()
            .query_row("SELECT name FROM tasks WHERE id = ?1", [id], |row| row.get(0))
            .unwrap();
        assert_eq!(self.is_candidate(id), expected, "candidate list for `{name}` ({id})");
        let clauses = self.clauses(id);
        assert_eq!(
            clauses.is_candidate(),
            expected,
            "clause view for `{name}` ({id}): {clauses:?}"
        );
    }

    fn revision_of(&self, parent: &str, name: &str) -> String {
        let id = self.chore(name);
        self.sql(&format!(
            "UPDATE tasks SET kind = 'revision', parent_task_id = '{parent}' WHERE id = '{id}'"
        ));
        id
    }

    fn product_id(&self, task_id: &str) -> String {
        self.db
            .connect()
            .unwrap()
            .query_row("SELECT product_id FROM tasks WHERE id = ?1", [task_id], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn insert_ci_attempt(&self, item: &str, head_sha: &str) -> String {
        self.db
            .insert_ci_remediation(CiRemediationInsertInput {
                product_id: self.product_id(item),
                work_item_id: item.to_owned(),
                pr_url: "https://github.com/o/r/pull/7".into(),
                pr_number: 7,
                head_branch: "feature".into(),
                head_sha_at_trigger: head_sha.into(),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: "[]".into(),
                failure_kind: "pr_branch_ci".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .expect("ci attempt")
            .id
    }

    fn insert_conflict_attempt(&self, item: &str, base_sha: &str) -> String {
        self.db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(self.product_id(item))
                    .work_item_id(item)
                    .pr_url("https://github.com/o/r/pull/7")
                    .pr_number(7)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger(base_sha)
                    .head_sha_before(format!("head-{base_sha}"))
                    .build(),
            )
            .unwrap()
            .expect("conflict attempt")
            .id
    }
}

#[test]
fn the_views_cover_open_tasks_and_expose_every_clause() {
    let f = fixture();
    let stuck = f.stuck_chore("Conflicted");
    let done = f.chore("Finished");
    f.sql(&format!("UPDATE tasks SET status = 'done' WHERE id = '{done}'"));
    let clauses = f.clauses(&stuck);
    assert_eq!(clauses.stuck_kind.as_deref(), Some("S5_merge_conflict"));
    assert!(clauses.is_candidate(), "{:?}", clauses.failing_clauses());
    assert!(clauses.failing_clauses().is_empty());
    assert!(!clauses.grace_waived && !clauses.progression_live && !clauses.in_cooldown);
    assert!(
        (now() - clauses.stuck_since - 3 * HOUR).abs() < 5,
        "{}",
        clauses.stuck_since
    );
    // A finished, a deleted and an unknown item are not rows of the view.
    assert!(f.db.progression_clauses(&done).unwrap().is_none());
    assert!(f.db.progression_clauses("task_missing").unwrap().is_none());
    f.sql(&format!("UPDATE tasks SET deleted_at = '1' WHERE id = '{stuck}'"));
    assert!(f.db.progression_clauses(&stuck).unwrap().is_none());
}

// ── Clause 1: not parked, not a human hold ────────────────────────────────

#[test]
fn clause_1_a_parked_item_its_project_and_its_chain_are_never_candidates() {
    let f = fixture();
    let direct = f.stuck_chore("Parked directly");
    f.assert_candidate(&direct, true);
    f.db.park_work_item_as_actor(&direct, true, Some("not now"), "human")
        .unwrap();
    f.assert_candidate(&direct, false);
    assert!(!f.clauses(&direct).eligible_item);
    f.db.park_work_item_as_actor(&direct, false, None, "human").unwrap();
    f.assert_candidate(&direct, true);

    // A parked project parks the items in it.
    let project =
        f.db.create_project(CreateProjectInput {
            product_id: f.product.clone(),
            name: "Parked project".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let member = f.stuck_chore("In the project");
    f.sql(&format!(
        "UPDATE tasks SET project_id = '{}' WHERE id = '{member}'",
        project.id
    ));
    f.assert_candidate(&member, true);
    f.db.park_work_item_as_actor(&project.id, true, None, "human").unwrap();
    f.assert_candidate(&member, false);
    f.db.park_work_item_as_actor(&project.id, false, None, "human").unwrap();
    f.assert_candidate(&member, true);

    // A revision follows its chain root, even without a stamp of its own.
    let root = f.chore("Chain root");
    let revision = f.revision_of(&root, "Revision");
    f.stick(&revision, "status = 'blocked', blocked_reason = 'worker_failed'", 3);
    f.assert_candidate(&revision, true);
    f.sql(&format!(
        "UPDATE tasks SET parked_at = '1', parked_via = 'direct' WHERE id = '{root}'"
    ));
    f.assert_candidate(&revision, false);
}

#[test]
fn clause_1_human_holds_are_never_candidates() {
    let f = fixture();
    for (label, assignments) in [
        ("human driven", "human_driven = 1"),
        ("deferred", "deferred = 1"),
        ("awaiting an answer", "blocked_reason = 'awaiting_operator_answer'"),
        ("deletion sign-off", "blocked_reason = 'deletion_signoff'"),
    ] {
        let id = f.stuck_chore(label);
        f.assert_candidate(&id, true);
        f.sql(&format!("UPDATE tasks SET {assignments} WHERE id = '{id}'"));
        f.assert_candidate(&id, false);
        assert!(
            !f.clauses(&id).eligible_item || f.clauses(&id).stuck_kind.is_none(),
            "{label}"
        );
    }

    // An open operator question, wherever the blocked reason points.
    let (_, asked, execution) = make_waiting_human_chore(&f.db, "asked");
    f.sql(&format!(
        "UPDATE work_executions SET status = 'failed' WHERE id = '{execution}'"
    ));
    f.stick(&asked, "status = 'blocked', blocked_reason = 'merge_conflict'", 3);
    f.assert_candidate(&asked, true);
    // (Raw row: the engine withdraws a question the moment the item's status
    // moves, so a question that outlives a stuck state has to be planted.)
    f.sql(&format!(
        "INSERT INTO operator_questions (id, work_item_id, execution_id, proposal_id, question_json, status, created_at)
         VALUES ('q1', '{asked}', '{execution}', 'p1', '{{}}', 'open', '1')"
    ));
    f.assert_candidate(&asked, false);
    f.sql("UPDATE operator_questions SET status = 'answered'");
    f.assert_candidate(&asked, true);

    // The owner's recorded refusal is a human hold; every other producer of a
    // worker failure is not. Each producer admits both a primary item and a
    // revision (the revision's parent is not green-CI `in_review`, so this is
    // S7, not the S13 dead-chain-head shape).
    for item_kind in ["primary", "revision"] {
        for basis in [
            WorkerFailureBasis::DeliveredWithoutPr,
            WorkerFailureBasis::ForeignPrBinding,
            WorkerFailureBasis::DeclaredBlocked,
            WorkerFailureBasis::NudgeBreaker,
            WorkerFailureBasis::OperatorNo,
        ] {
            let label = format!("{item_kind}-{}", basis.as_str());
            let (_, id, execution) = make_waiting_human_chore(&f.db, &label);
            if item_kind == "revision" {
                let parent = f.chore(&format!("parent-{label}"));
                f.sql(&format!(
                    "UPDATE tasks SET kind = 'revision', parent_task_id = '{parent}' WHERE id = '{id}'"
                ));
            }
            f.db.record_worker_failure_with_basis(&execution, "failed", basis)
                .unwrap();
            f.age(&id, 3 * HOUR);
            let expected = basis != WorkerFailureBasis::OperatorNo;
            f.assert_candidate(&id, expected);
            assert_eq!(
                f.clauses(&id).stuck_kind.as_deref(),
                Some("S7_worker_failed"),
                "{label}"
            );
            assert_eq!(f.clauses(&id).eligible_item, expected, "{label}");
        }
    }
}

fn question() -> boss_protocol::OperatorQuestion {
    boss_protocol::OperatorQuestion {
        text: "Change the timeout?".into(),
        explanation: "The test needs a longer timeout.".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
    }
}

#[test]
fn attention_only_positioning_hold_is_not_a_progression_candidate() {
    let f = fixture();
    for (label, assignments) in [
        (
            "in_review with failing CI",
            "status = 'in_review', ci_required_state = 'fail'",
        ),
        (
            "blocked merge conflict",
            "status = 'blocked', blocked_reason = 'merge_conflict'",
        ),
    ] {
        let id = f.chore(label);
        f.stick(&id, assignments, 3);
        f.assert_candidate(&id, true);
        f.db.upsert_work_item_attention(
            &id,
            "workspace_positioning_refused",
            "Workspace blocked: positioning refused",
            "Workspace blocked: PR is MERGED\n\nheld",
        )
        .unwrap();
        f.assert_candidate(&id, false);
        assert!(
            f.clauses(&id).stuck_kind.is_none(),
            "{label}: held review-phase items are not a progression stuck kind"
        );
        assert!(
            f.db.mint_progression_execution(&id).unwrap().is_none(),
            "{label}: mint of a held item"
        );
        let mint = f.db.mint_progression_executions().unwrap();
        assert!(
            mint.minted.iter().all(|(candidate, _)| candidate.work_item_id != id),
            "{label}: bulk mint of a held item"
        );
        assert!(f.db.clear_positioning_hold(&id).unwrap());
        f.assert_candidate(&id, true);
        assert!(
            f.db.mint_progression_execution(&id).unwrap().is_some(),
            "{label}: mint after the hold clears"
        );
    }
}

#[test]
fn clause_1_worker_failed_is_admitted_on_both_primary_and_revision_items() {
    let f = fixture();
    let primary = f.chore("Primary");
    let revision = f.revision_of(&primary, "Hidden revision");
    f.stick(&primary, "status = 'in_review', ci_required_state = 'success'", 3);
    f.stick(&revision, "status = 'blocked', blocked_reason = 'worker_failed'", 3);
    // A failed hidden revision while parent CI is green holds up its parent.
    // The revision is the stuck item.
    f.assert_candidate(&revision, true);
    assert_eq!(f.clauses(&revision).stuck_kind.as_deref(), Some("S13_revision_chain"));
    // A later revision queued behind the dead one is pending work on the PR
    // family (design 2.1 counts `waiting_dependency` everywhere), so the item is
    // not nominated while that gate row exists.
    let later = f.revision_of(&primary, "Later revision");
    f.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'dependency' WHERE id = '{later}'"
    ));
    let gate = f.execution(
        &later,
        ExecutionKind::RevisionImplementation,
        ExecutionStatus::WaitingDependency,
    );
    f.assert_candidate(&revision, false);
    assert!(!f.clauses(&revision).no_live_work);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'cancelled' WHERE id = '{gate}'"
    ));
    f.assert_candidate(&revision, true);

    let plain = f.chore("Plain primary");
    f.stick(&plain, "status = 'blocked', blocked_reason = 'worker_failed'", 3);
    f.assert_candidate(&plain, true);
    assert_eq!(f.clauses(&plain).stuck_kind.as_deref(), Some("S7_worker_failed"));
}

// ── Clause 2: no live or pending work ─────────────────────────────────────

#[test]
fn clause_2_live_work_on_the_item_its_revisions_or_its_pr_excludes_it() {
    let f = fixture();
    let item = f.stuck_chore("Conflicted PR");
    f.sql(&format!(
        "UPDATE tasks SET pr_url = 'https://github.com/o/r/pull/7' WHERE id = '{item}'"
    ));
    f.assert_candidate(&item, true);

    // A live AI review on the item's PR, bound to another item sharing the PR.
    let sharing = f.chore("Shares the PR");
    f.sql(&format!(
        "UPDATE tasks SET pr_url = 'https://github.com/o/r/pull/7' WHERE id = '{sharing}'"
    ));
    let review = f.execution(&sharing, ExecutionKind::PrReview, ExecutionStatus::Running);
    f.assert_candidate(&item, false);
    assert!(!f.clauses(&item).no_live_work);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'completed' WHERE id = '{review}'"
    ));
    f.assert_candidate(&item, true);

    // `waiting_dependency` on the item itself is pending work.
    let gated = f.execution(
        &item,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::WaitingDependency,
    );
    f.assert_candidate(&item, false);
    assert!(!f.clauses(&item).no_live_work);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'cancelled' WHERE id = '{gated}'"
    ));
    f.assert_candidate(&item, true);

    // `waiting_dependency` on another item that shares the PR (not a revision)
    // is pending work on the family.
    let sibling_gate = f.execution(
        &sharing,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::WaitingDependency,
    );
    f.assert_candidate(&item, false);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'cancelled' WHERE id = '{sibling_gate}'"
    ));
    f.assert_candidate(&item, true);

    // Live or pending work on a revision of the item excludes the item, in each
    // status; a finished execution admits it again.
    let revision = f.revision_of(&item, "Revision of the item");
    f.stick(&revision, "status = 'active'", 0);
    for status in [
        ExecutionStatus::Queued,
        ExecutionStatus::Ready,
        ExecutionStatus::Claimed,
        ExecutionStatus::Running,
        ExecutionStatus::WaitingHuman,
    ] {
        let run = f.execution(&revision, ExecutionKind::RevisionImplementation, status.clone());
        f.assert_candidate(&item, false);
        assert!(!f.clauses(&item).no_live_work, "{status:?}");
        f.sql(&format!(
            "UPDATE work_executions SET status = 'failed' WHERE id = '{run}'"
        ));
        f.assert_candidate(&item, true);
    }

    // `waiting_dependency` on a revision of the primary item counts, whether the
    // revision waits behind an earlier revision or behind an unrelated item.
    let unrelated = f.chore("Unrelated prerequisite");
    f.db.add_dependency(AddDependencyInput {
        dependent: revision.clone(),
        prerequisite: unrelated,
        relation: None,
    })
    .unwrap();
    let gate = f.execution(
        &revision,
        ExecutionKind::RevisionImplementation,
        ExecutionStatus::WaitingDependency,
    );
    f.assert_candidate(&item, false);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'cancelled' WHERE id = '{gate}'"
    ));
    f.assert_candidate(&item, true);

    // Live work on the item itself, and a progression run bound to it.
    for kind in [ExecutionKind::ChoreImplementation, ExecutionKind::Progression] {
        let own = f.execution(&item, kind, ExecutionStatus::Ready);
        f.assert_candidate(&item, false);
        f.sql(&format!(
            "UPDATE work_executions SET status = 'cancelled' WHERE id = '{own}'"
        ));
        f.assert_candidate(&item, true);
    }

    // Unrelated live work elsewhere in the product does not matter.
    let other = f.chore("Unrelated");
    f.execution(&other, ExecutionKind::ChoreImplementation, ExecutionStatus::Running);
    f.assert_candidate(&item, true);
}

#[test]
fn clause_2_an_inflight_ci_or_conflict_attempt_excludes_the_item() {
    let f = fixture();
    let item = f.stuck_chore("Conflicted PR");
    f.sql(&format!(
        "UPDATE tasks SET pr_url = 'https://github.com/o/r/pull/7' WHERE id = '{item}'"
    ));
    f.assert_candidate(&item, true);

    // A pending CI-fix attempt with no revision yet (the window after ci_watch
    // inserts the row and before the revision or execution exists).
    let ci = f.insert_ci_attempt(&item, "head-pending");
    f.assert_candidate(&item, false);
    assert!(!f.clauses(&item).no_inflight_ladder);
    assert!(f.clauses(&item).no_live_work, "no execution is live");
    assert!(
        f.clauses(&item)
            .failing_clauses()
            .iter()
            .any(|(n, reason)| *n == 2 && reason.contains("CI-fix or conflict-resolution")),
        "{:?}",
        f.clauses(&item).failing_clauses()
    );

    // Running, revision non-terminal, still no live execution.
    f.sql(&format!(
        "UPDATE ci_remediations SET status = 'running' WHERE id = '{ci}'"
    ));
    let fixer = f.revision_of(&item, "ci-fix: Conflicted PR");
    f.sql(&format!(
        "UPDATE ci_remediations SET revision_task_id = '{fixer}' WHERE id = '{ci}'"
    ));
    f.stick(&fixer, "status = 'blocked', blocked_reason = 'worker_failed'", 3);
    f.assert_candidate(&item, false);
    assert!(!f.clauses(&item).no_inflight_ladder);

    // The revision going terminal admits the item again.
    f.sql(&format!("UPDATE tasks SET status = 'done' WHERE id = '{fixer}'"));
    f.assert_candidate(&item, true);
    assert!(f.clauses(&item).no_inflight_ladder);

    // A succeeded attempt with no revision also admits.
    f.sql(&format!(
        "UPDATE ci_remediations SET status = 'pending', revision_task_id = NULL WHERE id = '{ci}'"
    ));
    f.assert_candidate(&item, false);
    f.sql(&format!(
        "UPDATE ci_remediations SET status = 'succeeded' WHERE id = '{ci}'"
    ));
    f.assert_candidate(&item, true);

    // The same pair for conflict_resolutions.
    let conflict = f.insert_conflict_attempt(&item, "base-pending");
    f.assert_candidate(&item, false);
    assert!(!f.clauses(&item).no_inflight_ladder);
    f.sql(&format!(
        "UPDATE conflict_resolutions SET status = 'running' WHERE id = '{conflict}'"
    ));
    let resolver = f.revision_of(&item, "merge-conflict: Conflicted PR");
    f.sql(&format!(
        "UPDATE conflict_resolutions SET revision_task_id = '{resolver}' WHERE id = '{conflict}'"
    ));
    f.stick(&resolver, "status = 'todo'", 0);
    f.assert_candidate(&item, false);
    f.sql(&format!("UPDATE tasks SET status = 'archived' WHERE id = '{resolver}'"));
    f.assert_candidate(&item, true);
    f.sql(&format!(
        "UPDATE conflict_resolutions SET status = 'pending', revision_task_id = NULL WHERE id = '{conflict}'"
    ));
    f.assert_candidate(&item, false);
    f.sql(&format!(
        "UPDATE conflict_resolutions SET status = 'succeeded' WHERE id = '{conflict}'"
    ));
    f.assert_candidate(&item, true);
}

// ── Clause 3: not waiting on capacity ─────────────────────────────────────

#[test]
fn clause_3_a_queued_run_waiting_on_capacity_is_not_stuck() {
    let f = fixture();
    let id = f.stuck_chore("Waiting for a slot");
    let queued = f.execution(&id, ExecutionKind::ChoreImplementation, ExecutionStatus::Ready);
    f.db.set_dispatch_wait_reason(&queued, "Waiting for an available worker (8 working)")
        .unwrap();
    let clauses = f.clauses(&id);
    assert!(!clauses.not_capacity_wait, "{clauses:?}");
    assert!(!clauses.no_live_work, "the queued run is also live work");
    f.assert_candidate(&id, false);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'cancelled' WHERE id = '{queued}'"
    ));
    assert!(f.clauses(&id).not_capacity_wait);
    f.assert_candidate(&id, true);
}

// ── Clause 4: dependency ──────────────────────────────────────────────────

#[test]
fn clause_4_a_dependent_is_progressed_through_its_prerequisite() {
    let f = fixture();
    let dependent = f.chore("Dependent");
    let prerequisite = f.chore("Prerequisite");
    f.db.add_dependency(AddDependencyInput {
        dependent: dependent.clone(),
        prerequisite: prerequisite.clone(),
        relation: None,
    })
    .unwrap();
    f.stick(&dependent, "status = 'blocked', blocked_reason = 'dependency'", 3);
    assert_eq!(f.clauses(&dependent).stuck_kind.as_deref(), Some("S4_dependency"));

    // An idle, unparked prerequisite that is in no stuck state does not hold
    // the dependent: the dependent is the candidate.
    f.sql(&format!("UPDATE tasks SET autostart = 1 WHERE id = '{prerequisite}'"));
    f.assert_candidate(&dependent, true);
    assert!(f.clauses(&dependent).dependency_clear);

    // A live one does...
    let run = f.execution(
        &prerequisite,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Running,
    );
    f.assert_candidate(&dependent, false);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'failed' WHERE id = '{run}'"
    ));

    // ...a stuck one (the prerequisite is the candidate, not the dependent)...
    f.stick(
        &prerequisite,
        "status = 'blocked', blocked_reason = 'merge_conflict'",
        3,
    );
    f.assert_candidate(&prerequisite, true);
    f.assert_candidate(&dependent, false);

    // ...and one the owner parked: the owner's decision about the prerequisite
    // is also the decision about the dependent.
    f.db.park_work_item_as_actor(&prerequisite, true, None, "human")
        .unwrap();
    f.assert_candidate(&dependent, false);
    f.db.park_work_item_as_actor(&prerequisite, false, None, "human")
        .unwrap();

    // A prerequisite waiting on a human is the owner's to unblock; the dependent
    // is surfaced instead, as the stuck report does (`engine_stuck`).
    f.sql(&format!(
        "UPDATE tasks SET human_driven = 1 WHERE id = '{prerequisite}'"
    ));
    f.assert_candidate(&dependent, true);
    f.sql(&format!(
        "UPDATE tasks SET human_driven = 0 WHERE id = '{prerequisite}'"
    ));
    f.assert_candidate(&dependent, false);

    // A dangling edge gates forever; that is exactly what progression repairs.
    f.sql(&format!(
        "UPDATE work_item_dependencies SET prerequisite_id = 'task_missing' WHERE dependent_id = '{dependent}'"
    ));
    f.assert_candidate(&dependent, true);
    // And a finished prerequisite no longer carries it.
    f.sql(&format!(
        "UPDATE work_item_dependencies SET prerequisite_id = '{prerequisite}' WHERE dependent_id = '{dependent}'"
    ));
    f.sql(&format!("UPDATE tasks SET status = 'done' WHERE id = '{prerequisite}'"));
    f.assert_candidate(&dependent, true);
}

impl Fixture {
    /// A dependency-blocked chore gated on `prerequisite`, stuck for three hours.
    fn dependent_on(&self, prerequisite: &str) -> String {
        let dependent = self.chore(&format!("Dependent on {prerequisite}"));
        self.sql(&format!(
            "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
             VALUES ('{dependent}', '{prerequisite}', 'blocks', '1')"
        ));
        self.stick(&dependent, "status = 'blocked', blocked_reason = 'dependency'", 3);
        dependent
    }
}

#[test]
fn clause_4_an_open_project_holds_its_dependent_only_while_it_is_live_or_a_member_is_a_candidate() {
    let f = fixture();
    let project =
        f.db.create_project(CreateProjectInput {
            product_id: f.product.clone(),
            name: "Prerequisite project".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let member = f.chore("Idle member");
    f.sql(&format!(
        "UPDATE tasks SET project_id = '{}' WHERE id = '{member}'",
        project.id
    ));
    let dependent = f.dependent_on(&project.id);

    // Only idle, non-stuck members: the project holds nothing and the dependent
    // is the candidate.
    f.assert_candidate(&dependent, true);

    // A running member task makes the project live.
    let run = f.execution(&member, ExecutionKind::ChoreImplementation, ExecutionStatus::Running);
    f.assert_candidate(&dependent, false);
    assert!(!f.clauses(&dependent).dependency_clear);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'failed' WHERE id = '{run}'"
    ));
    f.assert_candidate(&dependent, true);

    // A member that is itself a candidate is progressed first.
    f.stick(&member, "status = 'blocked', blocked_reason = 'merge_conflict'", 3);
    f.assert_candidate(&member, true);
    f.assert_candidate(&dependent, false);
    // Once the member is in its cooldown it is not a candidate and holds nothing.
    finished_progression_run(&f, &member, ExecutionStatus::Completed, 60);
    f.assert_candidate(&member, false);
    f.assert_candidate(&dependent, true);

    // A parked project is the owner's decision.
    f.db.park_work_item_as_actor(&project.id, true, None, "human").unwrap();
    f.assert_candidate(&dependent, false);
}

#[test]
fn clause_4_a_stuck_prerequisite_that_cannot_be_nominated_does_not_hold_the_dependent() {
    let f = fixture();
    let prerequisite = f.stuck_chore("Stuck prerequisite");
    let dependent = f.dependent_on(&prerequisite);
    f.assert_candidate(&prerequisite, true);
    f.assert_candidate(&dependent, false);

    // Inside its cooldown.
    let run = finished_progression_run(&f, &prerequisite, ExecutionStatus::Completed, 60);
    f.assert_candidate(&prerequisite, false);
    f.assert_candidate(&dependent, true);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'cancelled' WHERE id = '{run}'"
    ));
    f.assert_candidate(&dependent, false);

    // Its create budget exhausted.
    for i in 0..3 {
        let child = f.revision_of(&prerequisite, &format!("progression child {i}"));
        f.sql(&format!(
            "UPDATE tasks SET created_via = 'progression:exec_{i}', created_at = '{}' WHERE id = '{child}'",
            now()
        ));
    }
    f.assert_candidate(&prerequisite, false);
    f.assert_candidate(&dependent, true);
    f.sql("UPDATE tasks SET created_at = '1' WHERE created_via LIKE 'progression:%'");
    f.assert_candidate(&prerequisite, true);
    f.assert_candidate(&dependent, false);

    // At the infrastructure-failure cap.
    finished_progression_run(&f, &prerequisite, ExecutionStatus::Failed, 30 * HOUR);
    finished_progression_run(&f, &prerequisite, ExecutionStatus::Orphaned, 20 * HOUR);
    assert!(f.clauses(&prerequisite).infrastructure_failing);
    f.assert_candidate(&prerequisite, false);
    f.assert_candidate(&dependent, true);

    // Not yet stuck long enough (grace) holds nothing either.
    let fresh = f.chore("Fresh prerequisite");
    f.stick(&fresh, "status = 'blocked', blocked_reason = 'merge_conflict'", 0);
    let waiting = f.dependent_on(&fresh);
    f.assert_candidate(&fresh, false);
    f.assert_candidate(&waiting, true);
}

#[test]
fn the_stuck_report_reads_the_same_dependency_rule_as_the_view() {
    let f = fixture();
    // An idle, unparked prerequisite: the dependent is the candidate, so once
    // the sweep mints it a run the report shows that run.
    let idle = f.chore("Idle prerequisite");
    let dependent = f.dependent_on(&idle);
    f.assert_candidate(&dependent, true);
    assert_eq!(bucket(&f.db, &dependent), Some(StuckBucket::EngineStuck));
    f.db.mint_progression_executions().unwrap();
    assert_eq!(bucket(&f.db, &dependent), Some(StuckBucket::ProgressionInFlight));

    // The same for an open project whose members are all idle.
    let project =
        f.db.create_project(CreateProjectInput {
            product_id: f.product.clone(),
            name: "Idle project".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let gated = f.dependent_on(&project.id);
    f.assert_candidate(&gated, true);
    f.db.mint_progression_executions().unwrap();
    assert_eq!(bucket(&f.db, &gated), Some(StuckBucket::ProgressionInFlight));

    // A live prerequisite is still waiting upstream.
    let busy = f.chore("Busy prerequisite");
    f.execution(&busy, ExecutionKind::ChoreImplementation, ExecutionStatus::Running);
    let waiting = f.dependent_on(&busy);
    f.assert_candidate(&waiting, false);
    assert_eq!(bucket(&f.db, &waiting), Some(StuckBucket::WaitingOnDependency));
}

#[test]
fn clause_4_a_prerequisite_held_only_by_its_own_clause_4_does_not_hold_its_dependent() {
    let f = fixture();
    let upstream = f.stuck_chore("Upstream");
    let middle = f.dependent_on(&upstream);
    let downstream = f.dependent_on(&middle);
    f.assert_candidate(&upstream, true);
    f.assert_candidate(&middle, false);
    // Middle is excluded only by clause 4. Treating `candidate_ex_dependency` as
    // candidacy would hold downstream behind middle; real candidacy does not.
    assert!(f.clauses(&middle).eligible_item);
    assert!(f.clauses(&middle).stuck_long_enough);
    assert!(f.clauses(&middle).cooldown_and_budget_clear);
    f.assert_candidate(&downstream, true);
    assert!(f.clauses(&downstream).dependency_clear);

    // A project whose only stuck member is that middle item also does not hold
    // a dependent: the member is not a candidate.
    let project =
        f.db.create_project(CreateProjectInput {
            product_id: f.product.clone(),
            name: "Middle's project".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    f.sql(&format!(
        "UPDATE tasks SET project_id = '{}' WHERE id = '{middle}'",
        project.id
    ));
    let gated_on_project = f.dependent_on(&project.id);
    f.assert_candidate(&middle, false);
    f.assert_candidate(&gated_on_project, true);
}

#[test]
fn clause_4_a_task_that_depends_on_its_own_project_is_still_a_candidate() {
    let f = fixture();
    let project =
        f.db.create_project(CreateProjectInput {
            product_id: f.product.clone(),
            name: "Self project".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let sibling = f.chore("Idle sibling");
    f.sql(&format!(
        "UPDATE tasks SET project_id = '{}', autostart = 1 WHERE id = '{sibling}'",
        project.id
    ));
    let member = f.dependent_on(&project.id);
    f.sql(&format!(
        "UPDATE tasks SET project_id = '{}' WHERE id = '{member}'",
        project.id
    ));
    f.assert_candidate(&member, true);
}

#[test]
fn clause_4_a_same_family_idle_prerequisite_does_not_hide_the_dependent_run() {
    let f = fixture();
    let prerequisite = f.chore("Idle same-family prerequisite");
    f.sql(&format!(
        "UPDATE tasks SET autostart = 1, pr_url = 'https://github.com/o/r/pull/9' WHERE id = '{prerequisite}'"
    ));
    let dependent = f.dependent_on(&prerequisite);
    f.sql(&format!(
        "UPDATE tasks SET pr_url = 'https://github.com/o/r/pull/9' WHERE id = '{dependent}'"
    ));
    f.assert_candidate(&dependent, true);
    f.db.mint_progression_executions().unwrap();
    assert_eq!(bucket(&f.db, &dependent), Some(StuckBucket::ProgressionInFlight));
}

// ── Clause 5: stuck long enough, and the grace waiver ─────────────────────

#[test]
fn clause_5_an_item_must_be_stuck_for_the_grace_unless_it_is_waived() {
    let f = fixture();
    let id = f.chore("Just conflicted");
    f.stick(&id, "status = 'blocked', blocked_reason = 'merge_conflict'", 0);
    let clauses = f.clauses(&id);
    assert!(
        !clauses.stuck_long_enough,
        "a fresh conflict is inside the two-hour grace"
    );
    assert_eq!(
        clauses.failing_clauses().iter().map(|c| c.0).collect::<Vec<_>>(),
        vec![5]
    );
    f.age(&id, HOUR);
    f.assert_candidate(&id, false);
    f.age(&id, HOUR + 60);
    f.assert_candidate(&id, true);

    // The grace is the published limit, not a constant in the view.
    f.db.configure_progression_calibration(&ProgressionSettings::builder().stuck_after_secs(10 * HOUR).build())
        .unwrap();
    f.assert_candidate(&id, false);
    f.db.configure_progression_calibration(&ProgressionSettings::builder().stuck_after_secs(HOUR).build())
        .unwrap();
    f.assert_candidate(&id, true);
    f.sql(&format!(
        "DELETE FROM metadata WHERE key = '{METADATA_KEY_STUCK_AFTER_SECS}'"
    ));
    f.assert_candidate(&id, true);
    f.age(&id, -(5 * HOUR));
    f.assert_candidate(&id, false);

    // An item in no stuck state is never nominated, however old it is.
    let healthy = f.chore("Healthy");
    f.sql(&format!("UPDATE tasks SET status = 'active' WHERE id = '{healthy}'"));
    f.age(&healthy, 100 * HOUR);
    f.assert_candidate(&healthy, false);
    assert_eq!(f.clauses(&healthy).stuck_kind, None);
}

#[test]
fn clause_5_declared_blockers_and_a_fresh_unpark_are_admitted_before_the_grace() {
    let f = fixture();

    // The three declared-blocker producers waive the grace in the failure
    // transaction itself.
    for basis in [WorkerFailureBasis::DeclaredBlocked, WorkerFailureBasis::NudgeBreaker] {
        let (_, id, execution) = make_waiting_human_chore(&f.db, basis.as_str());
        f.db.record_worker_failure_with_basis(&execution, "blocked, no question", basis)
            .unwrap();
        let clauses = f.clauses(&id);
        assert!(clauses.grace_waived, "{}", basis.as_str());
        assert!(clauses.stuck_long_enough, "{}", basis.as_str());
        f.assert_candidate(&id, true);
    }
    let parked = f.chore("Deliberately parked");
    f.sql(&format!("UPDATE tasks SET status = 'active' WHERE id = '{parked}'"));
    assert!(f.db.bounce_deliberate_park_to_backlog(&parked, "orphan_sweep", None));
    assert_eq!(f.clauses(&parked).stuck_kind.as_deref(), Some("S3_deliberate_park"));
    assert!(f.clauses(&parked).grace_waived);
    f.assert_candidate(&parked, true);

    // Real failures that are not an unconverted question age like any other, and
    // a worker that asked a typed question is waiting on a human.
    let (_, delivered, execution) = make_waiting_human_chore(&f.db, "delivered");
    f.db.record_worker_failure_with_basis(&execution, "no PR", WorkerFailureBasis::DeliveredWithoutPr)
        .unwrap();
    assert!(!f.clauses(&delivered).grace_waived);
    f.assert_candidate(&delivered, false);
    f.age(&delivered, 3 * HOUR);
    f.assert_candidate(&delivered, true);
    let (_, asked, execution) = make_waiting_human_chore(&f.db, "asked");
    f.db.record_worker_awaiting_operator_answer(&execution, "decide", &question())
        .unwrap();
    assert!(!f.clauses(&asked).grace_waived);
    f.assert_candidate(&asked, false);

    // A churn-guard bounce is a dispatch-health signal, not a declared blocker.
    let churned = f.chore("Churned");
    f.sql(&format!("UPDATE tasks SET status = 'active' WHERE id = '{churned}'"));
    assert!(
        f.db.bounce_dispatch_failed_to_backlog(&churned, "churn_guard", "x")
            .unwrap()
    );
    assert!(!f.clauses(&churned).grace_waived);
    f.assert_candidate(&churned, false);

    // Un-parking an item that is still stuck waives the grace (step 7), so it
    // is diagnosed straight away.
    // The failure stamp is written after the park, so the un-park reconcile
    // keeps it as current evidence (an older stamp is cleared as stale).
    let unparked = f.chore("Parked then released");
    f.db.park_work_item_as_actor(&unparked, true, None, "human").unwrap();
    f.sql(&format!(
        "UPDATE tasks SET status = 'active', parked_at = CAST(CAST(parked_at AS INTEGER) - 10 AS TEXT)
         WHERE id = '{unparked}'"
    ));
    assert!(
        f.db.bounce_dispatch_failed_to_backlog(&unparked, "churn_guard", "x")
            .unwrap()
    );
    f.assert_candidate(&unparked, false);
    f.db.park_work_item_as_actor(&unparked, false, None, "human").unwrap();
    // The un-park transaction itself does not stamp: the background job stamps
    // after it has refreshed the item (step 7), so the sweep cannot nominate a
    // half-reconciled item.
    assert!(!f.clauses(&unparked).grace_waived);
    assert!(f.db.stamp_progression_grace_waived(&unparked).unwrap());
    assert!(f.clauses(&unparked).grace_waived);
    f.assert_candidate(&unparked, true);
}

#[test]
fn clause_5_a_stale_waiver_does_not_survive_into_a_new_stuck_state() {
    let f = fixture();
    let (_, id, execution) = make_waiting_human_chore(&f.db, "stale");
    f.db.record_worker_failure_with_basis(&execution, "blocked", WorkerFailureBasis::DeclaredBlocked)
        .unwrap();
    f.assert_candidate(&id, true);
    // The blocker is resolved, and the item later conflicts for an unrelated
    // reason: that is a new episode with its own grace, not the old waiver.
    f.age(&id, 5 * HOUR);
    f.sql(&format!(
        "UPDATE tasks SET blocked_reason = 'merge_conflict' WHERE id = '{id}'"
    ));
    let clauses = f.clauses(&id);
    assert!(!clauses.grace_waived, "{clauses:?}");
    f.assert_candidate(&id, false);
}

// ── Clause 6: cooldown and failure budget ─────────────────────────────────

fn finished_progression_run(f: &Fixture, id: &str, status: ExecutionStatus, finished_ago: i64) -> String {
    let run = f.execution(id, ExecutionKind::Progression, status);
    f.sql(&format!(
        "UPDATE work_executions SET created_at = {t}, finished_at = {t} WHERE id = '{run}'",
        t = now() - finished_ago
    ));
    run
}

#[test]
fn clause_6_a_recent_run_puts_the_item_in_cooldown() {
    let f = fixture();
    let id = f.stuck_chore("Progressed once");
    finished_progression_run(&f, &id, ExecutionStatus::Completed, HOUR);
    let clauses = f.clauses(&id);
    assert!(clauses.in_cooldown && !clauses.cooldown_and_budget_clear);
    f.assert_candidate(&id, false);
    // After the six-hour placeholder cooldown it is nominated again.
    f.sql("UPDATE work_executions SET finished_at = finished_at - 6 * 3600, created_at = created_at - 6 * 3600");
    assert!(!f.clauses(&id).in_cooldown);
    f.assert_candidate(&id, true);

    // A run that judged the item not stuck earns four times the cooldown.
    let quiet = f.stuck_chore("No action");
    let run = finished_progression_run(&f, &quiet, ExecutionStatus::Completed, 7 * HOUR);
    f.assert_candidate(&quiet, true);
    f.sql(&format!(
        "UPDATE work_executions SET run_done_outcome = 'no_changes_needed' WHERE id = '{run}'"
    ));
    f.assert_candidate(&quiet, false);
    f.sql(&format!(
        "UPDATE work_executions SET finished_at = finished_at - 18 * 3600, created_at = created_at - 18 * 3600
         WHERE id = '{run}'"
    ));
    f.assert_candidate(&quiet, true);

    // A run cancelled before it started did no work and starts no cooldown.
    let cancelled = f.stuck_chore("Cancelled run");
    finished_progression_run(&f, &cancelled, ExecutionStatus::Cancelled, 60);
    f.assert_candidate(&cancelled, true);

    // The limit is the published one.
    let id = f.stuck_chore("Tuned");
    finished_progression_run(&f, &id, ExecutionStatus::Completed, 2 * HOUR);
    f.assert_candidate(&id, false);
    f.db.configure_progression_calibration(&ProgressionSettings::builder().cooldown_secs(HOUR).build())
        .unwrap();
    f.assert_candidate(&id, true);
}

#[test]
fn clause_6_two_infrastructure_failures_in_a_row_stop_the_nominations() {
    let f = fixture();
    let id = f.stuck_chore("Progression keeps dying");
    // Long enough ago that neither run is inside the cooldown.
    finished_progression_run(&f, &id, ExecutionStatus::Failed, 30 * HOUR);
    f.assert_candidate(&id, true);
    assert!(!f.clauses(&id).infrastructure_failing, "one failure is retried");
    finished_progression_run(&f, &id, ExecutionStatus::Orphaned, 20 * HOUR);
    let clauses = f.clauses(&id);
    assert!(
        clauses.infrastructure_failing && !clauses.cooldown_and_budget_clear,
        "{clauses:?}"
    );
    f.assert_candidate(&id, false);
    // A run that did report resets the count.
    finished_progression_run(&f, &id, ExecutionStatus::Completed, 10 * HOUR);
    assert!(!f.clauses(&id).infrastructure_failing);
    f.assert_candidate(&id, true);
}

#[test]
fn clause_6_an_exhausted_create_budget_stops_the_nominations() {
    let f = fixture();
    let id = f.stuck_chore("Already created three");
    for i in 0..3 {
        let child = f.revision_of(&id, &format!("progression child {i}"));
        f.sql(&format!(
            "UPDATE tasks SET created_via = 'progression:exec_{i}', created_at = '{}' WHERE id = '{child}'",
            now()
        ));
    }
    let clauses = f.clauses(&id);
    assert!(!clauses.cooldown_and_budget_clear, "{clauses:?}");
    f.assert_candidate(&id, false);

    // Raising the published budget admits it again; the placeholder of 3 is
    // what the view reads when the key is absent.
    f.db.configure_progression_calibration(&ProgressionSettings::builder().max_creates_per_item_window(10).build())
        .unwrap();
    f.assert_candidate(&id, true);
    f.sql(&format!(
        "DELETE FROM metadata WHERE key = '{}'",
        boss_engine_storage_values::progression::METADATA_KEY_MAX_CREATES_PER_ITEM_WINDOW
    ));
    f.assert_candidate(&id, false);

    // Age the creates out of the rolling window.
    f.sql(&format!(
        "UPDATE tasks SET created_at = '{}' WHERE created_via LIKE 'progression:%'",
        now() - 8 * 24 * HOUR
    ));
    f.assert_candidate(&id, true);
}

// ── The sweep's mint ──────────────────────────────────────────────────────

#[test]
fn candidates_are_listed_oldest_stuck_first() {
    let f = fixture();
    let newer = f.chore("Newer");
    let oldest = f.chore("Oldest");
    let middle = f.chore("Middle");
    f.stick(&newer, "status = 'blocked', blocked_reason = 'merge_conflict'", 3);
    f.stick(&oldest, "status = 'blocked', blocked_reason = 'merge_conflict'", 30);
    f.stick(&middle, "status = 'blocked', blocked_reason = 'merge_conflict'", 10);
    let order: Vec<_> =
        f.db.list_progression_candidates(None)
            .unwrap()
            .into_iter()
            .map(|c| c.work_item_id)
            .collect();
    assert_eq!(order, vec![oldest.clone(), middle, newer]);
    assert_eq!(
        f.db.list_progression_candidates(Some(1)).unwrap()[0].work_item_id,
        oldest
    );
}

#[test]
fn minting_queues_one_ready_run_bound_to_the_item_and_consumes_the_waiver() {
    let f = fixture();
    let (_, id, execution) = make_waiting_human_chore(&f.db, "mint");
    f.db.record_worker_failure_with_basis(&execution, "blocked", WorkerFailureBasis::DeclaredBlocked)
        .unwrap();
    assert!(f.clauses(&id).grace_waived);

    let run = f.db.mint_progression_execution(&id).unwrap().expect("candidate");
    assert_eq!(run.kind, ExecutionKind::Progression);
    assert_eq!(run.status, ExecutionStatus::Ready);
    assert_eq!(run.work_item_id, id, "bound to the stuck item itself");
    let (created_via, stamp): (Option<String>, Option<String>) =
        f.db.connect()
            .unwrap()
            .query_row(
                "SELECT (SELECT created_via FROM work_executions WHERE id = ?1),
                    (SELECT progression_grace_waived_at FROM tasks WHERE id = ?2)",
                params![run.id, id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    assert_eq!(created_via.as_deref(), Some(CREATED_VIA_PROGRESSION_SWEEP));
    assert_eq!(stamp, None, "the waiver is consumed by the mint");

    // The run's own liveness excludes the item: no second run, no second writer.
    assert!(f.clauses(&id).progression_live);
    assert!(f.db.mint_progression_execution(&id).unwrap().is_none());
    assert!(!f.is_candidate(&id));
    // And an item that was never a candidate is not minted.
    let healthy = f.chore("Healthy");
    assert!(f.db.mint_progression_execution(&healthy).unwrap().is_none());
    assert_eq!(f.db.list_executions(Some(&healthy)).unwrap().len(), 0);
}

// ── A progression run is not an attempt at the item's work ────────────────

#[test]
fn a_finished_progression_run_does_not_stop_the_item_getting_its_next_implementation_run() {
    let f = fixture();
    let id = f.stuck_chore("Unblocked by progression");
    finished_progression_run(&f, &id, ExecutionStatus::Completed, 60);
    // The item is later moved back to `todo` with autostart: the reconcile pass
    // must mint it a fresh implementation execution. Were the finished
    // progression run the "latest execution", its different kind would block that.
    f.sql(&format!(
        "UPDATE tasks SET status = 'todo', blocked_reason = NULL, autostart = 1 WHERE id = '{id}'"
    ));
    let result = f.db.reconcile_product_executions(&f.product).unwrap();
    let minted: Vec<_> = result
        .created
        .iter()
        .filter(|execution| execution.work_item_id == id)
        .collect();
    assert_eq!(minted.len(), 1, "{result:?}");
    assert_eq!(minted[0].kind, ExecutionKind::ChoreImplementation);
}

#[test]
fn progression_runs_neither_strike_nor_reset_the_churn_guard() {
    let f = fixture();
    let id = f.chore("Churn accounting");
    for status in [ExecutionStatus::Failed, ExecutionStatus::Failed] {
        let run = f.execution(&id, ExecutionKind::ChoreImplementation, status);
        f.sql(&format!(
            "UPDATE work_executions SET created_at = {} WHERE id = '{run}'",
            now() - 100
        ));
    }
    let since = now() - 3600;
    assert_eq!(f.db.count_recent_terminal_executions(&id, since, None).unwrap(), 2);
    // Progression runs that died do not count as strikes...
    for _ in 0..3 {
        finished_progression_run(&f, &id, ExecutionStatus::Failed, 10);
    }
    assert_eq!(f.db.count_recent_terminal_executions(&id, since, None).unwrap(), 2);
    assert_eq!(
        f.db.list_recent_terminal_execution_ids(&id, since, None).unwrap().len(),
        2
    );
    assert_eq!(
        f.db.list_consecutive_unproductive_terminal_execution_ids(&id)
            .unwrap()
            .len(),
        2
    );
    // ...and one that completed does not reset the streak of failed work runs.
    finished_progression_run(&f, &id, ExecutionStatus::Completed, 5);
    assert_eq!(
        f.db.list_consecutive_unproductive_terminal_execution_ids(&id)
            .unwrap()
            .len(),
        2
    );
    // Asking for the progression kind by name still works.
    assert_eq!(
        f.db.count_recent_terminal_executions(&id, since, Some(ExecutionKind::Progression))
            .unwrap(),
        3
    );
}

#[test]
fn ready_progression_runs_are_ordered_by_age_not_by_the_class_of_their_item() {
    let f = fixture();
    let root = f.chore("Root");
    let plain = f.chore("Plain");
    // A merge-conflict revision would outrank a plain chore for an implementation
    // slot; for a progression slot only how long the item has been stuck counts.
    let revision = f.revision_of(&root, "merge-conflict: Root");
    f.sql(&format!(
        "UPDATE tasks SET created_via = 'merge-conflict:abc' WHERE id = '{revision}'"
    ));
    let first = f.execution(&plain, ExecutionKind::Progression, ExecutionStatus::Ready);
    f.sql(&format!(
        "UPDATE work_executions SET created_at = '100' WHERE id = '{first}'"
    ));
    let second = f.execution(&revision, ExecutionKind::Progression, ExecutionStatus::Ready);
    f.sql(&format!(
        "UPDATE work_executions SET created_at = '200' WHERE id = '{second}'"
    ));
    // A newer critical item must not jump ahead of an older ordinary one: the
    // critical ordering term does not apply to progression rows.
    let critical = f.chore("Critical");
    f.sql(&format!(
        "UPDATE tasks SET priority = 'critical' WHERE id = '{critical}'"
    ));
    let third = f.execution(&critical, ExecutionKind::Progression, ExecutionStatus::Ready);
    f.sql(&format!(
        "UPDATE work_executions SET created_at = '300' WHERE id = '{third}'"
    ));
    let order: Vec<_> =
        f.db.list_ready_executions()
            .unwrap()
            .into_iter()
            .map(|execution| execution.id)
            .collect();
    assert_eq!(order, vec![first, second, third]);
}

// ── The report ────────────────────────────────────────────────────────────

fn bucket(db: &WorkDb, id: &str) -> Option<StuckBucket> {
    db.stuck_report(0, now(), &observed_live_pid)
        .unwrap()
        .rows
        .into_iter()
        .find(|row| row.work_item_id == id)
        .map(|row| row.bucket)
}

#[test]
fn the_stuck_report_names_runs_in_flight_cooldowns_and_failures() {
    let f = fixture();
    let id = f.stuck_chore("Reported");
    assert_eq!(bucket(&f.db, &id), Some(StuckBucket::EngineStuck));

    // A queued run is being handled, and a running one too.
    let run = f.execution(&id, ExecutionKind::Progression, ExecutionStatus::Ready);
    assert_eq!(bucket(&f.db, &id), Some(StuckBucket::ProgressionInFlight));
    f.sql(&format!(
        "UPDATE work_executions SET status = 'running' WHERE id = '{run}'"
    ));
    assert_eq!(bucket(&f.db, &id), Some(StuckBucket::ProgressionInFlight));
    // A run held by a pause is waiting on capacity, which is what the report says.
    f.sql(&format!(
        "UPDATE work_executions SET status = 'ready' WHERE id = '{run}'"
    ));
    f.db.set_dispatch_wait_reason(&run, "dispatch_paused").unwrap();
    assert_eq!(bucket(&f.db, &id), Some(StuckBucket::WaitingOnCapacity));
    f.sql(&format!(
        "UPDATE work_executions SET status = 'completed', finished_at = {} WHERE id = '{run}'",
        now()
    ));
    // After the run, the cooldown keeps it "in flight" rather than re-defective.
    assert_eq!(bucket(&f.db, &id), Some(StuckBucket::ProgressionInFlight));
    f.sql(&format!(
        "UPDATE work_executions SET created_at = {t}, finished_at = {t} WHERE id = '{run}'",
        t = now() - 7 * HOUR
    ));
    assert_eq!(bucket(&f.db, &id), Some(StuckBucket::EngineStuck));

    // Two infrastructure failures: the engine admits defeat.
    let failing = f.stuck_chore("Progression keeps dying");
    finished_progression_run(&f, &failing, ExecutionStatus::Failed, 30 * HOUR);
    assert_eq!(
        bucket(&f.db, &failing),
        Some(StuckBucket::EngineStuck),
        "one failure is retried"
    );
    finished_progression_run(&f, &failing, ExecutionStatus::Failed, 20 * HOUR);
    assert_eq!(bucket(&f.db, &failing), Some(StuckBucket::ProgressionFailing));
}

// ── Agreement with the stuck report, and upgrade ──────────────────────────

#[test]
fn the_views_classify_a_stuck_state_the_way_the_stuck_report_does() {
    let f = fixture();
    // (assignments, the stuck report's code, if asserted, the views' code)
    let cases: &[(&str, Option<&str>, Option<&str>)] = &[
        (
            "status = 'blocked', blocked_reason = 'merge_conflict'",
            Some("S5_merge_conflict"),
            Some("S5_merge_conflict"),
        ),
        (
            "status = 'blocked', blocked_reason = 'ci_failure_exhausted'",
            Some("S6_ci_failure"),
            Some("S6_ci_failure"),
        ),
        (
            "status = 'blocked', blocked_reason = 'worker_failed'",
            Some("S7_worker_failed"),
            Some("S7_worker_failed"),
        ),
        (
            "status = 'blocked', blocked_reason = 'execution_recovery_failed'",
            Some("S9_execution_recovery"),
            Some("S9_execution_recovery"),
        ),
        (
            "status = 'blocked', blocked_reason = 'spawning_attempt_retired_unsuccessfully'",
            Some("S10_retired_attempt"),
            Some("S10_retired_attempt"),
        ),
        (
            "status = 'blocked', blocked_reason = 'pr_binding_unresolved'",
            Some("S11_pr_binding"),
            Some("S11_pr_binding"),
        ),
        (
            "status = 'blocked', blocked_reason = 'awaiting_operator_answer'",
            Some("S8_operator_question"),
            None,
        ),
        (
            "status = 'blocked', blocked_reason = 'deletion_signoff'",
            Some("S12_manual_block"),
            None,
        ),
        (
            "status = 'todo', dispatch_failed_reason = 'churn_guard'",
            Some("S2_churn_guard"),
            Some("S2_churn_guard"),
        ),
        (
            "status = 'todo', dispatch_failed_reason = 'pane_spawn_failed'",
            Some("S1_dispatch_failure"),
            Some("S1_dispatch_failure"),
        ),
        (
            "status = 'in_review', ci_required_state = 'fail'",
            Some("S6_ci_failure"),
            Some("S6_ci_failure"),
        ),
        ("status = 'in_review', ci_required_state = 'success'", None, None),
        ("status = 'todo'", Some("S21_manual_start"), None),
    ];
    for (assignments, report_code, view_code) in cases {
        let id = f.chore(assignments);
        f.sql(&format!("UPDATE tasks SET {assignments} WHERE id = '{id}'"));
        let reported =
            f.db.stuck_report(0, now(), &observed_live_pid)
                .unwrap()
                .rows
                .into_iter()
                .find(|row| row.work_item_id == id)
                .unwrap()
                .classification;
        if let Some(report_code) = report_code {
            assert_eq!(reported.split(':').next().unwrap(), *report_code, "{assignments}");
        }
        assert_eq!(f.clauses(&id).stuck_kind.as_deref(), *view_code, "{assignments}");
    }

    // Attention-based states.
    for (kind, report_code, view_code) in [
        (
            "worker_recovery_exhausted",
            "S14_worker_recovery",
            Some("S14_worker_recovery"),
        ),
        (
            "abandoned_branch_no_pr",
            "S22_abandoned_branch",
            Some("S22_abandoned_branch"),
        ),
        ("trunk_queue_evicted", "S17_merge_queue", None),
    ] {
        let id = f.chore(kind);
        f.sql(&format!(
            "UPDATE tasks SET status = 'active', autostart = 1 WHERE id = '{id}'"
        ));
        f.db.upsert_work_item_attention(&id, kind, "title", "body").unwrap();
        let reported =
            f.db.stuck_report(0, now(), &observed_live_pid)
                .unwrap()
                .rows
                .into_iter()
                .find(|row| row.work_item_id == id)
                .unwrap()
                .classification;
        assert_eq!(reported.split(':').next().unwrap(), report_code, "{kind}");
        assert_eq!(f.clauses(&id).stuck_kind.as_deref(), view_code, "{kind}");
    }
}

#[test]
fn the_migrations_are_idempotent_and_repair_a_database_that_lacks_them() {
    let (_dir, path) = disk_db_path("progression-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_named(&db, "Upgrade");
    let id = create_test_chore_manual(&db, &product.id, "Stuck").id;
    db.connect()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict' WHERE id = '{id}';
             UPDATE task_status_audit SET changed_at = changed_at - 36000 WHERE work_item_id = '{id}';
             DROP VIEW progression_candidates;
             DROP VIEW progression_candidate_clauses;
             DROP VIEW progression_real_candidates;
             DROP VIEW progression_candidate_base;
             DROP VIEW progression_run_state;
             DROP VIEW progression_live_tasks;
             DROP VIEW progression_live_execution_tasks;
             DROP VIEW progression_ladder_families;
             DROP VIEW progression_live_families;
             DROP VIEW progression_family_keys;
             DROP VIEW progression_chain_roots;
             DROP VIEW progression_prereq_task_edges;
             ALTER TABLE tasks DROP COLUMN progression_grace_waived_at;
             ALTER TABLE work_executions DROP COLUMN created_via;
             DELETE FROM schema_migrations WHERE id IN (
                '20261106_1200_progression_grace_waived_at',
                '20261106_1205_execution_created_via',
                '20261106_1210_progression_candidate_views');"
        ))
        .unwrap();
    drop(db);
    for _ in 0..2 {
        let db = WorkDb::open(path.clone()).unwrap();
        let candidates = db.list_progression_candidates(None).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].work_item_id, id);
        assert!(db.mint_progression_execution(&id).unwrap().is_some());
        db.connect()
            .unwrap()
            .execute("UPDATE work_executions SET status = 'cancelled'", [])
            .unwrap();
    }
}

#[test]
fn dropping_the_views_without_rolling_back_the_migration_is_repaired_on_open() {
    let (_dir, path) = disk_db_path("progression-views-refresh");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_named(&db, "Refresh");
    let id = create_test_chore_manual(&db, &product.id, "Stuck").id;
    db.connect()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict' WHERE id = '{id}';
             UPDATE task_status_audit SET changed_at = changed_at - 36000 WHERE work_item_id = '{id}';
             DROP VIEW progression_candidates;
             DROP VIEW progression_candidate_clauses;
             DROP VIEW progression_real_candidates;
             DROP VIEW progression_candidate_base;
             DROP VIEW progression_run_state;
             DROP VIEW progression_live_tasks;
             DROP VIEW progression_live_execution_tasks;
             DROP VIEW progression_ladder_families;
             DROP VIEW progression_live_families;
             DROP VIEW progression_family_keys;
             DROP VIEW progression_chain_roots;
             DROP VIEW progression_prereq_task_edges;"
        ))
        .unwrap();
    drop(db);
    let db = WorkDb::open(path).unwrap();
    let candidates = db.list_progression_candidates(None).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].work_item_id, id);
}

#[test]
fn a_batch_mint_queues_every_candidate_oldest_first_and_isolates_a_failing_item() {
    let f = fixture();
    let newer = f.chore("Newer");
    let oldest = f.chore("Oldest");
    f.stick(&newer, "status = 'blocked', blocked_reason = 'merge_conflict'", 3);
    f.stick(
        &oldest,
        "status = 'blocked', blocked_reason = 'ci_failure_exhausted'",
        30,
    );
    // An item whose repository cannot be resolved cannot be given an execution;
    // it must not stop the others.
    let no_repo_product = create_test_product_with_repo(&f.db, "No repository", Some("git@example.invalid:o/r.git"));
    let unresolvable = create_test_chore_manual(&f.db, &no_repo_product.id, "No repo").id;
    f.sql(&format!(
        "UPDATE products SET repo_remote_url = NULL WHERE id = '{}'",
        no_repo_product.id
    ));
    f.stick(
        &unresolvable,
        "status = 'blocked', blocked_reason = 'merge_conflict'",
        20,
    );

    let mint = f.db.mint_progression_executions().unwrap();
    let minted: Vec<_> = mint.minted.iter().map(|(c, _)| c.work_item_id.clone()).collect();
    assert_eq!(minted, vec![oldest.clone(), newer.clone()], "oldest stuck first");
    assert_eq!(mint.failed.len(), 1);
    assert_eq!(mint.failed[0].0.work_item_id, unresolvable);
    for (candidate, execution) in &mint.minted {
        assert_eq!(execution.work_item_id, candidate.work_item_id);
        assert_eq!(execution.kind, ExecutionKind::Progression);
        assert_eq!(execution.status, ExecutionStatus::Ready);
    }
    assert!(
        f.db.list_executions(Some(&unresolvable)).unwrap().is_empty(),
        "rolled back with its savepoint"
    );

    // One run per item: the runs just queued exclude their items, so a second
    // pass mints only what is still a candidate.
    let again = f.db.mint_progression_executions().unwrap();
    assert!(again.minted.iter().all(|(c, _)| c.work_item_id == unresolvable) || again.minted.is_empty());
    assert_eq!(f.db.list_executions(Some(&oldest)).unwrap().len(), 1);
    assert_eq!(f.db.list_executions(Some(&newer)).unwrap().len(), 1);
}

#[test]
fn a_batch_mint_gives_one_run_per_pr_family_to_the_oldest_stuck_item() {
    let f = fixture();

    // Parent in_review with red CI (S6) and its CI-fix revision blocked
    // worker_failed (S7) share the chain, so they share a family_key.
    let parent = f.chore("Parent with red CI");
    f.sql(&format!(
        "UPDATE tasks SET pr_url = 'https://github.com/o/r/pull/11' WHERE id = '{parent}'"
    ));
    f.stick(&parent, "status = 'in_review', ci_required_state = 'fail'", 30);
    let fixer = f.revision_of(&parent, "ci-fix: Parent with red CI");
    f.stick(&fixer, "status = 'blocked', blocked_reason = 'worker_failed'", 3);
    f.assert_candidate(&parent, true);
    f.assert_candidate(&fixer, true);

    // Two unrelated items bound to the same PR share a family_key too.
    let older_share = f.chore("Older share");
    let newer_share = f.chore("Newer share");
    f.sql(&format!(
        "UPDATE tasks SET pr_url = 'https://github.com/o/r/pull/12' WHERE id IN ('{older_share}', '{newer_share}')"
    ));
    f.stick(
        &older_share,
        "status = 'blocked', blocked_reason = 'merge_conflict'",
        20,
    );
    f.stick(&newer_share, "status = 'blocked', blocked_reason = 'merge_conflict'", 4);
    f.assert_candidate(&older_share, true);
    f.assert_candidate(&newer_share, true);

    let mint = f.db.mint_progression_executions().unwrap();
    let minted: Vec<_> = mint.minted.iter().map(|(c, _)| c.work_item_id.clone()).collect();
    assert_eq!(
        minted,
        vec![parent.clone(), older_share.clone()],
        "one run per family, oldest-stuck wins: {minted:?}"
    );
    assert!(mint.failed.is_empty(), "{:?}", mint.failed);
    assert_eq!(f.db.list_executions(Some(&fixer)).unwrap().len(), 0);
    assert_eq!(f.db.list_executions(Some(&newer_share)).unwrap().len(), 0);
    assert_eq!(bucket(&f.db, &parent), Some(StuckBucket::ProgressionInFlight));
    assert_eq!(bucket(&f.db, &older_share), Some(StuckBucket::ProgressionInFlight));
    // The rest of each family is hidden as "related work is live": the minted
    // `ready` run is the progression activity the report exposes.
    assert_eq!(bucket(&f.db, &fixer), None);
    assert_eq!(bucket(&f.db, &newer_share), None);
}

// ── A progression run survives the reconciler's gates about the item's work ─

#[test]
fn reconcile_leaves_a_run_minted_for_a_retired_attempt_revision_alone() {
    let f = fixture();
    let parent = f.chore("Parent");
    f.sql(&format!(
        "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/o/r/pull/7' WHERE id = '{parent}'"
    ));
    let revision = f.revision_of(&parent, "Conflict fix");
    let attempt = f.insert_conflict_attempt(&parent, "sha-1");
    f.sql(&format!(
        "UPDATE conflict_resolutions SET status = 'failed' WHERE id = '{attempt}';
         UPDATE tasks SET created_via = '{prefix}{attempt}' WHERE id = '{revision}';",
        prefix = CREATED_VIA_MERGE_CONFLICT_PREFIX
    ));
    f.stick(
        &revision,
        "status = 'blocked', blocked_reason = 'spawning_attempt_retired_unsuccessfully'",
        3,
    );
    assert_eq!(f.clauses(&revision).stuck_kind.as_deref(), Some("S10_retired_attempt"));

    let run =
        f.db.mint_progression_execution(&revision)
            .unwrap()
            .expect("the S10 revision is a candidate");
    // The reconciler runs on every structural mutation; twice, a cooldown apart.
    for _ in 0..2 {
        f.db.reconcile_product_executions(&f.product).unwrap();
        let status: String =
            f.db.connect()
                .unwrap()
                .query_row("SELECT status FROM work_executions WHERE id = ?1", [&run.id], |row| {
                    row.get(0)
                })
                .unwrap();
        assert_eq!(status, "ready", "the retired-attempt gate must not withdraw the run");
    }
    let clauses = f.clauses(&revision);
    assert!(
        clauses.progression_live && !clauses.infrastructure_failing,
        "{clauses:?}"
    );
}

#[test]
fn a_run_the_engine_withdrew_before_it_started_is_not_a_failure_or_a_cooldown() {
    let f = fixture();
    let id = f.stuck_chore("Withdrawn twice");
    for ago in [30 * HOUR, 20 * HOUR] {
        finished_progression_run(&f, &id, ExecutionStatus::Abandoned, ago);
    }
    let clauses = f.clauses(&id);
    assert!(!clauses.infrastructure_failing && !clauses.in_cooldown, "{clauses:?}");
    f.assert_candidate(&id, true);
    // Once a withdrawn run had started, it is a dead run like any other.
    f.sql("UPDATE work_executions SET started_at = created_at");
    assert!(f.clauses(&id).infrastructure_failing);
}

#[test]
fn a_run_that_died_before_it_started_still_counts_as_an_infrastructure_failure() {
    // A spawn that fails before start (`failed`) or loses its worker (`orphaned`)
    // is a real infrastructure failure even with no `started_at`: only the
    // engine's own withdrawal (`abandoned`) is exempt. Not counting these would
    // let the sweep re-nominate the item on every tick.
    for status in [ExecutionStatus::Failed, ExecutionStatus::Orphaned] {
        let f = fixture();
        let id = f.stuck_chore("Died before start");
        finished_progression_run(&f, &id, status.clone(), 20 * HOUR);
        assert!(!f.clauses(&id).infrastructure_failing, "one failure is retried");
        finished_progression_run(&f, &id, status.clone(), 30 * HOUR);
        let clauses = f.clauses(&id);
        assert!(clauses.infrastructure_failing, "{status:?}: {clauses:?}");
        f.assert_candidate(&id, false);
    }
}

#[test]
fn the_report_shows_a_dead_run_as_stuck_not_as_being_handled() {
    let f = fixture();
    let id = f.stuck_chore("Run died");
    let run = finished_progression_run(&f, &id, ExecutionStatus::Failed, 0);
    assert!(f.clauses(&id).in_cooldown, "the cooldown still throttles nomination");
    assert_eq!(bucket(&f.db, &id), Some(StuckBucket::EngineStuck));
    f.sql(&format!(
        "UPDATE work_executions SET status = 'completed' WHERE id = '{run}'"
    ));
    assert_eq!(bucket(&f.db, &id), Some(StuckBucket::ProgressionInFlight));
}
