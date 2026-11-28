use super::*;
use crate::test_support::{create_product, create_ready_chore_execution, create_test_chore, open_db};

const PR: &str = "https://github.com/org/repo/pull/1";

struct Row {
    opened_at: String,
    first_driver: Option<String>,
    baseline_head: Option<String>,
    first_ci: Option<String>,
    merged_at: Option<String>,
}

fn ledger(db: &WorkDb, root: &str) -> Option<Row> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT opened_at, first_driver, baseline_head, first_ci, merged_at
             FROM pr_family_outcomes WHERE work_item_id = ?1",
            [root],
            |row| {
                Ok(Row {
                    opened_at: row.get(0)?,
                    first_driver: row.get(1)?,
                    baseline_head: row.get(2)?,
                    first_ci: row.get(3)?,
                    merged_at: row.get(4)?,
                })
            },
        )
        .optional()
        .unwrap()
}

fn first_head(db: &WorkDb, root: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT first_head FROM pr_family_outcomes WHERE work_item_id = ?1",
            [root],
            |row| row.get(0),
        )
        .unwrap()
}

fn ledger_rows(db: &WorkDb) -> i64 {
    db.connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM pr_family_outcomes", [], |row| row.get(0))
        .unwrap()
}

fn sql(db: &WorkDb, statement: &str, params: impl rusqlite::Params) {
    db.connect().unwrap().execute(statement, params).unwrap();
}

fn launch(db: &WorkDb, execution: &WorkExecution, driver: &str, arms: Option<&str>) {
    db.record_execution_launch(&ExecutionLaunchRecord {
        execution_id: &execution.id,
        driver,
        model: "model",
        effort_level: None,
        experiment_arms: arms,
    })
    .unwrap();
}

fn make_revision(db: &WorkDb, product: &str, root: &str, name: &str) -> Task {
    let revision = create_test_chore(db, product, format!("{name} of {root}"));
    sql(
        db,
        "UPDATE tasks SET kind='revision', parent_task_id=?2 WHERE id=?1",
        params![revision.id, root],
    );
    revision
}

fn publish_pr(db: &WorkDb, root: &str) {
    sql(db, "UPDATE tasks SET pr_url=?2 WHERE id=?1", params![root, PR]);
}

fn set_times(db: &WorkDb, execution: &str, start: i64) {
    sql(
        db,
        "UPDATE work_executions SET created_at=?2, started_at=?2 WHERE id=?1",
        params![execution, start.to_string()],
    );
}

/// A run of `execution` created at `at`, with `tokens` input tokens (or none).
fn add_run(db: &WorkDb, execution: &WorkExecution, at: i64, tokens: Option<i64>) {
    let run = db
        .create_run(
            boss_protocol::CreateRunInput::builder()
                .execution_id(&execution.id)
                .agent_id("worker")
                .status("completed")
                .build(),
        )
        .unwrap();
    sql(
        db,
        "UPDATE work_runs SET model='model', input_tokens=?2, output_tokens=?3, created_at=?4 WHERE id=?1",
        params![run.id, tokens, tokens.map(|_| 0), at.to_string()],
    );
}

fn merge(db: &WorkDb, root: &str, at: i64) {
    sql(
        db,
        "UPDATE pr_family_outcomes SET merged_at=?2 WHERE work_item_id=?1",
        params![root, at.to_string()],
    );
}

#[test]
fn the_first_launch_of_a_family_opens_its_row() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_test_chore(&db, &product, "root");
    sql(&db, "UPDATE tasks SET pr_head_sha='published' WHERE id=?1", [&root.id]);
    let execution = create_ready_chore_execution(&db, &root.id);
    assert!(
        ledger(&db, &root.id).is_none(),
        "nothing opens before a launch is recorded"
    );

    launch(&db, &execution, "claude", None);

    let row = ledger(&db, &root.id).expect("the first launch opens the family");
    assert_eq!(row.first_driver.as_deref(), Some("claude"));
    assert_eq!(row.baseline_head.as_deref(), Some("published"));
    assert!(
        row.opened_at.parse::<i64>().is_ok(),
        "opened_at is epoch seconds: {}",
        row.opened_at
    );
    assert_eq!((row.first_ci, row.merged_at), (None, None));
    assert_eq!(first_head(&db, &root.id), None);
}

#[test]
fn a_family_opens_without_any_experiment_running() {
    // The ledger is not an experiment's: an empty or unrecorded arm set opens it too.
    for arms in [None, Some("{}"), Some(r#"{"alpha":"treatment"}"#)] {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_test_chore(&db, &product, "root");
        let execution = create_ready_chore_execution(&db, &root.id);
        launch(&db, &execution, "codex", arms);
        assert_eq!(ledger_rows(&db), 1, "{arms:?}");
    }
}

#[test]
fn later_launches_in_the_family_do_not_reopen_or_change_the_row() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_test_chore(&db, &product, "root");
    let first = create_ready_chore_execution(&db, &root.id);
    launch(&db, &first, "claude", None);
    let before = ledger(&db, &root.id).unwrap();
    sql(&db, "UPDATE tasks SET pr_head_sha='later' WHERE id=?1", [&root.id]);

    let revision = make_revision(&db, &product, &root.id, "revision");
    let second = create_ready_chore_execution(&db, &revision.id);
    launch(&db, &second, "codex", None);
    // A retry of the first execution launches again too.
    launch(&db, &first, "grok", None);

    assert_eq!(ledger_rows(&db), 1);
    let after = ledger(&db, &root.id).unwrap();
    assert_eq!(after.first_driver.as_deref(), Some("claude"));
    assert_eq!(after.baseline_head, before.baseline_head);
    assert_eq!(after.opened_at, before.opened_at);
    assert!(
        ledger(&db, &revision.id).is_none(),
        "a revision is never its own family"
    );
}

#[test]
fn a_revision_launching_first_opens_the_family_of_its_root() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_test_chore(&db, &product, "root");
    let revision = make_revision(&db, &product, &root.id, "revision");
    let execution = create_ready_chore_execution(&db, &revision.id);
    launch(&db, &execution, "claude", None);
    assert!(ledger(&db, &root.id).is_some());
    assert!(ledger(&db, &revision.id).is_none());
}

#[test]
fn a_family_that_launched_before_the_ledger_is_never_opened_mid_life() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_test_chore(&db, &product, "root");
    let first = create_ready_chore_execution(&db, &root.id);
    launch(&db, &first, "claude", None);
    // The family launched before the ledger existed: it has a launched
    // execution and no row.
    sql(&db, "DELETE FROM pr_family_outcomes", []);

    let revision = make_revision(&db, &product, &root.id, "revision");
    let second = create_ready_chore_execution(&db, &revision.id);
    launch(&db, &second, "claude", None);

    assert_eq!(
        ledger_rows(&db),
        0,
        "a later launch must not pass for the family's first"
    );
}

#[test]
fn a_family_whose_only_pre_ledger_launch_was_a_guide_is_never_opened_mid_life() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_test_chore(&db, &product, "root");
    publish_pr(&db, &root.id);
    let (_series, comparison) = crate::test_support::seed_review_guide_series(&db, &root.id);
    let guide = db
        .create_pr_review_guide_execution(&comparison, "https://github.com/acme/widget")
        .unwrap();
    launch(&db, &guide, "claude", None);
    // The family launched (via a review guide) before the ledger existed.
    sql(&db, "DELETE FROM pr_family_outcomes", []);
    assert_eq!(ledger_rows(&db), 0, "no launched task execution, and no ledger row");

    let later = create_ready_chore_execution(&db, &root.id);
    launch(&db, &later, "codex", None);

    assert_eq!(
        ledger_rows(&db),
        0,
        "a later launch must not pass for the family's first"
    );
}

#[test]
fn an_unlaunched_execution_does_not_count_as_an_earlier_launch() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_test_chore(&db, &product, "root");
    // Created, never launched: a pre-start failure leaves rows like this.
    let _never_launched = create_ready_chore_execution(&db, &root.id);
    let launched = create_ready_chore_execution(&db, &root.id);
    launch(&db, &launched, "claude", None);
    assert!(ledger(&db, &root.id).is_some());
}

#[test]
fn a_work_item_that_is_not_a_task_opens_nothing() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let task = create_test_chore(&db, &product, "stand-in");
    let execution = create_ready_chore_execution(&db, &task.id);
    sql(
        &db,
        "UPDATE work_executions SET work_item_id='not-a-task' WHERE id=?1",
        [&execution.id],
    );
    launch(&db, &execution, "claude", None);
    assert_eq!(ledger_rows(&db), 0);
}

#[test]
fn first_ci_is_the_first_terminal_result_on_the_first_head_after_the_first_launch() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_test_chore(&db, &product, "root");
    sql(&db, "UPDATE tasks SET pr_head_sha='before' WHERE id=?1", [&root.id]);
    let execution = create_ready_chore_execution(&db, &root.id);
    launch(&db, &execution, "claude", None);

    let observe = |head: &str, state: &str| {
        {
            let conn = db.connect().unwrap();
            observe_ci(&conn, &root.id, Some(head), state).unwrap();
        }
        ledger(&db, &root.id).unwrap().first_ci
    };
    // The head that existed at the first launch never counts.
    assert_eq!(observe("before", "success"), None);
    assert_eq!(observe("first", "in_progress"), None);
    assert_eq!(observe("first", "fail").as_deref(), Some("fail"));
    // A rerun on the same head, or a fix on a new head, cannot rewrite it.
    assert_eq!(observe("first", "success").as_deref(), Some("fail"));
    assert_eq!(observe("fix", "success").as_deref(), Some("fail"));
    assert_eq!(first_head(&db, &root.id).as_deref(), Some("first"));
}

#[test]
fn ci_is_observed_through_a_revision_and_ignored_for_unknown_families() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_test_chore(&db, &product, "root");
    let revision = make_revision(&db, &product, &root.id, "revision");
    let stranger = create_test_chore(&db, &product, "never launched");
    let execution = create_ready_chore_execution(&db, &root.id);
    launch(&db, &execution, "claude", None);
    {
        let conn = db.connect().unwrap();
        observe_ci(&conn, &revision.id, Some("head"), "success").unwrap();
        observe_ci(&conn, &stranger.id, Some("head"), "fail").unwrap();
        observe_ci(&conn, &root.id, None, "fail").unwrap();
    }
    assert_eq!(ledger(&db, &root.id).unwrap().first_ci.as_deref(), Some("success"));
    assert_eq!(ledger_rows(&db), 1, "a family that never launched records nothing");
}

#[test]
fn the_merge_is_stamped_once_through_any_family_member() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_test_chore(&db, &product, "root");
    let revision = make_revision(&db, &product, &root.id, "revision");
    let execution = create_ready_chore_execution(&db, &root.id);
    launch(&db, &execution, "claude", None);
    {
        let conn = db.connect().unwrap();
        record_merge(&conn, &revision.id, "100").unwrap();
        record_merge(&conn, &root.id, "200").unwrap();
        record_merge(&conn, "not-a-task", "300").unwrap();
    }
    assert_eq!(ledger(&db, &root.id).unwrap().merged_at.as_deref(), Some("100"));
}

#[test]
fn the_merge_poller_and_the_poll_state_write_the_ledger() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_test_chore(&db, &product, "root");
    let execution = create_ready_chore_execution(&db, &root.id);
    launch(&db, &execution, "claude", None);
    sql(
        &db,
        "UPDATE tasks SET pr_url=?2, status='in_review' WHERE id=?1",
        params![root.id, PR],
    );
    db.update_task_pr_poll_state(
        &root.id,
        PrPollStateInput {
            pr_head_sha: Some("first"),
            ci_required_state: "success",
            ..Default::default()
        },
    )
    .unwrap();
    db.mark_chore_pr_merged(&root.id, PR).unwrap();
    let row = ledger(&db, &root.id).unwrap();
    assert_eq!(row.first_ci.as_deref(), Some("success"));
    assert!(row.merged_at.is_some());
}

const ALPHA_TREATED: &str = r#"{"alpha":"treatment"}"#;
const ALPHA_CONTROL: &str = r#"{"alpha":"control"}"#;

/// A family with a published PR whose first execution launched with `arms`.
fn family(db: &WorkDb, product: &str, name: &str, arms: Option<&str>) -> (Task, WorkExecution) {
    let root = create_test_chore(db, product, name);
    publish_pr(db, &root.id);
    let execution = create_ready_chore_execution(db, &root.id);
    launch(db, &execution, "claude", arms);
    (root, execution)
}

#[test]
fn without_an_experiment_every_family_with_a_pr_is_read_with_no_arms() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let (open, _) = family(&db, &product, "open", None);
    let (merged, _) = family(&db, &product, "merged", Some(ALPHA_TREATED));
    merge(&db, &merged.id, 50);
    // No PR yet: not a PR family.
    let unpublished = create_test_chore(&db, &product, "no pr");
    launch(&db, &create_ready_chore_execution(&db, &unpublished.id), "claude", None);
    // Tombstoned: not read.
    let (deleted, _) = family(&db, &product, "deleted", None);
    sql(&db, "UPDATE tasks SET deleted_at='9' WHERE id=?1", [&deleted.id]);

    let records = db.family_outcome_records(None).unwrap();
    let roots: Vec<_> = records.iter().map(|record| record.family_root.as_str()).collect();
    assert_eq!(roots.len(), 2, "{roots:?}");
    assert!(roots.contains(&open.id.as_str()) && roots.contains(&merged.id.as_str()));
    assert!(records.iter().all(|record| record.launch_arms.is_empty()));
    let merged_record = records.iter().find(|record| record.family_root == merged.id).unwrap();
    assert!(merged_record.merged);
    assert_eq!(merged_record.first_driver.as_deref(), Some("claude"));
    assert!(
        !records
            .iter()
            .find(|record| record.family_root == open.id)
            .unwrap()
            .merged
    );
}

#[test]
fn an_experiment_reads_only_the_families_that_recorded_it_with_every_launch_arm() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let (treated, _) = family(&db, &product, "treated", Some(ALPHA_TREATED));
    let (_other, _) = family(&db, &product, "other experiment", Some(r#"{"beta":"control"}"#));
    let (_unrecorded, _) = family(&db, &product, "unrecorded", None);
    let (_bare, _) = family(&db, &product, "bare", Some("{}"));
    // A family whose revision launched under the other arm, and one whose
    // revision recorded nothing for the experiment.
    let (flipped, _) = family(&db, &product, "flipped", Some(ALPHA_TREATED));
    let revision = make_revision(&db, &product, &flipped.id, "revision");
    launch(
        &db,
        &create_ready_chore_execution(&db, &revision.id),
        "claude",
        Some(ALPHA_CONTROL),
    );
    let (half_recorded, _) = family(&db, &product, "half recorded", Some(ALPHA_CONTROL));
    let revision = make_revision(&db, &product, &half_recorded.id, "revision");
    launch(
        &db,
        &create_ready_chore_execution(&db, &revision.id),
        "claude",
        Some("{}"),
    );
    // Created but never launched: its arms are not in the record.
    let never = create_ready_chore_execution(&db, &treated.id);
    sql(
        &db,
        "UPDATE work_executions SET experiment_arms=?2 WHERE id=?1",
        params![never.id, ALPHA_CONTROL],
    );

    let records = db.family_outcome_records(Some("alpha")).unwrap();
    let arms = |root: &Task| {
        let mut arms = records
            .iter()
            .find(|record| record.family_root == root.id)
            .unwrap_or_else(|| panic!("{} is read", root.name))
            .launch_arms
            .clone();
        arms.sort();
        arms
    };
    assert_eq!(records.len(), 3, "only the families that recorded `alpha`");
    assert_eq!(arms(&treated), [Some("treatment".to_owned())]);
    assert_eq!(
        arms(&flipped),
        [Some("control".to_owned()), Some("treatment".to_owned())]
    );
    assert_eq!(arms(&half_recorded), [None, Some("control".to_owned())]);
}

#[test]
fn a_family_is_read_through_its_revisions_and_its_review_guide() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let (root, _) = family(&db, &product, "root", Some(ALPHA_TREATED));
    // A review guide's work item is a source comparison hanging off the root.
    let (_series, comparison) = crate::test_support::seed_review_guide_series(&db, &root.id);
    let guide = db
        .create_pr_review_guide_execution(&comparison, "https://github.com/acme/widget")
        .unwrap();
    launch(&db, &guide, "claude", Some(ALPHA_CONTROL));

    let records = db.family_outcome_records(Some("alpha")).unwrap();
    assert_eq!(records.len(), 1);
    let mut arms = records[0].launch_arms.clone();
    arms.sort();
    assert_eq!(arms, [Some("control".to_owned()), Some("treatment".to_owned())]);
}

#[test]
fn a_post_merge_launch_does_not_move_a_merged_family_into_mixed() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let (root, first) = family(&db, &product, "root", Some(ALPHA_TREATED));
    set_times(&db, &first.id, 10);
    merge(&db, &root.id, 50);

    let after = make_revision(&db, &product, &root.id, "after the merge");
    let late = create_ready_chore_execution(&db, &after.id);
    launch(&db, &late, "claude", Some("{}"));
    set_times(&db, &late.id, 90);

    let (_series, comparison) = crate::test_support::seed_review_guide_series(&db, &root.id);
    let guide = db
        .create_pr_review_guide_execution(&comparison, "https://github.com/acme/widget")
        .unwrap();
    launch(&db, &guide, "claude", Some("{}"));
    set_times(&db, &guide.id, 91);

    let records = db.family_outcome_records(Some("alpha")).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].launch_arms,
        [Some("treatment".to_owned())],
        "post-merge empty-arm launches must not mix a merged family"
    );
}

#[test]
fn outcome_facts_are_counted_per_family_up_to_the_merge() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let (root, first) = family(&db, &product, "root", Some(ALPHA_TREATED));
    set_times(&db, &first.id, 10);
    add_run(&db, &first, 11, Some(100));
    // Two launched revisions before the merge, one after, one never launched.
    for at in [20, 30] {
        let revision = make_revision(&db, &product, &root.id, &format!("before the merge {at}"));
        let execution = create_ready_chore_execution(&db, &revision.id);
        launch(&db, &execution, "claude", Some(ALPHA_TREATED));
        set_times(&db, &execution.id, at);
        add_run(&db, &execution, at + 1, Some(10));
    }
    let after_merge = make_revision(&db, &product, &root.id, "after the merge");
    let late = create_ready_chore_execution(&db, &after_merge.id);
    launch(&db, &late, "claude", Some(ALPHA_TREATED));
    set_times(&db, &late.id, 90);
    add_run(&db, &late, 91, Some(1000));
    let _unlaunched = make_revision(&db, &product, &root.id, "never launched");
    merge(&db, &root.id, 50);

    // Review facts: accepted verdicts before the merge count, a stale one and a
    // late one do not; two pre-merge batches and one post-merge batch with one
    // live and one tombstoned follow-up.
    let live = create_test_chore(&db, &product, "post-merge follow-up");
    let tombstoned = create_test_chore(&db, &product, "orphaned follow-up");
    let conn = db.connect().unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO pr_review_batches (id, cycle_root_id, base_sha, classification_json, created_at, phase, pr_number, pr_url, status, target_sha, updated_at)
         VALUES ('b1','{root}','b','{{}}','1','pre_merge',1,'{PR}','completed','t1','1'),
                ('b2','{root}','b','{{}}','2','pre_merge',1,'{PR}','failed','t2','2'),
                ('b3','{root}','b','{{}}','60','post_merge',1,'{PR}','completed','t3','60'),
                ('b4','{root}','b','{{}}','61','post_merge',1,'{PR}','completed','t4','61');",
        root = root.id
    ))
    .unwrap();
    for (id, findings, outcome, at) in [
        ("accepted", 3, crate::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS, 40),
        ("stale", 4, crate::REVIEW_GATE_OUTCOME_STALE_HEAD, 41),
        ("late", 5, crate::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS, 70),
    ] {
        conn.execute(
            "INSERT INTO pr_review_verdicts (id, execution_id, work_item_id, findings_count, gate_outcome, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, first.id, root.id, findings, outcome, at.to_string()],
        )
        .unwrap();
    }
    conn.execute("UPDATE tasks SET deleted_at='99' WHERE id=?1", [&tombstoned.id])
        .unwrap();
    for (id, batch, followup) in [("v-live", "b3", &live.id), ("v-dead", "b4", &tombstoned.id)] {
        conn.execute(
            "INSERT INTO pr_review_verdicts (id, execution_id, work_item_id, findings_count, gate_outcome, created_at, batch_id, revision_task_id)
             VALUES (?1, ?2, ?3, 1, ?4, '61', ?5, ?6)",
            params![id, first.id, root.id, crate::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS, batch, followup],
        )
        .unwrap();
    }
    drop(conn);
    // The first head's CI failed.
    observe_ci(&db.connect().unwrap(), &root.id, Some("h1"), "fail").unwrap();

    let records = db.family_outcome_records(None).unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert!(record.merged);
    assert_eq!(record.first_ci, Some(false));
    assert_eq!(record.revisions, 2, "two launched revisions before the merge");
    assert_eq!(record.review_batches, 2, "pre-merge batches, whatever their status");
    assert_eq!(
        record.findings, 3,
        "the accepted verdict before the merge; stale and later verdicts are not counted"
    );
    assert_eq!(record.followups, 1, "the tombstoned follow-up is not counted");
    let mut tokens: Vec<_> = record.runs.iter().map(|run| run.input_tokens).collect();
    tokens.sort();
    assert_eq!(
        tokens,
        [Some(10), Some(10), Some(100)],
        "the run after the merge is not the family's cost"
    );
}

#[test]
fn only_merged_families_carry_runs() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let (open, execution) = family(&db, &product, "open", None);
    add_run(&db, &execution, 5, Some(10));
    let (merged, execution) = family(&db, &product, "merged", None);
    add_run(&db, &execution, 5, Some(10));
    merge(&db, &merged.id, 50);
    let records = db.family_outcome_records(None).unwrap();
    let runs = |root: &Task| {
        records
            .iter()
            .find(|record| record.family_root == root.id)
            .unwrap()
            .runs
            .len()
    };
    assert_eq!(runs(&open), 0);
    assert_eq!(runs(&merged), 1);
}

#[test]
fn a_family_with_a_run_with_no_measurement_is_read_as_such() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let (merged, execution) = family(&db, &product, "merged", None);
    add_run(&db, &execution, 5, None);
    merge(&db, &merged.id, 50);
    let records = db.family_outcome_records(None).unwrap();
    assert_eq!(records[0].runs.len(), 1);
    assert_eq!(records[0].runs[0].input_tokens, None);
}

#[test]
fn an_empty_ledger_reads_as_no_families() {
    let (_dir, db) = open_db();
    assert!(db.family_outcome_records(None).unwrap().is_empty());
    assert!(db.family_outcome_records(Some("alpha")).unwrap().is_empty());
}
