//! Dependency health, root targeting, and escalation through the shared candidate views.
use super::*;

#[test]
fn revisions_repair_stale_blocks_after_same_chain_prerequisites_release_the_branch() {
    for status in ["in_review", "blocked"] {
        let f = fixture();
        let root = f.chore("PR root");
        let tail = f.revision_of(&root, "Previous revision");
        // Use a revision parent, rather than siblings, to exercise chain roots.
        let next = f.revision_of(&tail, "Next revision");
        f.sql(&format!(
            "UPDATE tasks SET status = 'in_review' WHERE id = '{root}';
             UPDATE tasks SET status = '{status}', blocked_reason = 'worker_failed' WHERE id = '{tail}';
             INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
             VALUES ('{next}', '{tail}', 'blocks', '1')"
        ));
        if status == "blocked" {
            f.execution(&tail, ExecutionKind::RevisionImplementation, ExecutionStatus::Failed);
        }
        f.stick(&next, "status = 'blocked', blocked_reason = 'dependency'", 3);
        assert!(f.db.gating_prereqs_for(&next).unwrap().is_empty());
        assert!(f.clauses(&next).dependency_clear);
        assert_eq!(f.clauses(&next).stuck_kind.as_deref(), Some("S4_dependency"));
        f.assert_candidate(&next, true);
        assert_ne!(bucket(&f.db, &next), Some(StuckBucket::WaitingOnDependency));

        // A failed task with a pending retry still owns the branch.
        if status == "blocked" {
            f.execution(&tail, ExecutionKind::RevisionImplementation, ExecutionStatus::Ready);
            assert_eq!(f.db.gating_prereqs_for(&next).unwrap(), vec![tail]);
            assert!(!f.clauses(&next).dependency_clear);
        }
    }
}

#[test]
fn unrelated_review_prerequisites_still_hold_revisions() {
    let f = fixture();
    let root = f.chore("PR root");
    let unrelated = f.chore("Unrelated PR");
    let next = f.dependent_on(&unrelated);
    f.sql(&format!(
        "UPDATE tasks SET kind = 'revision', parent_task_id = '{root}' WHERE id = '{next}';
         UPDATE tasks SET status = 'in_review' WHERE id IN ('{root}', '{unrelated}')"
    ));
    assert_eq!(f.db.gating_prereqs_for(&next).unwrap(), vec![unrelated]);
    assert!(!f.clauses(&next).dependency_clear);
    f.assert_candidate(&next, false);
    assert_eq!(bucket(&f.db, &next), Some(StuckBucket::WaitingOnDependency));
}

#[test]
fn conflict_dependency_health_matches_writer_exclusion_including_recovery() {
    for recovery in [false, true] {
        let f = fixture();
        let root = f.chore("PR root");
        let older = f.revision_of(&root, "Older writer");
        let conflict = f.revision_of(&root, "Conflict fix");
        assert!(older < conflict, "fixture follows revision ordering");
        let dependent = if recovery {
            f.revision_of(&root, "Conflict recovery")
        } else {
            conflict.clone()
        };
        f.sql(&format!(
            "UPDATE tasks SET status = 'in_review' WHERE id = '{root}';
             UPDATE tasks SET created_via = 'merge-conflict:test' WHERE id = '{conflict}';
             INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
             VALUES ('{dependent}', '{older}', 'blocks', '1')"
        ));
        if recovery {
            f.sql(&format!(
                "UPDATE tasks SET created_via = 'attention_conflict_recovery:{conflict}',
                 status = 'blocked', blocked_reason = 'worker_failed' WHERE id = '{dependent}';
                 UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed' WHERE id = '{conflict}'"
            ));
        }
        f.stick(&dependent, "status = 'blocked', blocked_reason = 'dependency'", 3);
        // Even an active task stops holding a conflict fix once no writer is live.
        f.sql(&format!("UPDATE tasks SET status = 'active' WHERE id = '{older}'"));
        assert!(f.db.gating_prereqs_for(&dependent).unwrap().is_empty());
        assert!(f.clauses(&dependent).dependency_clear);
        f.assert_candidate(&dependent, true);

        let run = f.execution(&older, ExecutionKind::RevisionImplementation, ExecutionStatus::Running);
        assert_eq!(f.db.gating_prereqs_for(&dependent).unwrap(), vec![older.clone()]);
        assert!(!f.clauses(&dependent).dependency_clear);
        f.sql(&format!(
            "UPDATE work_executions SET status = 'failed' WHERE id = '{run}'"
        ));
        f.execution(&older, ExecutionKind::PrReview, ExecutionStatus::Running);
        assert!(f.db.gating_prereqs_for(&dependent).unwrap().is_empty());
        assert!(f.clauses(&dependent).dependency_clear);
    }
}

#[test]
fn older_conflict_fixes_wait_only_while_newer_fixes_are_in_flight() {
    let f = fixture();
    let root = f.chore("PR root");
    let older = f.revision_of(&root, "Older conflict fix");
    let newer = f.revision_of(&root, "Newer conflict fix");
    assert!(older < newer, "fixture follows revision ordering");
    f.sql(&format!(
        "UPDATE tasks SET created_via = 'merge-conflict:test' WHERE id IN ('{older}', '{newer}');
         INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
         VALUES ('{older}', '{newer}', 'blocks', '1')"
    ));
    f.stick(&older, "status = 'blocked', blocked_reason = 'dependency'", 3);
    assert_eq!(f.db.gating_prereqs_for(&older).unwrap(), vec![newer.clone()]);
    assert!(!f.clauses(&older).dependency_clear);
    f.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed' WHERE id = '{newer}'"
    ));
    assert!(f.db.gating_prereqs_for(&older).unwrap().is_empty());
    assert!(f.clauses(&older).dependency_clear);
    f.assert_candidate(&older, true);
}

#[test]
fn a_healthy_chain_waits_indefinitely_with_manual_or_automatic_work() {
    let f = fixture();
    let root = f.chore("Backfill");
    let second = f.dependent_on(&root);
    let third = f.dependent_on(&second);
    let fourth = f.dependent_on(&third);
    for item in [&second, &third, &fourth] {
        f.age(item, 365 * 24 * HOUR);
        f.sql(&format!("UPDATE tasks SET kind = 'project_task' WHERE id = '{item}'"));
    }
    for autostart in [0, 1] {
        for status in ["todo", "active", "in_review"] {
            f.sql(&format!(
                "UPDATE tasks SET autostart = {autostart};
                 UPDATE tasks SET status = '{status}' WHERE id = '{root}'"
            ));
            for item in [&root, &second, &third, &fourth] {
                f.assert_candidate(item, false);
            }
            assert_eq!(bucket(&f.db, &fourth), Some(StuckBucket::WaitingOnDependency));
        }
    }
    for status in [ExecutionStatus::Ready, ExecutionStatus::Running] {
        let run = f.execution(&root, ExecutionKind::ChoreImplementation, status);
        f.assert_candidate(&fourth, false);
        f.sql(&format!(
            "UPDATE work_executions SET status = 'completed' WHERE id = '{run}'"
        ));
    }
    assert!(f.db.mint_progression_execution(&fourth).unwrap().is_none());
    let mint = f.db.mint_progression_executions().unwrap();
    assert!(mint.minted.is_empty());
    assert!(mint.asked.is_empty());
    assert!(mint.failed.is_empty());
    assert!(mint.ladder_failed.is_empty());
}

#[test]
fn broken_or_satisfied_edges_are_repaired_at_the_direct_dependent() {
    for change in ["status = 'archived'", "deleted_at = '1'", "status = 'done'"] {
        let f = fixture();
        let root = f.chore("Prerequisite");
        let middle = f.dependent_on(&root);
        let downstream = f.dependent_on(&middle);
        f.sql(&format!("UPDATE tasks SET {change} WHERE id = '{root}'"));
        f.assert_candidate(&middle, true);
        f.assert_candidate(&downstream, false);
        assert_eq!(f.clauses(&middle).stuck_kind.as_deref(), Some("S4_dependency"));
        let mint = f.db.mint_progression_executions().unwrap();
        assert!(mint.failed.is_empty());
        assert_eq!(mint.minted.len(), 1);
        assert_eq!(mint.minted[0].0.work_item_id, middle);
    }
    let f = fixture();
    let missing = f.dependent_on("task_missing");
    f.assert_candidate(&missing, true);
    f.sql("DELETE FROM work_item_dependencies");
    f.assert_candidate(&missing, true);
}

#[test]
fn completed_edges_do_not_hide_unfinished_work_but_broken_edges_need_repair() {
    let f = fixture();
    let finished = f.chore("Finished prerequisite");
    let unfinished = f.chore("Unfinished prerequisite");
    let dependent = f.dependent_on(&finished);
    f.sql(&format!(
        "UPDATE tasks SET status = 'done' WHERE id = '{finished}';
         INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
         VALUES ('{dependent}', '{unfinished}', 'blocks', '1')"
    ));
    f.assert_candidate(&dependent, false);
    f.sql(&format!("UPDATE tasks SET status = 'archived' WHERE id = '{finished}'"));
    f.assert_candidate(&dependent, true);
}

#[test]
fn transitive_cycles_target_cycle_members_even_with_a_healthy_branch() {
    let f = fixture();
    let first = f.chore("Cycle start");
    let second = f.dependent_on(&first);
    let third = f.dependent_on(&second);
    let downstream = f.dependent_on(&third);
    let healthy = f.chore("Healthy branch");
    // Public dependency writes reject cycles. Plant a corrupt graph to exercise
    // diagnosis of legacy or externally damaged edges, including a live branch.
    f.sql(&format!(
        "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
         VALUES ('{first}', '{third}', 'blocks', '1'), ('{second}', '{healthy}', 'blocks', '1')"
    ));
    f.stick(&first, "status = 'blocked', blocked_reason = 'dependency'", 3);
    for item in [&first, &second, &third] {
        f.assert_candidate(item, true);
    }
    f.assert_candidate(&downstream, false);
    // A non-blocking ordering edge must not close the cycle for downstream.
    f.sql(&format!(
        "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
         VALUES ('{first}', '{downstream}', 'merge_order', '1')"
    ));
    f.assert_candidate(&downstream, false);
    // Direct self edges cannot exist: the schema enforces this independently
    // of the API. Self-dependencies through project membership are tested below.
    let error =
        f.db.connect()
            .unwrap()
            .execute(
                "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
         VALUES (?1, ?1, 'blocks', '1')",
                [&first],
            )
            .unwrap_err();
    assert!(error.to_string().contains("dependent_id <> prerequisite_id"));
}

#[test]
fn failed_or_orphaned_root_work_is_targeted_through_its_own_stuck_state() {
    for status in [ExecutionStatus::Failed, ExecutionStatus::Orphaned] {
        let f = fixture();
        let root = f.chore("Failed root");
        f.execution(&root, ExecutionKind::ChoreImplementation, status);
        f.stick(&root, "status = 'blocked', blocked_reason = 'worker_failed'", 3);
        let middle = f.dependent_on(&root);
        let downstream = f.dependent_on(&middle);
        f.assert_candidate(&root, true);
        f.assert_candidate(&middle, false);
        f.assert_candidate(&downstream, false);
        let mint = f.db.mint_progression_executions().unwrap();
        assert!(mint.failed.is_empty());
        assert_eq!(mint.minted.len(), 1);
        assert_eq!(mint.minted[0].0.work_item_id, root);
        assert_eq!(mint.minted[0].0.stuck_kind, "S7_worker_failed");
    }
}

#[test]
fn past_progression_failures_do_not_ask_to_park_a_healthy_waiter() {
    let f = fixture();
    let root = f.chore("Manual work");
    let second = f.dependent_on(&root);
    let third = f.dependent_on(&second);
    let fourth = f.dependent_on(&third);
    finished_progression_run(&f, &fourth, ExecutionStatus::Failed, 30 * HOUR);
    finished_progression_run(&f, &fourth, ExecutionStatus::Orphaned, 20 * HOUR);
    assert!(f.clauses(&fourth).infrastructure_failing);
    assert!(!f.clauses(&fourth).dependency_clear);
    let mint = f.db.mint_progression_executions().unwrap();
    assert!(mint.minted.is_empty());
    assert!(mint.asked.is_empty());
    assert!(mint.failed.is_empty());
    assert!(mint.ladder_failed.is_empty());
    let questions: i64 =
        f.db.connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM operator_questions", [], |row| row.get(0))
            .unwrap();
    assert_eq!(questions, 0);
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

    // Idle unfinished work holds its dependent, with or without autostart.
    f.sql(&format!("UPDATE tasks SET autostart = 1 WHERE id = '{prerequisite}'"));
    f.assert_candidate(&dependent, false);
    assert!(!f.clauses(&dependent).dependency_clear);

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

    // A prerequisite waiting on a human remains the owner's to unblock.
    f.sql(&format!(
        "UPDATE tasks SET human_driven = 1 WHERE id = '{prerequisite}'"
    ));
    f.assert_candidate(&dependent, false);
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
fn clause_4_an_open_project_with_unfinished_members_holds_its_dependent() {
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

    // Idle, non-stuck members are unfinished work too.
    f.assert_candidate(&dependent, false);

    // A running member task makes the project live.
    let run = f.execution(&member, ExecutionKind::ChoreImplementation, ExecutionStatus::Running);
    f.assert_candidate(&dependent, false);
    assert!(!f.clauses(&dependent).dependency_clear);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'failed' WHERE id = '{run}'"
    ));
    f.assert_candidate(&dependent, false);

    // A member that is itself a candidate is progressed first.
    f.stick(&member, "status = 'blocked', blocked_reason = 'merge_conflict'", 3);
    f.assert_candidate(&member, true);
    f.assert_candidate(&dependent, false);
    // A member in cooldown still owns its recovery.
    finished_progression_run(&f, &member, ExecutionStatus::Completed, 60);
    f.assert_candidate(&member, false);
    f.assert_candidate(&dependent, false);

    // A parked project is the owner's decision.
    f.db.park_work_item_as_actor(&project.id, true, None, "human").unwrap();
    f.assert_candidate(&dependent, false);
}

#[test]
fn clause_4_root_recovery_limits_do_not_redirect_progression_to_dependents() {
    let f = fixture();
    let prerequisite = f.stuck_chore("Stuck prerequisite");
    let dependent = f.dependent_on(&prerequisite);
    f.assert_candidate(&prerequisite, true);
    f.assert_candidate(&dependent, false);

    // Inside its cooldown.
    let run = finished_progression_run(&f, &prerequisite, ExecutionStatus::Completed, 60);
    f.assert_candidate(&prerequisite, false);
    f.assert_candidate(&dependent, false);
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
    f.assert_candidate(&dependent, false);
    f.sql("UPDATE tasks SET created_at = '1' WHERE created_via LIKE 'progression:%'");
    f.assert_candidate(&prerequisite, true);
    f.assert_candidate(&dependent, false);

    // At the infrastructure-failure cap.
    finished_progression_run(&f, &prerequisite, ExecutionStatus::Failed, 30 * HOUR);
    finished_progression_run(&f, &prerequisite, ExecutionStatus::Orphaned, 20 * HOUR);
    assert!(f.clauses(&prerequisite).infrastructure_failing);
    f.assert_candidate(&prerequisite, false);
    f.assert_candidate(&dependent, false);

    // A fresh failure waits for its own grace; its dependent is still waiting.
    let fresh = f.chore("Fresh prerequisite");
    f.stick(&fresh, "status = 'blocked', blocked_reason = 'merge_conflict'", 0);
    let waiting = f.dependent_on(&fresh);
    f.assert_candidate(&fresh, false);
    f.assert_candidate(&waiting, false);
}

#[test]
fn the_stuck_report_reads_the_same_dependency_rule_as_the_view() {
    let f = fixture();
    // An idle prerequisite keeps the dependent in the waiting bucket.
    let idle = f.chore("Idle prerequisite");
    let dependent = f.dependent_on(&idle);
    f.assert_candidate(&dependent, false);
    assert_eq!(bucket(&f.db, &dependent), Some(StuckBucket::WaitingOnDependency));
    f.db.mint_progression_executions().unwrap();
    assert_eq!(bucket(&f.db, &dependent), Some(StuckBucket::WaitingOnDependency));

    // An empty, unparked project has no unfinished member to progress.
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
fn clause_4_the_stuck_root_is_picked_and_all_downstream_items_wait() {
    let f = fixture();
    let upstream = f.stuck_chore("Upstream");
    let middle = f.dependent_on(&upstream);
    let downstream = f.dependent_on(&middle);
    f.assert_candidate(&upstream, true);
    f.assert_candidate(&middle, false);
    // Waiting remains transitive; middle's own candidacy is irrelevant.
    assert!(f.clauses(&middle).eligible_item);
    assert!(f.clauses(&middle).stuck_long_enough);
    assert!(f.clauses(&middle).cooldown_and_budget_clear);
    f.assert_candidate(&downstream, false);
    assert!(!f.clauses(&downstream).dependency_clear);

    // The same chain through a project member also keeps waiting.
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
    f.assert_candidate(&gated_on_project, false);
    let mint = f.db.mint_progression_executions().unwrap();
    assert!(mint.failed.is_empty());
    assert_eq!(mint.minted.len(), 1);
    assert_eq!(mint.minted[0].0.work_item_id, upstream);
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
fn clause_4_a_same_family_finished_prerequisite_does_not_hide_the_dependent_run() {
    let f = fixture();
    let prerequisite = f.chore("Finished same-family prerequisite");
    f.sql(&format!(
        "UPDATE tasks SET status = 'done', pr_url = 'https://github.com/o/r/pull/9' WHERE id = '{prerequisite}'"
    ));
    let dependent = f.dependent_on(&prerequisite);
    f.sql(&format!(
        "UPDATE tasks SET pr_url = 'https://github.com/o/r/pull/9' WHERE id = '{dependent}'"
    ));
    f.assert_candidate(&dependent, true);
    f.db.mint_progression_executions().unwrap();
    assert_eq!(bucket(&f.db, &dependent), Some(StuckBucket::ProgressionInFlight));
}

#[test]
fn revision_waiting_on_its_own_pr_is_a_named_cycle_even_with_archived_prerequisites() {
    let f = fixture();
    let root = f.chore("Open PR root");
    let revision = f.revision_of(&root, "Review revision");
    let prerequisite = f.chore("Split-out work");
    let archived = f.chore("Archived prerequisite");
    f.sql(&format!(
        "UPDATE tasks SET status = 'in_review' WHERE id = '{root}';
         UPDATE tasks SET status = 'archived' WHERE id = '{archived}';
         INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
         VALUES ('{revision}', '{prerequisite}', 'blocks', '1'),
                ('{prerequisite}', '{root}', 'blocks', '1'),
                ('{revision}', '{archived}', 'blocks', '1')"
    ));
    for id in [&revision, &prerequisite] {
        f.stick(id, "status = 'blocked', blocked_reason = 'dependency'", 3);
    }
    f.execution(
        &revision,
        ExecutionKind::RevisionImplementation,
        ExecutionStatus::WaitingDependency,
    );
    let later = f.revision_of(&root, "Later revision queued on the same PR");
    f.sql(&format!(
        "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
         VALUES ('{later}', '{revision}', 'blocks', '1')"
    ));
    f.execution(
        &later,
        ExecutionKind::RevisionImplementation,
        ExecutionStatus::WaitingDependency,
    );
    let report = f.db.stuck_report(0, now(), &observed_live_pid).unwrap();
    for id in [&revision, &prerequisite] {
        assert_eq!(
            report
                .rows
                .iter()
                .find(|row| &row.work_item_id == id)
                .unwrap()
                .classification,
            "S4_dependency_cycle"
        );
        assert_eq!(f.clauses(id).stuck_kind.as_deref(), Some("S4_dependency_cycle"));
        f.assert_candidate(id, true);
        assert_eq!(bucket(&f.db, id), Some(StuckBucket::EngineStuck));
    }
    assert!(f.db.gating_prereqs_for(&revision).unwrap().contains(&prerequisite));
    assert!(f.db.gating_prereqs_for(&root).unwrap().contains(&prerequisite));
    // Archiving a member cannot silently satisfy a structural cycle.
    f.sql(&format!(
        "UPDATE tasks SET status = 'archived' WHERE id = '{prerequisite}'"
    ));
    assert!(f.db.gating_prereqs_for(&revision).unwrap().contains(&prerequisite));
    assert_eq!(f.clauses(&revision).stuck_kind.as_deref(), Some("S4_dependency_cycle"));
    f.execution(&revision, ExecutionKind::Progression, ExecutionStatus::Ready);
    assert_eq!(bucket(&f.db, &revision), Some(StuckBucket::ProgressionInFlight));
    f.assert_candidate(&revision, false);
    // Diagnosis must not repair the graph by dropping edges or archiving work.
    assert_eq!(
        query_task(&f.db.connect().unwrap(), &revision).unwrap().unwrap().status,
        TaskStatus::Blocked
    );
}

#[test]
fn graph_repairs_clear_cycle_membership_without_reopening() {
    for convert in [false, true] {
        let f = fixture();
        let root = f.chore("PR root");
        let revision = f.revision_of(&root, "Review findings");
        let prerequisite = f.chore("Split-out prerequisite");
        // The root's edge closes a loop through the revision-parent edge.
        f.sql(&format!(
            "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
             VALUES ('{root}', '{prerequisite}', 'blocks', '1'),
                    ('{prerequisite}', '{revision}', 'blocks', '1')"
        ));
        f.stick(&revision, "status = 'blocked', blocked_reason = 'dependency'", 3);
        assert_eq!(f.clauses(&revision).stuck_kind.as_deref(), Some("S4_dependency_cycle"));
        assert!(f.db.gating_prereqs_for(&revision).unwrap().contains(&root));
        if convert {
            let mut conn = f.db.connect().unwrap();
            let tx = conn.transaction().unwrap();
            let rev = query_task(&tx, &revision).unwrap().unwrap();
            let (changed, _) = crate::review_findings_followup::convert_revision_to_review_findings_followup(
                &tx,
                &rev,
                &root,
                &now_string(),
                true,
            )
            .unwrap();
            assert_eq!(changed, 1);
            tx.commit().unwrap();
        } else {
            f.db.delete_work_item(&prerequisite).unwrap();
        }
        let conn = f.db.connect().unwrap();
        assert!(!crate::work_dependencies::cycle::any_member(&conn, &[&root, &revision, &prerequisite]).unwrap());
        drop(conn);
        assert!(f.db.gating_prereqs_for(&revision).unwrap().is_empty());
        assert_ne!(f.clauses(&revision).stuck_kind.as_deref(), Some("S4_dependency_cycle"));
        let report = f.db.stuck_report(0, now(), &observed_live_pid).unwrap();
        assert!(
            report
                .rows
                .iter()
                .all(|row| row.classification != "S4_dependency_cycle")
        );
    }
}
