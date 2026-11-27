//! How a cost snapshot's context-shape figures land on the run row: assigned
//! verbatim when observed (so a figure can return to NULL), untouched when the
//! snapshot carries none of them.

use super::*;

use boss_engine_storage_values::RunCostSnapshot;

type Shape = (Option<i64>, Option<i64>, Option<i64>, Option<i64>);

fn running_execution() -> (WorkDb, String, String) {
    let db = WorkDb::open(temp_db_path("run-context-shape")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "context shape");
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    let (_, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();
    (db, execution.id, run.id)
}

fn stored(db: &WorkDb, run_id: &str) -> Shape {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT first_call_context_tokens, first_call_cache_write_tokens, peak_context_tokens, compactions
             FROM work_runs WHERE id = ?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

fn shape(first: Option<i64>, written: Option<i64>, peak: Option<i64>, compactions: Option<i64>) -> RunCostSnapshot {
    RunCostSnapshot {
        first_call_context_tokens: first,
        first_call_cache_write_tokens: written,
        peak_context_tokens: peak,
        compactions,
        ..Default::default()
    }
}

#[test]
fn a_new_run_has_no_context_shape_rather_than_zeros() {
    let (db, _, run_id) = running_execution();
    assert_eq!(stored(&db, &run_id), (None, None, None, None));
}

#[test]
fn a_snapshot_writes_every_context_shape_column() {
    let (db, execution_id, run_id) = running_execution();
    db.set_run_cost_snapshot(&execution_id, shape(Some(20_000), Some(19_800), Some(170_000), Some(2)))
        .unwrap();
    assert_eq!(
        stored(&db, &run_id),
        (Some(20_000), Some(19_800), Some(170_000), Some(2))
    );
}

#[test]
fn a_snapshot_that_observed_no_context_figure_leaves_the_stored_ones_alone() {
    let (db, execution_id, run_id) = running_execution();
    db.set_run_cost_snapshot(&execution_id, shape(Some(20_000), Some(19_800), Some(170_000), Some(1)))
        .unwrap();
    // A hook that only records a usage reason, or a duration, carries none of
    // the four figures and must not erase them.
    db.set_run_cost_snapshot(
        &execution_id,
        RunCostSnapshot {
            usage_unavailable: Some("one response omitted usage".into()),
            agent_active_ms: Some(5),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        stored(&db, &run_id),
        (Some(20_000), Some(19_800), Some(170_000), Some(1))
    );
}

#[test]
fn a_figure_that_becomes_unknown_replaces_the_stale_one_instead_of_surviving() {
    let (db, execution_id, run_id) = running_execution();
    db.set_run_cost_snapshot(&execution_id, shape(Some(1_000), None, Some(1_000), None))
        .unwrap();
    // A later call whose prompt size is unreadable makes the peak unknowable.
    // Keeping the earlier peak would present a lower bound as the peak.
    db.set_run_cost_snapshot(&execution_id, shape(Some(1_000), None, None, None))
        .unwrap();
    assert_eq!(stored(&db, &run_id), (Some(1_000), None, None, None));
}

#[test]
fn a_figure_rises_and_a_compaction_count_grows_across_incremental_snapshots() {
    let (db, execution_id, run_id) = running_execution();
    for (peak, compactions) in [(1_000, 0), (180_000, 0), (180_000, 1), (180_000, 2)] {
        db.set_run_cost_snapshot(
            &execution_id,
            shape(Some(900), Some(800), Some(peak), Some(compactions)),
        )
        .unwrap();
    }
    assert_eq!(stored(&db, &run_id), (Some(900), Some(800), Some(180_000), Some(2)));
}

#[test]
fn a_full_replacement_without_figures_clears_what_the_old_transcript_left() {
    let (db, execution_id, run_id) = running_execution();
    db.set_run_cost_snapshot(&execution_id, shape(Some(900), Some(800), Some(5_000), Some(3)))
        .unwrap();
    db.set_run_cost_snapshot(
        &execution_id,
        RunCostSnapshot {
            full_replacement: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(stored(&db, &run_id), (None, None, None, None));
}

#[test]
fn a_recorded_zero_compactions_is_stored_as_zero_and_is_not_null() {
    let (db, execution_id, run_id) = running_execution();
    db.set_run_cost_snapshot(&execution_id, shape(Some(900), None, Some(900), Some(0)))
        .unwrap();
    assert_eq!(stored(&db, &run_id).3, Some(0));
}
