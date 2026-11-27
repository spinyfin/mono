use super::*;
use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
use boss_engine_effort::{SpawnResolutionInput, resolve_spawn_config};

fn run_model(db: &WorkDb, run_id: &str) -> Option<String> {
    db.cost_records_for_window(0, i64::MAX)
        .unwrap()
        .into_iter()
        .find(|record| record.run_id == run_id)
        .unwrap()
        .model
}

#[test]
fn agy_selected_model_is_recorded_on_execution_run_and_allocation() {
    for (effort, reasoning, expected) in [
        (EffortLevel::Small, ReasoningMode::Standard, "gemini-4-hydrogen-medium"),
        (EffortLevel::Large, ReasoningMode::Standard, "gemini-4-neon-xhigh"),
        (EffortLevel::Large, ReasoningMode::Investigation, "gemini-4-carbon-high"),
    ] {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let task = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(&product.id)
                    .name("Gemini selection")
                    .driver("agy")
                    .effort_level(effort)
                    .reasoning(reasoning)
                    .build(),
            )
            .unwrap();
        let execution = create_ready_chore_execution(&db, &task.id);
        let config = resolve_spawn_config(
            &SpawnResolutionInput::builder()
                .task_driver("agy")
                .effort_level(effort)
                .reasoning(reasoning)
                .build(),
        )
        .unwrap();
        assert_eq!(config.model, expected);
        let (_, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "repo",
                "lease",
                "workspace",
                "/tmp/gemini-selection",
            )
            .unwrap();
        db.record_execution_launch_config(&execution.id, &config.driver, &config.model, Some(effort))
            .unwrap();
        assert_eq!(
            db.get_execution(&execution.id).unwrap().model.as_deref(),
            Some(expected)
        );
        assert_eq!(run_model(&db, &run.id).as_deref(), Some(expected));
        assert_eq!(
            db.get_execution_driver_decision(&execution.id)
                .unwrap()
                .unwrap()
                .model
                .as_deref(),
            Some(expected)
        );
        db.record_execution_launch_config(&execution.id, "agy", "gemini-4-carbon-low", None)
            .unwrap();
        assert_eq!(run_model(&db, &run.id).as_deref(), Some(expected));
    }
}

#[test]
fn allocation_model_migration_preserves_older_decisions() {
    let (dir, db) = open_db();
    let product = create_test_product(&db);
    let task = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(&product.id)
                .name("Older allocation")
                .driver("agy")
                .build(),
        )
        .unwrap();
    let execution = create_ready_chore_execution(&db, &task.id);
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "ALTER TABLE execution_driver_decisions DROP COLUMN model;
        DELETE FROM schema_migrations WHERE id = '20261122_0432_execution_allocation_model';",
    )
    .unwrap();
    drop(conn);
    // Reopening exercises the real registry upgrade path rather than a duplicate SQL copy.
    let upgraded = WorkDb::open(dir.path().join("state.db")).unwrap();
    let decision = upgraded.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.driver.as_deref(), Some("agy"));
    assert_eq!(decision.model, None);
}

#[test]
fn launch_recording_preserves_observed_or_mixed_model_usage() {
    for observed in [Some("gemini-4-neon-high"), None] {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let task = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(&product.id)
                    .name("Observed Gemini usage")
                    .driver("agy")
                    .build(),
            )
            .unwrap();
        let execution = create_ready_chore_execution(&db, &task.id);
        let (_, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "repo",
                "lease",
                "workspace",
                "/tmp/gemini-selection",
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_runs SET model=?2,input_tokens=100 WHERE id=?1",
                params![run.id, observed],
            )
            .unwrap();
        db.record_execution_launch_config(&execution.id, "agy", "gemini-4-hydrogen-low", None)
            .unwrap();
        assert_eq!(run_model(&db, &run.id).as_deref(), observed);
    }
}

// ── experiment arms in the launch record ────────────────────────────────────

const TREATED: &str = r#"{"alpha":"treatment","beta":"control"}"#;

fn new_execution(db: &WorkDb) -> WorkExecution {
    let product = create_test_product(db);
    let task = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(&product.id)
                .name("Experiment arms")
                .build(),
        )
        .unwrap();
    create_ready_chore_execution(db, &task.id)
}

fn record(db: &WorkDb, execution_id: &str, arms: Option<&str>) {
    db.record_execution_launch(&ExecutionLaunchRecord {
        execution_id,
        driver: "claude",
        model: "opus",
        effort_level: Some(EffortLevel::Small),
        experiment_arms: arms,
    })
    .unwrap();
}

fn stored_arms_column(db: &WorkDb, execution_id: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT experiment_arms FROM work_executions WHERE id = ?1",
            [execution_id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn first_launch_writes_the_tuple_and_the_arms_together() {
    let (_dir, db) = open_db();
    let execution = new_execution(&db);
    assert_eq!(
        db.stored_experiment_arms(&execution.id).unwrap(),
        StoredExperimentArms::NotLaunched
    );

    let updated = db
        .record_execution_launch(&ExecutionLaunchRecord {
            execution_id: &execution.id,
            driver: "claude",
            model: "opus",
            effort_level: Some(EffortLevel::Small),
            experiment_arms: Some(TREATED),
        })
        .unwrap();
    assert_eq!(updated.driver.as_deref(), Some("claude"));
    assert_eq!(updated.model.as_deref(), Some("opus"));
    assert_eq!(updated.effort_level, Some(EffortLevel::Small));
    assert_eq!(stored_arms_column(&db, &execution.id).as_deref(), Some(TREATED));
    assert_eq!(
        db.stored_experiment_arms(&execution.id).unwrap(),
        StoredExperimentArms::Recorded(TREATED.to_owned())
    );
}

#[test]
fn an_empty_arm_set_is_recorded_as_nothing_running_and_is_not_null() {
    let (_dir, db) = open_db();
    let execution = new_execution(&db);
    record(&db, &execution.id, Some("{}"));
    assert_eq!(stored_arms_column(&db, &execution.id).as_deref(), Some("{}"));
    assert_eq!(
        db.stored_experiment_arms(&execution.id).unwrap(),
        StoredExperimentArms::Recorded("{}".to_owned())
    );
}

#[test]
fn the_four_argument_wrapper_leaves_the_arms_not_recorded() {
    let (_dir, db) = open_db();
    let execution = new_execution(&db);
    let updated = db
        .record_execution_launch_config(&execution.id, "codex", "gpt-5.5-codex", Some(EffortLevel::Large))
        .unwrap();
    assert_eq!(updated.driver.as_deref(), Some("codex"));
    assert_eq!(updated.effort_level, Some(EffortLevel::Large));
    assert_eq!(stored_arms_column(&db, &execution.id), None);
    assert_eq!(
        db.stored_experiment_arms(&execution.id).unwrap(),
        StoredExperimentArms::Unrecorded
    );
}

#[test]
fn a_later_launch_reuses_the_stored_arms_whatever_it_resolved() {
    let (_dir, db) = open_db();
    let execution = new_execution(&db);
    record(&db, &execution.id, Some(TREATED));

    // A retry that resolved other arms, a new experiment, or none at all.
    for later in [
        r#"{"alpha":"control","beta":"treatment"}"#,
        r#"{"alpha":"treatment","beta":"control","gamma":"treatment"}"#,
        "{}",
    ] {
        db.record_execution_launch(&ExecutionLaunchRecord {
            execution_id: &execution.id,
            driver: "codex",
            model: "another-model",
            effort_level: Some(EffortLevel::Large),
            experiment_arms: Some(later),
        })
        .unwrap();
        assert_eq!(
            stored_arms_column(&db, &execution.id).as_deref(),
            Some(TREATED),
            "{later}"
        );
    }
    let execution = db.get_execution(&execution.id).unwrap();
    assert_eq!(execution.driver.as_deref(), Some("claude"));
    assert_eq!(execution.model.as_deref(), Some("opus"));
}

#[test]
fn a_later_launch_that_stopped_an_experiment_rewrites_it_in_both_arms() {
    let (_dir, db) = open_db();
    let execution = new_execution(&db);
    record(
        &db,
        &execution.id,
        Some(r#"{"alpha":"treatment","beta":"control","gamma":"treatment"}"#),
    );

    // alpha (treatment) and beta (control) were turned off; gamma kept running.
    record(
        &db,
        &execution.id,
        Some(r#"{"alpha":"stopped","beta":"stopped","gamma":"treatment"}"#),
    );
    assert_eq!(
        stored_arms_column(&db, &execution.id).as_deref(),
        Some(r#"{"alpha":"stopped","beta":"stopped","gamma":"treatment"}"#),
    );

    // A stopped key stays stopped, and nothing re-arms it.
    record(&db, &execution.id, Some(TREATED));
    assert_eq!(
        stored_arms_column(&db, &execution.id).as_deref(),
        Some(r#"{"alpha":"stopped","beta":"stopped","gamma":"treatment"}"#),
    );
}

#[test]
fn a_later_launch_never_fills_in_arms_the_first_did_not_record() {
    let (_dir, db) = open_db();
    let execution = new_execution(&db);
    record(&db, &execution.id, None);
    record(&db, &execution.id, Some(TREATED));
    record(&db, &execution.id, Some(r#"{"alpha":"stopped"}"#));
    assert_eq!(stored_arms_column(&db, &execution.id), None);
    assert_eq!(
        db.stored_experiment_arms(&execution.id).unwrap(),
        StoredExperimentArms::Unrecorded
    );
}

#[test]
fn text_that_is_not_an_arm_set_is_not_stored_but_the_launch_still_is() {
    for bad in [
        "",
        "null",
        "[]",
        "not json",
        r#"{"alpha":"half"}"#,
        r#"{"alpha":1}"#,
        r#"{"alpha":"treatment""#,
    ] {
        let (_dir, db) = open_db();
        let execution = new_execution(&db);
        let updated = db
            .record_execution_launch(&ExecutionLaunchRecord {
                execution_id: &execution.id,
                driver: "claude",
                model: "opus",
                effort_level: None,
                experiment_arms: Some(bad),
            })
            .unwrap();
        assert_eq!(updated.model.as_deref(), Some("opus"), "{bad:?}");
        assert_eq!(stored_arms_column(&db, &execution.id), None, "{bad:?}");
    }
}

#[test]
fn arms_are_stored_in_canonical_text_whatever_the_caller_wrote() {
    let (_dir, db) = open_db();
    let execution = new_execution(&db);
    record(
        &db,
        &execution.id,
        Some(r#" { "beta" : "control" , "alpha" : "treatment" } "#),
    );
    assert_eq!(stored_arms_column(&db, &execution.id).as_deref(), Some(TREATED));
}

#[test]
fn stored_arms_for_a_missing_execution_is_an_error() {
    let (_dir, db) = open_db();
    assert!(db.stored_experiment_arms("exec_missing").is_err());
}

#[test]
fn experiment_arms_migration_upgrades_a_database_without_the_column() {
    let (dir, db) = open_db();
    let execution = new_execution(&db);
    record(&db, &execution.id, Some(TREATED));
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "ALTER TABLE work_executions DROP COLUMN experiment_arms;
        DELETE FROM schema_migrations WHERE id = '20261127_2050_execution_experiment_arms';",
    )
    .unwrap();
    drop(conn);
    drop(db);
    // Reopening runs the real registry upgrade, not a copy of its SQL.
    let upgraded = WorkDb::open(dir.path().join("state.db")).unwrap();
    assert_eq!(
        stored_arms_column(&upgraded, &execution.id),
        None,
        "rows from before the column read as not recorded"
    );
    // The launch the dropped column had recorded is still a launch, so a later
    // run of this execution is not treated as a first one.
    assert_eq!(
        upgraded.stored_experiment_arms(&execution.id).unwrap(),
        StoredExperimentArms::Unrecorded
    );
}

// ── PR family and bucket ────────────────────────────────────────────────────

fn insert_revision(db: &WorkDb, product_id: &str, parent_id: &str) -> String {
    let id = next_id("task");
    let now = now_string();
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             VALUES (?1, ?2, 'revision', 'revision', '', 'todo', ?3, ?3, ?4)",
            params![id, product_id, now, parent_id],
        )
        .unwrap();
    id
}

#[test]
fn a_work_item_and_every_revision_under_it_share_one_family() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore(&db, &product.id, "root");
    let first = insert_revision(&db, &product.id, &root.id);
    let second = insert_revision(&db, &product.id, &first);
    let conn = db.connect().unwrap();
    for (kind, work_item_id) in [
        (ExecutionKind::ChoreImplementation, &root.id),
        (ExecutionKind::RevisionImplementation, &first),
        (ExecutionKind::RevisionImplementation, &second),
        (ExecutionKind::PrReview, &root.id),
        (ExecutionKind::PrReview, &second),
        (ExecutionKind::CiRemediation, &first),
    ] {
        assert_eq!(
            experiment_family_root(&conn, &kind, work_item_id).unwrap(),
            root.id,
            "{kind:?} on {work_item_id}"
        );
    }
}

#[test]
fn a_work_item_with_no_task_row_is_its_own_family() {
    let (_dir, db) = open_db();
    let conn = db.connect().unwrap();
    assert_eq!(
        experiment_family_root(&conn, &ExecutionKind::AnswerAgent, "comment_123").unwrap(),
        "comment_123"
    );
}

#[test]
fn an_executions_family_is_read_from_its_own_work_item() {
    let (_dir, db) = open_db();
    let execution = new_execution(&db);
    assert_eq!(db.experiment_family_root(&execution).unwrap(), execution.work_item_id);
}

#[test]
fn a_review_guide_takes_the_family_of_the_pr_it_explains() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore(&db, &product.id, "root");
    let revision = insert_revision(&db, &product.id, &root.id);
    let (_series, comparison) = crate::test_support::seed_review_guide_series(&db, &root.id);
    let guide = db
        .create_pr_review_guide_execution(&comparison, "https://github.com/acme/widget")
        .unwrap();
    assert_eq!(guide.work_item_id, comparison);
    let guide_family = db.experiment_family_root(&guide).unwrap();
    assert_eq!(guide_family, root.id);

    // The same family as the work item and its revisions, so the guide is in
    // the arm of the PR it explains.
    let conn = db.connect().unwrap();
    assert_eq!(
        experiment_family_root(&conn, &ExecutionKind::RevisionImplementation, &revision).unwrap(),
        guide_family
    );
}

#[test]
fn a_review_guide_whose_comparison_does_not_resolve_is_its_own_family() {
    let (_dir, db) = open_db();
    let conn = db.connect().unwrap();
    assert_eq!(
        experiment_family_root(&conn, &ExecutionKind::PrReviewGuide, "comparison_gone").unwrap(),
        "comparison_gone"
    );
}

#[test]
fn the_bucket_is_stable_in_range_and_independent_across_keys() {
    for family in ["task_a", "task_b", "task_c", ""] {
        for key in ["batched_search", "ranged_reads"] {
            let bucket = experiment_bucket(family, key);
            assert_eq!(bucket, experiment_bucket(family, key), "stable for {family}/{key}");
            assert!(bucket < 100, "{family}/{key}");
        }
    }
    // Including the key makes two experiments' draws differ for some families
    // and agree for others, rather than one decision applied to both.
    let families: Vec<String> = (0..400).map(|n| format!("task_{n}")).collect();
    let agree = families
        .iter()
        .filter(|family| (experiment_bucket(family, "alpha") < 50) == (experiment_bucket(family, "beta") < 50))
        .count();
    assert!(
        (120..=280).contains(&agree),
        "two keys agree on {agree} of 400 families"
    );
}

#[test]
fn the_bucket_treats_about_half_of_families() {
    let treated = (0..2000)
        .filter(|n| experiment_bucket(&format!("task_{n}"), "batched_search") < 50)
        .count();
    assert!(
        (900..=1100).contains(&treated),
        "{treated} of 2000 families fell below the treatment limit"
    );
}
