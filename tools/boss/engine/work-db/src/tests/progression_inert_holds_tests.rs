//! Idle gates must not hide a stuck family; live writers must still exclude it.
use super::*;

fn dependency(f: &Fixture, dependent: &str, prerequisite: &str) {
    f.sql(&format!(
        "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
         VALUES ('{dependent}', '{prerequisite}', 'blocks', '1')"
    ));
}

fn owner_settings(f: &Fixture, id: &str) -> (Option<String>, bool) {
    f.db.connect()
        .unwrap()
        .query_row("SELECT parked_at, autostart FROM tasks WHERE id = ?1", [id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
}

fn link_attempt(f: &Fixture, parent: &str, revision: &str, conflict: bool) {
    let (table, attempt) = if conflict {
        ("conflict_resolutions", f.insert_conflict_attempt(parent, "base"))
    } else {
        ("ci_remediations", f.insert_ci_attempt(parent, "head"))
    };
    f.sql(&format!(
        "UPDATE {table} SET revision_task_id = '{revision}' WHERE id = '{attempt}'"
    ));
}

#[test]
fn failed_revision_is_not_excluded_by_its_own_pending_ladder_attempt() {
    for conflict in [false, true] {
        let f = fixture();
        let parent = f.chore("Parent PR");
        let revision = f.revision_of(&parent, "Failed repair");
        link_attempt(&f, &parent, &revision, conflict);
        f.stick(&revision, "status = 'blocked', blocked_reason = 'worker_failed'", 3);
        f.execution(
            &revision,
            ExecutionKind::RevisionImplementation,
            ExecutionStatus::Failed,
        );
        f.assert_candidate(&revision, true);
        assert!(f.clauses(&revision).no_inflight_ladder);

        // The explicit self exception never overrides the separate live-work gate.
        for status in [ExecutionStatus::Queued, ExecutionStatus::Running] {
            let run = f.execution(&revision, ExecutionKind::RevisionImplementation, status);
            assert!(f.clauses(&revision).no_inflight_ladder);
            assert!(!f.clauses(&revision).no_live_work);
            f.assert_candidate(&revision, false);
            f.sql(&format!(
                "UPDATE work_executions SET status = 'failed' WHERE id = '{run}'"
            ));
        }
        assert!(f.db.mint_progression_execution(&revision).unwrap().is_some());
    }
}

#[test]
fn waiting_revision_behind_parked_disabled_or_cancelled_work_does_not_hide_parent() {
    for hold in ["parked", "autostart off", "cancelled"] {
        let f = fixture();
        let parent = f.stuck_chore("Conflicted parent");
        let revision = f.revision_of(&parent, "Waiting repair");
        link_attempt(&f, &parent, &revision, true);
        let prerequisite = f.chore("Idle prerequisite");
        dependency(&f, &revision, &prerequisite);
        f.stick(&revision, "status = 'blocked', blocked_reason = 'dependency'", 3);
        f.execution(
            &revision,
            ExecutionKind::RevisionImplementation,
            ExecutionStatus::WaitingDependency,
        );
        match hold {
            "parked" => {
                f.db.park_work_item_as_actor(&prerequisite, true, None, "human")
                    .unwrap();
            }
            "autostart off" => f.sql(&format!("UPDATE tasks SET autostart = 0 WHERE id = '{prerequisite}'")),
            _ => {
                f.sql(&format!("UPDATE tasks SET autostart = 1 WHERE id = '{prerequisite}'"));
                f.execution(
                    &prerequisite,
                    ExecutionKind::ChoreImplementation,
                    ExecutionStatus::Cancelled,
                );
            }
        }
        f.assert_candidate(&parent, true);
        f.assert_candidate(&revision, false);
        assert!(
            !f.clauses(&revision).dependency_clear,
            "dependency remains owner-controlled"
        );
        let before = owner_settings(&f, &prerequisite);
        assert!(f.db.mint_progression_execution(&parent).unwrap().is_some());
        assert_eq!(before, owner_settings(&f, &prerequisite));
    }
}

#[test]
fn cancelled_autostart_off_ladder_revision_does_not_hide_conflicted_parent() {
    let f = fixture();
    let parent = f.stuck_chore("Conflicted parent");
    let revision = f.revision_of(&parent, "Disabled repair");
    link_attempt(&f, &parent, &revision, true);
    f.sql(&format!(
        "UPDATE tasks SET status = 'todo', autostart = 0 WHERE id = '{revision}'"
    ));
    f.execution(
        &revision,
        ExecutionKind::RevisionImplementation,
        ExecutionStatus::Cancelled,
    );
    f.assert_candidate(&parent, true);
    f.assert_candidate(&revision, false);
    assert!(f.clauses(&parent).no_inflight_ladder);
    let live = f.execution(&revision, ExecutionKind::RevisionImplementation, ExecutionStatus::Ready);
    f.assert_candidate(&parent, false);
    assert!(!f.clauses(&parent).no_inflight_ladder);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'cancelled' WHERE id = '{live}'"
    ));
    assert!(f.db.mint_progression_execution(&parent).unwrap().is_some());
    assert!(!owner_settings(&f, &revision).1);
}

#[test]
fn post_merge_followup_is_visited_while_its_revision_waits_on_idle_review_prerequisites() {
    let f = fixture();
    let parent = f.stuck_chore("Post-merge findings");
    f.sql(&format!("UPDATE tasks SET kind = 'followup' WHERE id = '{parent}'"));
    let revision = f.revision_of(&parent, "Waiting review fixes");
    f.stick(&revision, "status = 'blocked', blocked_reason = 'dependency'", 3);
    f.execution(
        &revision,
        ExecutionKind::RevisionImplementation,
        ExecutionStatus::WaitingDependency,
    );
    let mut prerequisites = Vec::new();
    for i in 0..8 {
        let prerequisite = f.chore(&format!("PR prerequisite {i}"));
        f.sql(&format!(
            "UPDATE tasks SET status = 'in_review' WHERE id = '{prerequisite}'"
        ));
        dependency(&f, &revision, &prerequisite);
        prerequisites.push(prerequisite);
    }
    f.assert_candidate(&parent, true);
    f.assert_candidate(&revision, false);
    assert!(!f.clauses(&revision).dependency_clear);
    // Real work on even one prerequisite keeps the wait progressing.
    let run = f.execution(&prerequisites[0], ExecutionKind::PrReview, ExecutionStatus::Running);
    f.assert_candidate(&parent, false);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'completed' WHERE id = '{run}'"
    ));
    let minted = f.db.mint_progression_executions().unwrap();
    assert_eq!(minted.minted.len(), 1);
    assert_eq!(minted.minted[0].0.work_item_id, parent);
}

#[test]
fn dependency_liveness_is_transitive_across_pr_families_and_idle_cycles_terminate() {
    let f = fixture();
    let parent = f.stuck_chore("Stuck parent");
    let revision = f.revision_of(&parent, "Waiting revision");
    let middle = f.chore("Middle prerequisite");
    let leaf = f.chore("Leaf prerequisite");
    let writer = f.revision_of(&leaf, "Leaf writer");
    dependency(&f, &revision, &middle);
    dependency(&f, &middle, &leaf);
    for waiting in [&revision, &middle] {
        f.execution(
            waiting,
            ExecutionKind::RevisionImplementation,
            ExecutionStatus::WaitingDependency,
        );
    }
    f.assert_candidate(&parent, true);
    let run = f.execution(&writer, ExecutionKind::RevisionImplementation, ExecutionStatus::Running);
    f.assert_candidate(&parent, false);
    f.sql(&format!(
        "UPDATE work_executions SET status = 'completed' WHERE id = '{run}'"
    ));
    dependency(&f, &leaf, &middle);
    f.execution(
        &leaf,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::WaitingDependency,
    );
    f.assert_candidate(&parent, true);
    // A worker on another item bound to the parent's PR still excludes it.
    let sharing = f.chore("Shared PR writer");
    f.sql(&format!(
        "UPDATE tasks SET pr_url = 'https://github.com/o/r/pull/17' WHERE id IN ('{parent}', '{sharing}')"
    ));
    f.execution(&sharing, ExecutionKind::ChoreImplementation, ExecutionStatus::Running);
    f.assert_candidate(&parent, false);
}
