//! One test per progression verb, each with its refusals, plus the progression
//! branch of `done`. Everything runs through `submit_progression_verb` /
//! `submit_worker_proposal`, so the proposal row, the action record, the
//! provenance and the actor stamps are asserted where the engine writes them.
use super::*;
use crate::test_support::{create_test_chore_manual, create_test_product};
use boss_protocol::{
    CreateAttentionItemInput, CreateExecutionInput, ExecutionStatus, LAST_STATUS_ACTOR_PROGRESSION,
    ProgressionDisposition, WorkItem,
};
use serde_json::json;

const FINGERPRINT: &str = "fp-1";
const STUCK: &str = "S7_worker_failed";

struct Fx {
    db: WorkDb,
    product: String,
    checker: FakePrStateChecker,
    keys: std::cell::Cell<u32>,
}

fn fx() -> Fx {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product(&db).id;
    Fx {
        db,
        product,
        checker: FakePrStateChecker::always(PrOpenState::Open),
        keys: std::cell::Cell::new(0),
    }
}

impl Fx {
    fn sql(&self, sql: &str) {
        self.db.connect().unwrap().execute_batch(sql).unwrap();
    }

    fn chore(&self, name: &str) -> String {
        create_test_chore_manual(&self.db, &self.product, name).id
    }

    fn task(&self, id: &str) -> Task {
        match self.db.get_work_item(id).unwrap() {
            WorkItem::Task(task) | WorkItem::Chore(task) => task,
            other => panic!("{id} is not a task: {other:?}"),
        }
    }

    /// An item with an open PR, waiting in review.
    fn pr_item(&self, name: &str, ci: &str) -> String {
        let id = self.chore(name);
        self.sql(&format!(
            "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/spinyfin/mono/pull/{}',
                    ci_required_state = '{ci}' WHERE id = '{id}'",
            self.keys.get() + 100
        ));
        id
    }

    fn revision_of(&self, parent: &str, name: &str, status: &str) -> String {
        let id = self.chore(name);
        self.sql(&format!(
            "UPDATE tasks SET kind = 'revision', parent_task_id = '{parent}', status = '{status}' WHERE id = '{id}'"
        ));
        id
    }

    /// Put `id` in `blocked: worker_failed`, recorded as produced by `basis`.
    fn fail(&self, id: &str, basis: &str) {
        crate::task_status_audit::execute(
            &self.db.connect().unwrap(),
            basis,
            "engine",
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed', autostart = 0 WHERE id = ?1",
            [id],
        )
        .unwrap();
    }

    /// A live progression run bound to `item`, spawned for the current graph and
    /// with its diagnosis episode recorded.
    fn run(&self, item: &str, fingerprint: &str, stuck_kind: &str) -> String {
        let execution = self
            .db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(item)
                    .kind(ExecutionKind::Progression)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();
        self.db.capture_progression_neighbourhood(&execution.id).unwrap();
        self.db
            .record_progression_episode(&execution.id, fingerprint, stuck_kind)
            .unwrap();
        execution.id
    }

    fn submit(
        &self,
        execution: &str,
        kind: ProposalKind,
        payload: serde_json::Value,
    ) -> std::result::Result<SubmitWorkerProposalOutcome, ProposalSubmissionError> {
        let n = self.keys.get() + 1;
        self.keys.set(n);
        let item = self.db.work_item_for_execution(execution).unwrap().unwrap();
        let payload_json = boss_engine_proposal_validation::validate_payload(kind, &payload)
            .unwrap_or_else(|errors| panic!("{kind} payload invalid: {errors:?}"))
            .canonical_json;
        self.db
            .submit_progression_verb(
                SubmitWorkerProposalInput {
                    execution_id: execution,
                    work_item_id: &item,
                    kind,
                    payload_json: &payload_json,
                    idempotency_key: &format!("key-{n}"),
                },
                &self.checker,
            )
            .unwrap()
    }

    fn applied(&self, execution: &str, kind: ProposalKind, payload: serde_json::Value) -> String {
        let outcome = self
            .submit(execution, kind, payload)
            .unwrap_or_else(|error| panic!("{kind} refused: {error}"));
        assert_eq!(outcome.proposal.state, ProposalState::Applied);
        outcome
            .proposal
            .applied_ref
            .expect("an applied verb names what it touched")
    }

    fn refused(&self, execution: &str, kind: ProposalKind, payload: serde_json::Value) -> String {
        let error = self
            .submit(execution, kind, payload)
            .expect_err(&format!("{kind} should have been refused"));
        assert_eq!(error.code, ProposalErrorCode::NotAccepted);
        error.message
    }

    fn actions(&self, item: &str) -> Vec<(String, String, String, String, String)> {
        let conn = self.db.connect().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT verb, fingerprint, stuck_kind, subject, execution_id FROM progression_actions
                 WHERE work_item_id = ?1 ORDER BY rowid",
            )
            .unwrap();
        stmt.query_map([item], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    fn stored_proposals(&self, item: &str) -> usize {
        self.db
            .list_worker_proposals_for_work_item(item, None, None)
            .unwrap()
            .len()
    }

    fn executions(&self, item: &str) -> Vec<WorkExecution> {
        self.db
            .list_executions(Some(item))
            .unwrap()
            .into_iter()
            .filter(|execution| execution.kind != ExecutionKind::Progression)
            .collect()
    }

    fn created_via(&self, execution: &str) -> Option<String> {
        self.db
            .connect()
            .unwrap()
            .query_row(
                "SELECT created_via FROM work_executions WHERE id = ?1",
                [execution],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn actor(&self, id: &str) -> String {
        self.db
            .connect()
            .unwrap()
            .query_row("SELECT last_status_actor FROM tasks WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .unwrap()
    }
}

// ── revise ───────────────────────────────────────────────────────────────────

#[test]
fn revise_creates_a_progression_revision_on_the_items_open_pr() {
    let fx = fx();
    let item = fx.pr_item("Red PR", "fail");
    let run = fx.run(&item, FINGERPRINT, "S6_ci_failure");

    let revision = fx.applied(
        &run,
        ProposalKind::Revise,
        json!({"brief": "The check fails for X; fix Y."}),
    );

    let task = fx.task(&revision);
    assert_eq!(task.kind, TaskKind::Revision);
    assert_eq!(task.parent_task_id.as_deref(), Some(item.as_str()));
    assert_eq!(task.created_via, format!("progression:{run}"));
    assert_eq!(fx.actor(&revision), LAST_STATUS_ACTOR_PROGRESSION);
    assert_eq!(task.description, "The check fails for X; fix Y.");
    assert!(
        !fx.executions(&revision).is_empty(),
        "the revision is born with its execution"
    );
    assert_eq!(
        fx.actions(&item),
        vec![(
            "revise".into(),
            FINGERPRINT.into(),
            "S6_ci_failure".into(),
            String::new(),
            run.clone()
        )]
    );
    let stored = fx
        .db
        .list_worker_proposals_for_work_item(&item, Some(ProposalKind::Revise), None)
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].applied_ref.as_deref(), Some(revision.as_str()));
}

#[test]
fn revise_refuses_a_closed_pr_and_stores_nothing() {
    let mut fx = fx();
    let item = fx.pr_item("Closed PR", "fail");
    let run = fx.run(&item, FINGERPRINT, "S6_ci_failure");
    fx.checker = FakePrStateChecker::always(PrOpenState::ClosedUnmerged);

    let message = fx.refused(&run, ProposalKind::Revise, json!({"brief": "Try again"}));

    assert!(message.contains("closed without merging"), "{message}");
    assert_eq!(fx.stored_proposals(&item), 0, "a refusal stores no proposal row");
    assert!(fx.actions(&item).is_empty());
    let children: i64 = fx
        .db
        .connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1", [&item], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(children, 0, "the transaction rolled back");
}

#[test]
fn revise_is_refused_on_a_repeat_one_open_revision_and_past_the_budget() {
    let fx = fx();
    let item = fx.pr_item("Red PR", "fail");
    let first = fx.run(&item, FINGERPRINT, "S6_ci_failure");
    let revision = fx.applied(&first, ProposalKind::Revise, json!({"brief": "One"}));

    // The same verb on the same fingerprint names the prior run, from the same
    // run and from a later one.
    let again = fx.refused(&first, ProposalKind::Revise, json!({"brief": "Two"}));
    assert!(again.contains(&first) && again.contains(FINGERPRINT), "{again}");
    let second = fx.run(&item, FINGERPRINT, "S6_ci_failure");
    assert!(
        fx.refused(&second, ProposalKind::Revise, json!({"brief": "Three"}))
            .contains(&first)
    );

    // A new fingerprint is not a repeat, but the first revision is still open.
    let third = fx.run(&item, "fp-2", "S6_ci_failure");
    let open = fx.refused(&third, ProposalKind::Revise, json!({"brief": "Four"}));
    assert!(
        open.contains(&revision) && open.contains("open progression revision"),
        "{open}"
    );

    // Past the create budget the agent is told to ask, whatever else holds.
    fx.db
        .set_metadata("progression.max_creates_per_item_window", "1")
        .unwrap();
    let budget = fx.refused(&third, ProposalKind::Revise, json!({"brief": "Five"}));
    assert!(
        budget.contains("create budget") && budget.contains("Ask the owner"),
        "{budget}"
    );
}

// ── precursor ────────────────────────────────────────────────────────────────

#[test]
fn precursor_creates_the_chore_and_the_dependency_edge() {
    let fx = fx();
    let item = fx.pr_item("Red PR", "fail");
    let run = fx.run(&item, FINGERPRINT, "S6_ci_failure");

    let chore = fx.applied(
        &run,
        ProposalKind::Precursor,
        json!({"name": "Fix the flaky fixture on main", "brief": "It fails on main too."}),
    );

    let task = fx.task(&chore);
    assert_eq!(task.name, "Fix the flaky fixture on main");
    assert_eq!(task.status, TaskStatus::Todo);
    assert!(task.autostart);
    assert_eq!(task.priority, "medium");
    assert_eq!(task.product_id, fx.product);
    assert_eq!(task.created_via, format!("progression:{run}"));
    assert_eq!(fx.actor(&chore), LAST_STATUS_ACTOR_PROGRESSION);
    assert!(
        crate::work_dependencies::query_edge(&fx.db.connect().unwrap(), &item, &chore, "blocks")
            .unwrap()
            .is_some(),
        "the item depends on the precursor"
    );
    let waiting = fx.task(&item);
    assert_eq!(waiting.status, TaskStatus::Blocked);
    assert_eq!(waiting.blocked_reason.as_deref(), Some("dependency"));
    assert_progression_status(&fx, &item);
    fx.sql(&format!("UPDATE tasks SET status = 'done' WHERE id = '{chore}'"));
    crate::dep_helpers::cascade_dependents_after_prereq_status_change(
        &mut PendingEvents::new(),
        &fx.db.connect().unwrap(),
        &chore,
        "done",
        &now_string(),
    )
    .unwrap();
    assert_eq!(fx.task(&item).status, TaskStatus::InReview);
}

#[test]
fn precursor_allows_one_open_precursor_and_counts_against_the_budget() {
    let fx = fx();
    let item = fx.pr_item("Red PR", "fail");
    let first = fx.run(&item, FINGERPRINT, "S6_ci_failure");
    let chore = fx.applied(&first, ProposalKind::Precursor, json!({"name": "First", "brief": "b"}));

    let second = fx.run(&item, "fp-2", "S6_ci_failure");
    let open = fx.refused(
        &second,
        ProposalKind::Precursor,
        json!({"name": "Second", "brief": "b"}),
    );
    assert!(
        open.contains(&chore) && open.contains("open progression-created precursor"),
        "{open}"
    );

    // Once the first is done, the budget (a creation counted by both the candidate
    // view and the action record) is what stops a third.
    fx.sql(&format!("UPDATE tasks SET status = 'done' WHERE id = '{chore}'"));
    fx.db
        .set_metadata("progression.max_creates_per_item_window", "1")
        .unwrap();
    assert!(
        fx.refused(&second, ProposalKind::Precursor, json!({"name": "Third", "brief": "b"}))
            .contains("budget")
    );
}

#[test]
fn precursor_is_refused_while_work_is_live_on_the_item() {
    let fx = fx();
    let item = fx.pr_item("Busy", "fail");
    fx.sql(&format!("UPDATE tasks SET status = 'active' WHERE id = '{item}'"));
    fx.db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&item)
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let run = fx.run(&item, FINGERPRINT, STUCK);
    let message = fx.refused(&run, ProposalKind::Precursor, json!({"name": "Fix", "brief": "b"}));
    assert!(message.contains("live or pending work"), "{message}");
}

// ── depend / undepend ────────────────────────────────────────────────────────

#[test]
fn depend_adds_an_edge_inside_the_neighbourhood_and_refuses_outside_it() {
    let fx = fx();
    let item = fx.pr_item("Item", "fail");
    let revision = fx.revision_of(&item, "Its revision", "blocked");
    let outside = fx.chore("Two hops away");
    let run = fx.run(&item, FINGERPRINT, STUCK);

    let target = fx.applied(&run, ProposalKind::Depend, json!({"on": revision}));

    assert_eq!(target, revision);
    assert_progression_status(&fx, &item);
    assert!(
        crate::work_dependencies::query_edge(&fx.db.connect().unwrap(), &item, &revision, "blocks")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        fx.actions(&item),
        vec![(
            "depend".into(),
            FINGERPRINT.into(),
            STUCK.into(),
            revision.clone(),
            run.clone()
        )]
    );
    let escaped = fx.refused(&run, ProposalKind::Depend, json!({"on": outside}));
    assert!(escaped.contains("outside the neighbourhood"), "{escaped}");
    let itself = fx.refused(&run, ProposalKind::Depend, json!({"on": item}));
    assert!(itself.contains("itself"), "{itself}");
}

#[test]
fn undepend_drops_only_a_prerequisite_that_no_longer_holds_the_item_up() {
    let fx = fx();
    let item = fx.chore("Waiting");
    let live = fx.chore("Still running");
    let finished = fx.chore("Landed");
    fx.sql(&format!("UPDATE tasks SET status = 'done' WHERE id = '{finished}'"));
    fx.sql(&format!(
        "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, created_at) VALUES
             ('{item}', '{live}', '1'), ('{item}', '{finished}', '1')"
    ));
    let run = fx.run(&item, FINGERPRINT, "S4_dependency");

    let refused = fx.refused(&run, ProposalKind::Undepend, json!({"on": live}));
    assert!(refused.contains("still waiting on it"), "{refused}");
    fx.applied(&run, ProposalKind::Undepend, json!({"on": finished}));
    let conn = fx.db.connect().unwrap();
    assert!(
        crate::work_dependencies::query_edge(&conn, &item, &finished, "blocks")
            .unwrap()
            .is_none()
    );
    assert!(
        crate::work_dependencies::query_edge(&conn, &item, &live, "blocks")
            .unwrap()
            .is_some()
    );
    drop(conn);
    let missing = fx.refused(&run, ProposalKind::Undepend, json!({"on": finished}));
    assert!(missing.contains("does not depend on"), "{missing}");
    let outside = fx.chore("Unrelated");
    assert!(
        fx.refused(&run, ProposalKind::Undepend, json!({"on": outside}))
            .contains("outside the neighbourhood")
    );
}

#[test]
fn undepend_may_drop_a_dead_revision_whose_parent_pr_is_green() {
    let fx = fx();
    let parent = fx.pr_item("Parent", "success");
    let dead = fx.revision_of(&parent, "Dead revision", "blocked");
    let waiting = fx.revision_of(&parent, "Later revision", "blocked");
    fx.sql(&format!(
        "UPDATE tasks SET blocked_reason = 'dependency' WHERE id = '{waiting}';
         INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, created_at) VALUES ('{waiting}', '{dead}', '1')"
    ));
    let run = fx.run(&waiting, FINGERPRINT, "S4_dependency");

    fx.applied(&run, ProposalKind::Undepend, json!({"on": dead}));

    assert_progression_status(&fx, &waiting);
    let released = fx.task(&waiting);
    assert_eq!(
        released.status,
        TaskStatus::Todo,
        "the engine-owned dependency block lifts"
    );
}

// ── redispatch ───────────────────────────────────────────────────────────────

#[test]
fn redispatch_on_a_deliberate_park_clears_the_stamp_and_resolves_its_attention_in_one_transaction() {
    let fx = fx();
    let item = fx.chore("Parked by a worker");
    let earlier = fx
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&item)
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Failed)
                .build(),
        )
        .unwrap();
    let attention = fx
        .db
        .create_attention_item(
            CreateAttentionItemInput::builder()
                .execution_id(&earlier.id)
                .kind("run_done_declared_blocked")
                .title("Worker declared a blocker")
                .body_markdown("needs approval")
                .build(),
        )
        .unwrap();
    fx.sql(&format!(
        "UPDATE tasks SET autostart = 0, dispatch_failed_reason = 'deliberate_park',
                dispatch_failed_error = 'blocked', dispatch_failed_at = '1' WHERE id = '{item}'"
    ));
    let run = fx.run(&item, FINGERPRINT, "S3_deliberate_park");

    let minted = fx.applied(
        &run,
        ProposalKind::Redispatch,
        json!({"brief_addendum": "The approval the worker asked for is already recorded."}),
    );

    let task = fx.task(&item);
    assert_eq!(task.dispatch_failed_reason, None, "the stamp is cleared");
    assert!(
        task.description.contains("already recorded"),
        "the addendum rides on the brief"
    );
    assert_eq!(fx.actor(&item), LAST_STATUS_ACTOR_PROGRESSION);
    let resolved = fx.db.get_attention_item(&attention.id).unwrap();
    assert_eq!(
        resolved.status, "resolved",
        "the park's attention resolves with the start"
    );
    let execution = fx.db.get_execution(&minted).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Ready);
    assert_eq!(fx.created_via(&minted), Some(format!("progression:{run}")));
}

#[test]
fn redispatch_clears_every_non_operator_worker_failure_on_primary_and_revision_items() {
    for basis in [
        "declared_blocked",
        "nudge_breaker",
        "delivered_without_pr",
        "foreign_pr_binding",
    ] {
        let fx = fx();
        let primary = fx.chore("Primary");
        fx.fail(&primary, basis);
        let run = fx.run(&primary, FINGERPRINT, STUCK);
        let minted = fx.applied(&run, ProposalKind::Redispatch, json!({}));
        assert_eq!(
            fx.db.get_execution(&minted).unwrap().status,
            ExecutionStatus::Ready,
            "{basis}"
        );
        assert_eq!(fx.task(&primary).blocked_reason, None, "{basis}");
        assert_eq!(fx.created_via(&minted), Some(format!("progression:{run}")), "{basis}");

        let parent = fx.pr_item("Parent", "success");
        let revision = fx.revision_of(&parent, "Revision", "todo");
        fx.fail(&revision, basis);
        let run = fx.run(&revision, FINGERPRINT, STUCK);
        let minted = fx.applied(&run, ProposalKind::Redispatch, json!({}));
        let execution = fx.db.get_execution(&minted).unwrap();
        assert_eq!(execution.kind, ExecutionKind::RevisionImplementation, "{basis}");
        assert_eq!(fx.created_via(&minted), Some(format!("progression:{run}")), "{basis}");
    }
}

#[test]
fn redispatch_is_refused_for_live_work_questions_refusals_unknown_provenance_and_parked_items() {
    let fx = fx();

    let busy = fx.chore("Busy");
    fx.fail(&busy, "declared_blocked");
    fx.db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&busy)
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let run = fx.run(&busy, FINGERPRINT, STUCK);
    assert!(
        fx.refused(&run, ProposalKind::Redispatch, json!({}))
            .contains("never fights work in flight")
    );

    let asked = fx.chore("Waiting on the owner");
    let asker = fx.run(&asked, FINGERPRINT, STUCK);
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'awaiting_operator_answer' WHERE id = '{asked}'"
    ));
    assert!(
        fx.refused(&asker, ProposalKind::Redispatch, json!({}))
            .contains("open question")
    );

    let refused = fx.chore("Owner said no");
    fx.fail(&refused, "operator_no");
    let run = fx.run(&refused, FINGERPRINT, STUCK);
    assert!(
        fx.refused(&run, ProposalKind::Redispatch, json!({}))
            .contains("owner refused")
    );

    let legacy = fx.chore("Legacy failure");
    fx.fail(&legacy, "unknown_legacy");
    let run = fx.run(&legacy, FINGERPRINT, STUCK);
    assert!(
        fx.refused(&run, ProposalKind::Redispatch, json!({}))
            .contains("unknown_legacy")
    );

    let parked = fx.chore("Parked");
    fx.fail(&parked, "declared_blocked");
    let run = fx.run(&parked, FINGERPRINT, STUCK);
    fx.sql(&format!(
        "UPDATE tasks SET parked_at = '1', parked_by = 'human' WHERE id = '{parked}'"
    ));
    assert!(fx.refused(&run, ProposalKind::Redispatch, json!({})).contains("parked"));
}

#[test]
fn redispatch_is_bounded_across_fingerprints_until_a_work_run_completes() {
    let fx = fx();
    let item = fx.chore("Keeps failing");
    fx.fail(&item, "declared_blocked");
    let first = fx.run(&item, "fp-1", STUCK);
    let minted = fx.applied(&first, ProposalKind::Redispatch, json!({}));

    // The restart fails and the item is stuck again, under a new fingerprint.
    fx.sql(&format!(
        "UPDATE work_executions SET status = 'failed' WHERE id = '{minted}'"
    ));
    fx.fail(&item, "declared_blocked");
    let second = fx.run(&item, "fp-2", STUCK);
    let bound = fx.refused(&second, ProposalKind::Redispatch, json!({}));
    assert!(bound.contains("redispatch bound") && bound.contains(&first), "{bound}");
    assert_eq!(fx.actions(&item).len(), 1, "a refusal records nothing");

    // Parking and un-parking, a progression run ending, and a different stuck kind
    // do not reset it; the bound is per stuck kind.
    let other_kind = fx.run(&item, "fp-3", "S2_churn_guard");
    fx.sql(&format!(
        "UPDATE tasks SET dispatch_failed_reason = 'churn_guard', status = 'todo', blocked_reason = NULL WHERE id = '{item}'"
    ));
    fx.applied(&other_kind, ProposalKind::Redispatch, json!({}));

    // A completed work run resets it.
    fx.sql(&format!(
        "UPDATE work_executions SET status = 'failed' WHERE work_item_id = '{item}' AND kind <> 'progression'"
    ));
    let later = fx
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&item)
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    fx.sql(&format!(
        "UPDATE work_executions SET created_at = CAST(created_at AS INTEGER) + 5 WHERE id = '{}'",
        later.id
    ));
    fx.fail(&item, "declared_blocked");
    let third = fx.run(&item, "fp-4", STUCK);
    fx.applied(&third, ProposalKind::Redispatch, json!({}));
}

#[test]
fn redispatch_of_an_item_in_review_restarts_its_dead_review() {
    let fx = fx();
    let item = fx.pr_item("Reviewed PR", "success");
    fx.sql(&format!(
        "INSERT INTO work_attention_items (id, execution_id, work_item_id, kind, status, title, body_markdown, created_at)
         VALUES ('atn_churn', NULL, '{item}', 'churn_guard_parked', 'open', 'Review churn', 'parked', '1')"
    ));
    let run = fx.run(&item, FINGERPRINT, "S16_review_churn");

    let minted = fx.applied(&run, ProposalKind::Redispatch, json!({}));

    assert_eq!(fx.db.get_execution(&minted).unwrap().kind, ExecutionKind::PrReview);
    assert_eq!(fx.created_via(&minted), Some(format!("progression:{run}")));
    assert_eq!(fx.db.get_attention_item("atn_churn").unwrap().status, "resolved");
}

// ── clear-signal ─────────────────────────────────────────────────────────────

#[test]
fn clear_signal_clears_a_block_only_when_the_stored_probe_agrees() {
    let fx = fx();
    let item = fx.pr_item("Conflicted", "success");
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict', pr_mergeable_state = 'CONFLICTING' WHERE id = '{item}'"
    ));
    let run = fx.run(&item, FINGERPRINT, "S5_merge_conflict");

    let disagrees = fx.refused(&run, ProposalKind::ClearSignal, json!({"reason": "merge_conflict"}));
    assert!(
        disagrees.contains("probe disagrees") && disagrees.contains("CONFLICTING"),
        "{disagrees}"
    );
    assert_eq!(fx.task(&item).blocked_reason.as_deref(), Some("merge_conflict"));
    assert!(fx.actions(&item).is_empty());

    fx.sql(&format!(
        "UPDATE tasks SET pr_mergeable_state = 'MERGEABLE' WHERE id = '{item}'"
    ));
    fx.applied(&run, ProposalKind::ClearSignal, json!({"reason": "merge_conflict"}));
    let cleared = fx.task(&item);
    assert_eq!(cleared.status, TaskStatus::InReview);
    assert_eq!(cleared.blocked_reason, None);
    assert_eq!(fx.actor(&item), LAST_STATUS_ACTOR_PROGRESSION);
    assert_eq!(
        fx.actions(&item),
        vec![(
            "clear_signal".into(),
            FINGERPRINT.into(),
            "S5_merge_conflict".into(),
            "merge_conflict".into(),
            run.clone()
        )]
    );
}

#[test]
fn clear_signal_covers_ci_and_refuses_a_block_of_another_reason() {
    let fx = fx();
    let item = fx.pr_item("Exhausted", "fail");
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure_exhausted' WHERE id = '{item}'"
    ));
    let run = fx.run(&item, FINGERPRINT, "S6_ci_failure");

    assert!(
        fx.refused(&run, ProposalKind::ClearSignal, json!({"reason": "ci_failure"}))
            .contains("probe disagrees")
    );
    assert!(
        fx.refused(&run, ProposalKind::ClearSignal, json!({"reason": "merge_conflict"}))
            .contains("is not `blocked: merge_conflict`")
    );
    fx.sql(&format!(
        "UPDATE tasks SET ci_required_state = 'success' WHERE id = '{item}'"
    ));
    fx.applied(&run, ProposalKind::ClearSignal, json!({"reason": "ci_failure"}));
    assert_eq!(fx.task(&item).status, TaskStatus::InReview);
    // The repeat is refused even though the state moved on: same verb, same fingerprint.
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure_exhausted' WHERE id = '{item}'"
    ));
    assert!(
        fx.refused(&run, ProposalKind::ClearSignal, json!({"reason": "ci_failure"}))
            .contains(&run)
    );
}

// ── archive-revision ─────────────────────────────────────────────────────────

#[test]
fn archive_revision_retires_a_dead_revision_and_releases_the_one_queued_behind_it() {
    let fx = fx();
    let parent = fx.pr_item("Parent", "success");
    let dead = fx.revision_of(&parent, "Dead revision", "blocked");
    fx.fail(&dead, "declared_blocked");
    let queued = fx.revision_of(&parent, "Queued behind it", "blocked");
    fx.sql(&format!(
        "UPDATE tasks SET blocked_reason = 'dependency' WHERE id = '{queued}';
         INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, created_at) VALUES ('{queued}', '{dead}', '1')"
    ));
    let run = fx.run(&parent, FINGERPRINT, "S13_revision_chain");

    let archived = fx.applied(
        &run,
        ProposalKind::ArchiveRevision,
        json!({"revision": dead, "reason": "dead; parent is green"}),
    );

    assert_eq!(archived, dead);
    let task = fx.task(&dead);
    assert_eq!(task.status, TaskStatus::Archived);
    assert_eq!(task.archived_reason.as_deref(), Some("dead; parent is green"));
    assert_eq!(fx.actor(&dead), LAST_STATUS_ACTOR_PROGRESSION);
    assert_eq!(
        fx.task(&queued).status,
        TaskStatus::Todo,
        "the later revision is released"
    );
    assert_eq!(fx.task(&parent).status, TaskStatus::InReview, "never the item itself");
    // The same call twice is a repeat; a different revision is not.
    assert!(
        fx.refused(
            &run,
            ProposalKind::ArchiveRevision,
            json!({"revision": dead, "reason": "again"})
        )
        .contains("already")
    );
    let another = fx.revision_of(&parent, "Another dead one", "blocked");
    let second = fx.run(&parent, "fp-2", "S13_revision_chain");
    fx.applied(
        &second,
        ProposalKind::ArchiveRevision,
        json!({"revision": another, "reason": "dead too"}),
    );
}

#[test]
fn archive_revision_refuses_non_revisions_the_item_itself_live_work_and_a_red_parent() {
    let fx = fx();
    let parent = fx.pr_item("Parent", "success");
    let prerequisite = fx.chore("A prerequisite");
    fx.sql(&format!(
        "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, created_at) VALUES ('{parent}', '{prerequisite}', '1')"
    ));
    let live = fx.revision_of(&parent, "Being worked", "active");
    fx.db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&live)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let run = fx.run(&parent, FINGERPRINT, "S13_revision_chain");

    assert!(
        fx.refused(
            &run,
            ProposalKind::ArchiveRevision,
            json!({"revision": prerequisite, "reason": "x"})
        )
        .contains("not a revision")
    );
    assert!(
        fx.refused(
            &run,
            ProposalKind::ArchiveRevision,
            json!({"revision": parent, "reason": "x"})
        )
        .contains("never the item itself")
    );
    assert!(
        fx.refused(
            &run,
            ProposalKind::ArchiveRevision,
            json!({"revision": live, "reason": "x"})
        )
        .contains("live or pending work")
    );

    let dead = fx.revision_of(&parent, "Dead", "blocked");
    fx.sql(&format!(
        "UPDATE tasks SET ci_required_state = 'fail' WHERE id = '{parent}'"
    ));
    let red = fx.run(&parent, "fp-2", "S13_revision_chain");
    assert!(
        fx.refused(
            &red,
            ProposalKind::ArchiveRevision,
            json!({"revision": dead, "reason": "x"})
        )
        .contains("not green")
    );
    let outside = fx.revision_of(&fx.chore("Another PR"), "Someone else's revision", "blocked");
    assert!(
        fx.refused(
            &red,
            ProposalKind::ArchiveRevision,
            json!({"revision": outside, "reason": "x"})
        )
        .contains("outside the neighbourhood")
    );
}

// ── unblock ──────────────────────────────────────────────────────────────────

#[test]
fn unblock_moves_a_blocked_item_to_todo_once_nothing_holds_it() {
    let fx = fx();
    let item = fx.chore("Stale block");
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'dependency' WHERE id = '{item}'"
    ));
    let run = fx.run(&item, FINGERPRINT, "S4_dependency");

    fx.applied(&run, ProposalKind::Unblock, json!({}));

    let task = fx.task(&item);
    assert_eq!(task.status, TaskStatus::Todo);
    assert_eq!(task.blocked_reason, None);
    assert_eq!(fx.actor(&item), LAST_STATUS_ACTOR_PROGRESSION);
    assert!(fx.refused(&run, ProposalKind::Unblock, json!({})).contains("is `todo`"));
}

#[test]
fn unblock_refuses_a_block_whose_reason_has_its_own_verb_or_belongs_to_a_person() {
    let fx = fx();
    let gated = fx.chore("Still gated");
    let prerequisite = fx.chore("Not done");
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'dependency' WHERE id = '{gated}';
         INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, created_at) VALUES ('{gated}', '{prerequisite}', '1')"
    ));
    let run = fx.run(&gated, FINGERPRINT, "S4_dependency");
    assert!(
        fx.refused(&run, ProposalKind::Unblock, json!({}))
            .contains("still gated")
    );

    for (reason, hint) in [("worker_failed", "redispatch"), ("merge_conflict", "clear-signal")] {
        let item = fx.chore(reason);
        fx.sql(&format!(
            "UPDATE tasks SET status = 'blocked', blocked_reason = '{reason}' WHERE id = '{item}'"
        ));
        let run = fx.run(&item, FINGERPRINT, STUCK);
        assert!(
            fx.refused(&run, ProposalKind::Unblock, json!({})).contains(hint),
            "{reason}"
        );
    }
    let manual = fx.chore("Blocked by hand");
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', last_status_actor = 'human' WHERE id = '{manual}'"
    ));
    let run = fx.run(&manual, FINGERPRINT, STUCK);
    assert!(
        fx.refused(&run, ProposalKind::Unblock, json!({}))
            .contains("by a person")
    );
}

// ── the run and the transaction ──────────────────────────────────────────────

#[test]
fn a_verb_needs_a_live_progression_run_with_a_recorded_episode() {
    let fx = fx();
    let item = fx.chore("Stuck");
    fx.fail(&item, "declared_blocked");

    // An ordinary worker run cannot submit a progression verb.
    let worker = fx
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&item)
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    assert!(
        fx.refused(&worker.id, ProposalKind::Redispatch, json!({}))
            .contains("only a live progression run")
    );

    // A progression run whose brief was never composed has no fingerprint.
    let bare = fx
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&item)
                .kind(ExecutionKind::Progression)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    fx.db.capture_progression_neighbourhood(&bare.id).unwrap();
    assert!(
        fx.refused(&bare.id, ProposalKind::Redispatch, json!({}))
            .contains("no diagnosis episode")
    );

    // A finished run cannot act.
    let done = fx.run(&item, FINGERPRINT, STUCK);
    fx.sql(&format!(
        "UPDATE work_executions SET status = 'completed' WHERE id = '{done}'"
    ));
    assert!(
        fx.refused(&done, ProposalKind::Redispatch, json!({}))
            .contains("only a live progression run")
    );
    assert_eq!(fx.stored_proposals(&item), 0);
}

#[test]
fn a_replayed_submission_does_not_apply_twice_and_the_generic_pipeline_refuses_verbs() {
    let fx = fx();
    let item = fx.pr_item("Red PR", "fail");
    let run = fx.run(&item, FINGERPRINT, "S6_ci_failure");
    let payload = json!({"brief": "Fix it"});
    let canonical = boss_engine_proposal_validation::validate_payload(ProposalKind::Revise, &payload)
        .unwrap()
        .canonical_json;
    let input = || SubmitWorkerProposalInput {
        execution_id: &run,
        work_item_id: &item,
        kind: ProposalKind::Revise,
        payload_json: &canonical,
        idempotency_key: "same-key",
    };

    let first = fx.db.submit_progression_verb(input(), &fx.checker).unwrap().unwrap();
    let replay = fx.db.submit_progression_verb(input(), &fx.checker).unwrap().unwrap();

    assert!(!first.already_submitted && replay.already_submitted);
    assert_eq!(replay.proposal.id, first.proposal.id);
    assert_eq!(fx.actions(&item).len(), 1);
    let generic = fx.db.submit_worker_proposal(input()).unwrap_err().to_string();
    assert!(generic.contains("progression verb"), "{generic}");
}

// ── done ─────────────────────────────────────────────────────────────────────

fn done(fx: &Fx, run: &str, payload: serde_json::Value, key: &str) -> SubmitWorkerProposalOutcome {
    let item = fx.db.work_item_for_execution(run).unwrap().unwrap();
    let canonical = boss_engine_proposal_validation::validate_payload(ProposalKind::RunDone, &payload)
        .unwrap_or_else(|errors| panic!("done payload invalid: {errors:?}"))
        .canonical_json;
    fx.db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: run,
            work_item_id: &item,
            kind: ProposalKind::RunDone,
            payload_json: &canonical,
            idempotency_key: key,
        })
        .unwrap()
        .unwrap()
}

fn question() -> serde_json::Value {
    json!({"text": "Merge this PR?", "answer_type": {"kind": "yes_no"}, "explanation": "Merging is human-only."})
}

#[test]
fn done_records_the_disposition_and_a_delivered_run_looks_for_no_pr() {
    let fx = fx();
    for (outcome, disposition) in [
        ("delivered", ProgressionDisposition::Progressed),
        ("no_changes_needed", ProgressionDisposition::NoAction),
    ] {
        let item = fx.chore(outcome);
        let run = fx.run(&item, FINGERPRINT, STUCK);
        assert_eq!(fx.db.progression_disposition(&run).unwrap(), None);

        let outcome = done(
            &fx,
            &run,
            json!({"outcome": outcome, "summary": "Looked; nothing to do."}),
            "k",
        );

        assert_eq!(outcome.proposal.state, ProposalState::Applied);
        assert_eq!(fx.db.progression_disposition(&run).unwrap(), Some(disposition));
        // Finishing the run never needs a PR, a lease or a checkout.
        let finished = fx.db.finish_progression_declared(&run).unwrap().expect("finished");
        assert_eq!(finished.status, ExecutionStatus::Completed);
        assert_eq!(fx.task(&item).status, TaskStatus::Todo, "the item itself is untouched");
        assert!(fx.db.finish_progression_declared(&run).unwrap().is_none(), "idempotent");
    }
}

#[test]
fn a_bare_blocked_and_a_question_without_a_criterion_are_refused_for_a_progression_run() {
    let fx = fx();
    let item = fx.chore("Stuck");
    let run = fx.run(&item, FINGERPRINT, STUCK);

    let bare = done(&fx, &run, json!({"outcome": "blocked", "summary": "Stuck"}), "bare");
    assert_eq!(bare.proposal.state, ProposalState::Rejected);
    assert!(
        bare.proposal
            .decision_reason
            .unwrap()
            .contains("bare `blocked` is refused")
    );

    let no_criterion = done(
        &fx,
        &run,
        json!({"outcome": "blocked", "summary": "Stuck", "question": question()}),
        "nc",
    );
    assert_eq!(no_criterion.proposal.state, ProposalState::Rejected);
    assert!(no_criterion.proposal.decision_reason.unwrap().contains("--criterion"));
    assert_eq!(
        fx.db.progression_disposition(&run).unwrap(),
        None,
        "a refusal records no disposition"
    );
    assert!(fx.db.list_operator_questions(&item).unwrap().is_empty());

    // An ordinary worker run keeps the historical contract: a bare `blocked` is accepted.
    let worker = fx
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&item)
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let accepted = done(
        &fx,
        &worker.id,
        json!({"outcome": "blocked", "summary": "Stuck"}),
        "worker",
    );
    assert_eq!(accepted.proposal.state, ProposalState::Applied);
}

#[test]
fn no_changes_needed_is_refused_for_a_declared_blocker_item() {
    let fx = fx();
    let item = fx.chore("A worker said it was blocked");
    fx.fail(&item, "declared_blocked");
    let run = fx.run(&item, FINGERPRINT, STUCK);
    let refused = done(
        &fx,
        &run,
        json!({"outcome": "no_changes_needed", "summary": "Fine as is"}),
        "k1",
    );
    assert_eq!(refused.proposal.state, ProposalState::Rejected);
    assert!(refused.proposal.decision_reason.unwrap().contains("declared a blocker"));

    let parked = fx.chore("Parked by a worker");
    fx.sql(&format!(
        "UPDATE tasks SET autostart = 0, dispatch_failed_reason = 'deliberate_park' WHERE id = '{parked}'"
    ));
    let run = fx.run(&parked, FINGERPRINT, "S3_deliberate_park");
    assert_eq!(
        done(
            &fx,
            &run,
            json!({"outcome": "no_changes_needed", "summary": "Fine"}),
            "k2"
        )
        .proposal
        .state,
        ProposalState::Rejected
    );

    // A failure the engine produced is not a declared blocker, and neither is a healthy item.
    let delivered = fx.chore("Delivered without a PR");
    fx.fail(&delivered, "delivered_without_pr");
    let run = fx.run(&delivered, FINGERPRINT, STUCK);
    assert_eq!(
        done(
            &fx,
            &run,
            json!({"outcome": "no_changes_needed", "summary": "Fine"}),
            "k3"
        )
        .proposal
        .state,
        ProposalState::Applied
    );
}

#[test]
fn asking_parks_the_item_on_the_question_and_completes_the_run_as_asked() {
    let fx = fx();
    let item = fx.pr_item("Needs a merge", "success");
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed' WHERE id = '{item}'"
    ));
    let run = fx.run(&item, FINGERPRINT, STUCK);

    let asked = done(
        &fx,
        &run,
        json!({"outcome": "blocked", "summary": "Needs the owner", "question": question(), "criterion": "1"}),
        "ask",
    );

    assert_eq!(asked.proposal.state, ProposalState::Applied);
    assert_eq!(
        fx.db.progression_disposition(&run).unwrap(),
        Some(ProgressionDisposition::Asked)
    );
    let questions = fx.db.list_operator_questions(&item).unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(
        questions[0].status,
        boss_protocol::OperatorQuestionStatus::Open,
        "a blocked item can park on it"
    );
    assert_eq!(
        boss_protocol::run_done_question_criterion(&asked.proposal.payload_json),
        Some(boss_protocol::ProgressionQuestionCriterion::HumanOnlyAction)
    );
    let finished = fx.db.finish_progression_declared(&run).unwrap().unwrap();
    assert_eq!(
        finished.status,
        ExecutionStatus::Completed,
        "an asked run is not a failed run"
    );
    let parked = fx.task(&item);
    assert_eq!(parked.status, TaskStatus::Blocked);
    assert_eq!(parked.blocked_reason.as_deref(), Some("awaiting_operator_answer"));
    assert_eq!(fx.actor(&item), LAST_STATUS_ACTOR_PROGRESSION);
    assert_eq!(
        fx.db.list_operator_questions(&item).unwrap()[0].status,
        boss_protocol::OperatorQuestionStatus::Open
    );
}

#[test]
fn a_second_question_is_refused_while_one_is_open() {
    let fx = fx();
    let item = fx.chore("Already asked");
    let first = fx.run(&item, FINGERPRINT, STUCK);
    done(
        &fx,
        &first,
        json!({"outcome": "blocked", "summary": "Ask", "question": question(), "criterion": "2"}),
        "q1",
    );
    fx.db.finish_progression_declared(&first).unwrap();

    let second = fx.run(&item, "fp-2", STUCK);
    let again = done(
        &fx,
        &second,
        json!({"outcome": "blocked", "summary": "Ask", "question": question(), "criterion": "2"}),
        "q2",
    );

    assert_eq!(again.proposal.state, ProposalState::Rejected);
    assert!(
        again
            .proposal
            .decision_reason
            .unwrap()
            .contains("already has an open question")
    );
}

#[test]
fn the_owners_answer_returns_an_asked_pr_item_to_review_instead_of_failing() {
    let fx = fx();
    let item = fx.pr_item("Needs a merge", "success");
    let run = fx.run(&item, FINGERPRINT, "S16_review_churn");
    done(
        &fx,
        &run,
        json!({"outcome": "blocked", "summary": "Needs the owner", "question": question(), "criterion": "1"}),
        "ask",
    );
    fx.db.finish_progression_declared(&run).unwrap();
    assert_eq!(
        fx.task(&item).blocked_reason.as_deref(),
        Some("awaiting_operator_answer")
    );

    // A new implementation of an item that already has a PR would be refused; the
    // answer must not be rolled back by that, so the item goes back to review.
    let outcome = fx
        .db
        .answer_operator_question_with(&item, boss_protocol::OperatorAnswer::YesNo { value: true }, &fx.checker)
        .expect("the owner's answer is recorded");

    assert!(!outcome.minted_execution);
    let answered = fx.task(&item);
    assert_eq!(answered.status, TaskStatus::InReview);
    assert_eq!(answered.blocked_reason, None);
    assert!(
        answered.description.contains("Merge this PR?"),
        "the decision rides on the brief"
    );
    assert_eq!(
        fx.db.list_operator_questions(&item).unwrap()[0].status,
        boss_protocol::OperatorQuestionStatus::Answered
    );
}

fn assert_progression_status(fx: &Fx, item: &str) {
    assert_eq!(fx.actor(item), LAST_STATUS_ACTOR_PROGRESSION);
    let actor: String = fx.db.connect().unwrap().query_row(
        "SELECT actor FROM task_status_audit WHERE work_item_id = ?1 AND property = 'status' ORDER BY rowid DESC LIMIT 1",
        [item], |row| row.get(0),
    ).unwrap();
    assert_eq!(actor, LAST_STATUS_ACTOR_PROGRESSION);
}

fn declared_blocker(fx: &Fx, item: &str) -> String {
    let earlier = fx
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(item)
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let proposal = done(
        fx,
        &earlier.id,
        json!({"outcome": "blocked", "summary": "Cannot proceed: choose the retention period."}),
        "worker-blocked",
    );
    assert_eq!(proposal.proposal.state, ProposalState::Applied);
    fx.db
        .record_worker_failure_with_basis(
            &earlier.id,
            "Cannot proceed: choose the retention period.",
            crate::pr_flow::WorkerFailureBasis::DeclaredBlocked,
        )
        .unwrap()
        .unwrap();
    let attention = fx
        .db
        .create_attention_item(
            CreateAttentionItemInput::builder()
                .execution_id(&earlier.id)
                .kind("run_done_declared_blocked")
                .title("Worker declared a blocker")
                .body_markdown("Choose the retention period.")
                .build(),
        )
        .unwrap();
    attention.id
}

#[test]
fn declared_blocker_converts_to_redispatch_with_worker_text_and_addendum() {
    let fx = fx();
    let item = fx.chore("Retention policy");
    let attention = declared_blocker(&fx, &item);
    let run = fx.run(&item, FINGERPRINT, STUCK);
    let next = fx.applied(&run, ProposalKind::Redispatch,
        json!({"brief_addendum": "Worker: Cannot proceed: choose the retention period. The owner has now selected seven days."}));
    assert_progression_status(&fx, &item);
    assert!(fx.task(&item).description.contains("choose the retention period"));
    assert!(fx.task(&item).description.contains("selected seven days"));
    assert_eq!(fx.db.get_execution(&next).unwrap().status, ExecutionStatus::Ready);
    assert_eq!(fx.db.get_attention_item(&attention).unwrap().status, "resolved");
    assert_eq!(
        done(
            &fx,
            &run,
            json!({"outcome":"delivered","summary":"Restarted with the decision"}),
            "complete"
        )
        .proposal
        .state,
        ProposalState::Applied
    );
    fx.db.finish_progression_declared(&run).unwrap().unwrap();
}

#[test]
fn declared_blocker_converts_to_question_with_worker_text_and_human_decision() {
    let fx = fx();
    let item = fx.chore("Retention policy");
    let attention = declared_blocker(&fx, &item);
    let run = fx.run(&item, FINGERPRINT, STUCK);
    let asked = done(
        &fx,
        &run,
        json!({
            "outcome":"blocked", "summary":"Needs a retention decision", "criterion":"3",
            "question":{"text":"Keep records for seven days?", "answer_type":{"kind":"yes_no"},
                "explanation":"Context:\nRetention needs a decision.\nYes:\nKeep seven days.\nNo:\nKeep thirty days.\nRecommendation:\nYes.\nDetails:\nWorker: Cannot proceed: choose the retention period."}
        }),
        "ask",
    );
    assert_eq!(asked.proposal.state, ProposalState::Applied);
    assert_eq!(
        boss_protocol::run_done_question_criterion(&asked.proposal.payload_json),
        Some(boss_protocol::ProgressionQuestionCriterion::HumanDecision)
    );
    let questions = fx.db.list_operator_questions(&item).unwrap();
    let stored = serde_json::to_string(&questions[0]).unwrap();
    assert!(stored.contains("Details:"));
    assert!(stored.contains("Worker: Cannot proceed: choose the retention period."));
    fx.db.finish_progression_declared(&run).unwrap().unwrap();
    assert_eq!(
        fx.task(&item).blocked_reason.as_deref(),
        Some("awaiting_operator_answer")
    );
    assert!(!fx.task(&item).autostart);
    assert_eq!(fx.db.get_attention_item(&attention).unwrap().status, "resolved");
}

#[test]
fn question_is_refused_or_withdrawn_when_work_resumes() {
    for status in [ExecutionStatus::Ready, ExecutionStatus::Running] {
        for before_submission in [true, false] {
            let fx = fx();
            let item = fx.chore("Restart race");
            let run = fx.run(&item, FINGERPRINT, STUCK);
            let restart = || {
                fx.db
                    .create_execution(
                        CreateExecutionInput::builder()
                            .work_item_id(&item)
                            .kind(ExecutionKind::TaskImplementation)
                            .status(status.clone())
                            .build(),
                    )
                    .unwrap()
            };
            if before_submission {
                restart();
            }
            let asked = done(
                &fx,
                &run,
                json!({"outcome":"blocked","summary":"Ask","question":question(),"criterion":"3"}),
                "ask",
            );
            if before_submission {
                assert_eq!(asked.proposal.state, ProposalState::Rejected);
            } else {
                assert_eq!(asked.proposal.state, ProposalState::Applied);
                restart();
                let before = fx.task(&item);
                fx.db.finish_progression_declared(&run).unwrap().unwrap();
                let after = fx.task(&item);
                assert_eq!(before.status, after.status);
                assert_eq!(before.autostart, after.autostart);
                assert_eq!(
                    fx.db.list_operator_questions(&item).unwrap()[0].status,
                    boss_protocol::OperatorQuestionStatus::Withdrawn
                );
                assert_eq!(
                    fx.db.progression_disposition(&run).unwrap(),
                    Some(ProgressionDisposition::Withdrawn)
                );
            }
        }
    }
}

#[test]
fn changing_the_dependency_subject_does_not_bypass_the_fingerprint_rule() {
    let fx = fx();
    let item = fx.pr_item("Parent", "fail");
    let first = fx.revision_of(&item, "First", "blocked");
    let second = fx.revision_of(&item, "Second", "blocked");
    let run = fx.run(&item, FINGERPRINT, STUCK);
    fx.applied(&run, ProposalKind::Depend, json!({"on": first}));
    let refusal = fx.refused(&run, ProposalKind::Depend, json!({"on": second}));
    assert!(
        refusal.contains(&run) && refusal.contains("already applied"),
        "{refusal}"
    );
}

#[test]
fn refused_verbs_consume_the_execution_rate_cap() {
    let fx = fx();
    let item = fx.chore("No PR");
    let run = fx.run(&item, FINGERPRINT, STUCK);
    let mut capped = false;
    for _ in 0..100 {
        let result = fx
            .submit(&run, ProposalKind::ClearSignal, json!({"reason":"ci_failure"}))
            .unwrap_err();
        if result.code != ProposalErrorCode::NotAccepted {
            capped = true;
            break;
        }
    }
    assert!(capped, "refused commands must eventually hit the proposal rate cap");
    assert!(fx.actions(&item).is_empty());
}

#[test]
fn clear_signal_retires_attempts_but_refuses_unresolved_queue_failures() {
    for queue in [false, true] {
        let fx = fx();
        let item = fx.pr_item("CI failure", "success");
        fx.sql(&format!(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure' WHERE id = '{item}'"
        ));
        let revision = fx.revision_of(&item, "Repair", "todo");
        fx.sql(&format!(
            "UPDATE tasks SET created_via = 'ci-fix:attempt' WHERE id = '{revision}'"
        ));
        let attempt = fx
            .db
            .insert_ci_remediation(crate::CiRemediationInsertInput {
                product_id: fx.product.clone(),
                work_item_id: item.clone(),
                pr_url: fx.task(&item).pr_url.unwrap(),
                pr_number: 100,
                head_branch: "feature".into(),
                head_sha_at_trigger: "abc".into(),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: "[]".into(),
                failure_kind: if queue { "merge_queue_rebounce" } else { "pr_branch_ci" }.into(),
                before_commit_sha: None,
            })
            .unwrap()
            .unwrap();
        fx.sql(&format!(
            "UPDATE ci_remediations SET revision_task_id = '{revision}' WHERE id = '{}'",
            attempt.id
        ));
        let run = fx.run(&item, FINGERPRINT, "S6_ci_failure");
        if queue {
            let error = fx.refused(&run, ProposalKind::ClearSignal, json!({"reason":"ci_failure"}));
            assert!(error.contains("queue-side"), "{error}");
            assert_eq!(fx.task(&item).blocked_reason.as_deref(), Some("ci_failure"));
            assert!(fx.db.active_ci_remediation_for_work_item(&item).unwrap().is_some());
        } else {
            fx.applied(&run, ProposalKind::ClearSignal, json!({"reason":"ci_failure"}));
            assert!(fx.db.active_ci_remediation_for_work_item(&item).unwrap().is_none());
            assert_eq!(
                query_ci_remediation(&fx.db.connect().unwrap(), &attempt.id)
                    .unwrap()
                    .unwrap()
                    .status,
                "succeeded"
            );
            assert_eq!(fx.task(&revision).status, TaskStatus::Archived);
            assert_eq!(fx.actor(&revision), LAST_STATUS_ACTOR_PROGRESSION);
            fx.sql(&format!(
                "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure' WHERE id = '{item}'"
            ));
            let error = fx.refused(
                &run,
                ProposalKind::ClearSignal,
                json!({"reason":"ci_failure_exhausted"}),
            );
            assert!(error.contains(&run) && error.contains("already applied"), "{error}");
        }
    }
}
