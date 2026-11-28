//! `boss cost experiment` end to end: the family outcome ledger written by the
//! launch, the merge poller's CI and merge observations, and the completion
//! path, read back by the work database and folded by the outcomes builder.

use super::raw;
use crate::cost_report::outcomes::{ALL_FAMILIES_ARM, build_experiment_outcomes};
use crate::test_support::{create_product, create_ready_chore_execution, create_test_chore, open_db};
use crate::work::{ExecutionLaunchRecord, PrPollStateInput, WorkDb};
use boss_engine_effort::sol::{ASTRA_MODEL, SOL_MODEL};
use boss_protocol::{ArmOutcomes, ExperimentOutcomes, Task, WorkExecution};
use rusqlite::params;

const PR: &str = "https://github.com/org/repo/pull/1";
const TREATED: &str = r#"{"alpha":"treatment"}"#;
const CONTROL: &str = r#"{"alpha":"control"}"#;
const STOPPED: &str = r#"{"alpha":"stopped"}"#;

fn launch(db: &WorkDb, execution: &WorkExecution, driver: &str, arms: &str) {
    db.record_execution_launch(&ExecutionLaunchRecord {
        execution_id: &execution.id,
        driver,
        model: "model",
        effort_level: None,
        experiment_arms: Some(arms),
    })
    .unwrap();
}

fn poll(db: &WorkDb, root: &Task, head: &str, ci: &str) {
    db.update_task_pr_poll_state(
        &root.id,
        PrPollStateInput {
            pr_head_sha: Some(head),
            ci_required_state: ci,
            ..Default::default()
        },
    )
    .unwrap();
}

/// A run of `execution` at `at` seconds with `tokens` input tokens, or none.
fn run(db: &WorkDb, execution: &WorkExecution, at: i64, tokens: Option<i64>) {
    let run = db
        .create_run(
            boss_protocol::CreateRunInput::builder()
                .execution_id(&execution.id)
                .agent_id("worker")
                .status("completed")
                .build(),
        )
        .unwrap();
    raw(db)
        .execute(
            "UPDATE work_runs SET model='model',input_tokens=?2,output_tokens=?3,created_at=?4 WHERE id=?1",
            params![run.id, tokens, tokens.map(|_| 0), at.to_string()],
        )
        .unwrap();
}

/// One PR family: a root task with a published PR whose first execution
/// launched on `driver` under `arms`, with one measured run, polled through a
/// CI result and (optionally) merged through the poller's path.
struct Family {
    root: Task,
    first: WorkExecution,
}

fn family(db: &WorkDb, product: &str, name: &str, driver: &str, arms: &str) -> Family {
    let root = create_test_chore(db, product, name);
    raw(db)
        .execute(
            "UPDATE tasks SET pr_url=?2, status='in_review' WHERE id=?1",
            params![root.id, PR],
        )
        .unwrap();
    let first = create_ready_chore_execution(db, &root.id);
    launch(db, &first, driver, arms);
    Family { root, first }
}

fn merged(db: &WorkDb, family: &Family) {
    db.mark_chore_pr_merged(&family.root.id, PR).unwrap();
}

fn arm<'a>(outcomes: &'a ExperimentOutcomes, name: &str) -> &'a ArmOutcomes {
    outcomes.arms.iter().find(|arm| arm.arm == name).unwrap()
}

#[test]
fn an_experiment_compares_arms_excludes_mixed_and_stopped_families_and_splits_by_driver() {
    let (_dir, db) = open_db();
    let product = create_product(&db);

    // Treatment: one merged family on claude whose first CI failed after a
    // revision, one merged on codex, one still open.
    let treated = family(&db, &product, "treated claude", "claude", TREATED);
    run(&db, &treated.first, 10, Some(1_000));
    poll(&db, &treated.root, "t1", "fail");
    let revision = create_test_chore(&db, &product, "revision");
    raw(&db)
        .execute(
            "UPDATE tasks SET kind='revision',parent_task_id=?2 WHERE id=?1",
            params![revision.id, treated.root.id],
        )
        .unwrap();
    let revision_execution = create_ready_chore_execution(&db, &revision.id);
    launch(&db, &revision_execution, "claude", TREATED);
    run(&db, &revision_execution, 20, Some(500));
    merged(&db, &treated);

    let treated_codex = family(&db, &product, "treated codex", "codex", TREATED);
    run(&db, &treated_codex.first, 10, Some(300));
    poll(&db, &treated_codex.root, "c1", "success");
    merged(&db, &treated_codex);

    let _open = family(&db, &product, "treated open", "claude", TREATED);

    // Control: one merged family whose run has no measurement.
    let control = family(&db, &product, "control", "claude", CONTROL);
    run(&db, &control.first, 10, None);
    poll(&db, &control.root, "k1", "success");
    merged(&db, &control);

    // Left out: a family whose revision launched under the other arm, one
    // whose experiment was stopped, and one that recorded nothing for it.
    let flipped = family(&db, &product, "flipped", "claude", TREATED);
    let flip = create_test_chore(&db, &product, "flip revision");
    raw(&db)
        .execute(
            "UPDATE tasks SET kind='revision',parent_task_id=?2 WHERE id=?1",
            params![flip.id, flipped.root.id],
        )
        .unwrap();
    launch(&db, &create_ready_chore_execution(&db, &flip.id), "claude", CONTROL);
    let _stopped = family(&db, &product, "stopped", "claude", STOPPED);
    let _unrelated = family(&db, &product, "unrelated", "claude", "{}");

    let records = db.family_outcome_records(Some("alpha")).unwrap();
    let outcomes = build_experiment_outcomes(Some("alpha"), &records);

    assert_eq!((outcomes.mixed_families, outcomes.stopped_families), (1, 1));
    let treatment = arm(&outcomes, "treatment");
    assert_eq!(treatment.outcomes.families, 3);
    assert_eq!(treatment.outcomes.merged_families, 2);
    assert_eq!(treatment.outcomes.revisions_before_merge, 1);
    assert_eq!(treatment.outcomes.revisions_per_merged_pr, Some(0.5));
    // First CI: claude's failed on its first head, codex's passed, the open one is unobserved.
    assert_eq!(treatment.outcomes.ci_observed_families, 2);
    assert_eq!(treatment.outcomes.first_attempt_ci_pass_rate, Some(0.5));
    // 1,500 and 300 tokens over the two merged families.
    assert_eq!(treatment.outcomes.tokens_per_merged_family, Some(900.0));
    let drivers: Vec<_> = treatment
        .by_driver
        .iter()
        .map(|row| {
            (
                row.driver.as_str(),
                row.outcomes.families,
                row.outcomes.tokens_per_merged_family,
            )
        })
        .collect();
    assert_eq!(drivers, [("claude", 2, Some(1_500.0)), ("codex", 1, Some(300.0))]);

    let control = arm(&outcomes, "control");
    assert_eq!((control.outcomes.families, control.outcomes.merged_families), (1, 1));
    assert_eq!(control.outcomes.first_attempt_ci_pass_rate, Some(1.0));
    assert_eq!(control.outcomes.tokens_per_merged_family, None);
    assert_eq!(control.outcomes.merged_families_tokens_unmeasured, 1);
}

#[test]
fn without_an_experiment_every_family_gives_one_tokens_per_merged_pr_figure() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    for (name, tokens, arms) in [("one", 100, "{}"), ("two", 300, TREATED), ("three", 500, CONTROL)] {
        let family = family(&db, &product, name, "claude", arms);
        run(&db, &family.first, 10, Some(tokens));
        merged(&db, &family);
    }
    let outcomes = build_experiment_outcomes(None, &db.family_outcome_records(None).unwrap());
    assert_eq!(outcomes.arms.len(), 1);
    assert_eq!(outcomes.arms[0].arm, ALL_FAMILIES_ARM);
    assert_eq!(outcomes.arms[0].outcomes.families, 3);
    assert_eq!(outcomes.arms[0].outcomes.tokens_per_merged_family, Some(300.0));
}

#[test]
fn a_family_the_sol_share_enrolled_through_a_revision_keeps_its_sol_first_ci() {
    // The family ledger opens at the family's first launch; the Sol ledger at
    // its first Sol-eligible execution. Opening the first for every family must
    // not move the head the second judges CI on.
    let (_dir, db) = open_db();
    let product = create_product(&db);
    db.set_codex_sol_percentage(100).unwrap();
    let root = create_test_chore(&db, &product, "claude root");
    raw(&db)
        .execute(
            "UPDATE tasks SET pr_url=?2, status='in_review' WHERE id=?1",
            params![root.id, PR],
        )
        .unwrap();
    let original = create_ready_chore_execution(&db, &root.id);
    db.select_codex_sol_model(&original.id, false, ASTRA_MODEL).unwrap();
    launch(&db, &original, "claude", "{}");
    // The root's own first head passes CI before any Sol work exists.
    poll(&db, &root, "root-head", "success");

    let revision = create_test_chore(&db, &product, "codex revision");
    raw(&db)
        .execute(
            "UPDATE tasks SET kind='revision',parent_task_id=?2 WHERE id=?1",
            params![revision.id, root.id],
        )
        .unwrap();
    let revision_execution = create_ready_chore_execution(&db, &revision.id);
    assert_eq!(
        db.select_codex_sol_model(&revision_execution.id, true, ASTRA_MODEL)
            .unwrap(),
        SOL_MODEL
    );
    launch(&db, &revision_execution, "codex", "{}");
    raw(&db)
        .execute(
            "UPDATE work_executions SET created_at='20',started_at='20' WHERE id=?1",
            [&revision_execution.id],
        )
        .unwrap();
    // The Sol worker's push fails CI.
    poll(&db, &root, "fix-head", "fail");

    let sol = db.sol_comparison_records().unwrap();
    assert_eq!(sol.len(), 1);
    assert_eq!(
        sol[0].first_ci,
        Some(false),
        "Sol judges the first head after Sol enrolled"
    );
    let family = db.family_outcome_records(None).unwrap();
    assert_eq!(family.len(), 1);
    assert_eq!(
        family[0].first_ci,
        Some(true),
        "the family judges the first head after its first launch"
    );
    assert_eq!(family[0].first_driver.as_deref(), Some("claude"));
}
