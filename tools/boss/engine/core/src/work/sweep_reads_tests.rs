use std::time::Instant;

use boss_protocol::{EffortLevel, WorkItem, WorkItemPatch};

use super::*;
use crate::test_support::{create_active_chore, create_product, create_spawned_execution, open_db};

fn set_effort(db: &WorkDb, id: &str, effort: EffortLevel) {
    db.update_work_item(
        id,
        WorkItemPatch {
            effort_level: Some(effort.as_str().to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
}

fn set_status(db: &WorkDb, id: &str, status: &str) {
    db.update_work_item(
        id,
        WorkItemPatch {
            status: Some(status.to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn effort_levels_batch_returns_only_classified_visible_rows() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let small = create_active_chore(&db, &product, "small");
    let unclassified = create_active_chore(&db, &product, "unclassified");
    let deleted = create_active_chore(&db, &product, "deleted");
    set_effort(&db, &small, EffortLevel::Small);
    set_effort(&db, &deleted, EffortLevel::Large);
    db.delete_work_item(&deleted).unwrap();

    let ids = vec![
        small.clone(),
        unclassified.clone(),
        deleted.clone(),
        "task_missing".to_owned(),
        "prod_not_a_task".to_owned(),
    ];
    let levels = db.task_effort_levels(&ids).unwrap();
    assert_eq!(levels.len(), 1, "{levels:?}");
    assert_eq!(levels.get(&small), Some(&EffortLevel::Small));

    assert_eq!(db.task_effort_level(&small).unwrap(), Some(EffortLevel::Small));
    assert_eq!(db.task_effort_level(&unclassified).unwrap(), None);
    assert_eq!(db.task_effort_level(&deleted).unwrap(), None);
    assert!(db.task_effort_levels(&[]).unwrap().is_empty());
}

#[test]
fn effort_level_matches_the_full_projection() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    for effort in EffortLevel::ALL {
        let id = create_active_chore(&db, &product, effort.as_str());
        set_effort(&db, &id, *effort);
        let WorkItem::Chore(full) = db.get_work_item(&id).unwrap() else {
            panic!("chore expected");
        };
        assert_eq!(db.task_effort_level(&id).unwrap(), full.effort_level);
    }
}

#[test]
fn closed_states_batch_mirrors_is_terminal_and_hides_tombstones() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let active = create_active_chore(&db, &product, "active");
    let done = create_active_chore(&db, &product, "done");
    let deleted = create_active_chore(&db, &product, "deleted");
    set_status(&db, &done, "done");
    db.delete_work_item(&deleted).unwrap();

    let closed = db
        .task_closed_states(&[active.clone(), done.clone(), deleted.clone(), "task_nope".to_owned()])
        .unwrap();
    assert_eq!(closed.get(&active), Some(&false));
    assert_eq!(closed.get(&done), Some(&true));
    assert!(!closed.contains_key(&deleted));
    assert!(!closed.contains_key("task_nope"));

    assert!(!db.task_closed_state(&active).unwrap());
    assert!(db.task_closed_state(&done).unwrap());
    let err = db.task_closed_state(&deleted).unwrap_err();
    assert!(err.to_string().contains("unknown task"), "{err:#}");
    // The sweep follows a closedness error with the row-missing probe; a
    // tombstoned row is "confirmed gone", the same answer the full read gave.
    assert!(db.work_item_row_missing(&deleted).unwrap());
}

#[test]
fn is_bound_work_item_closed_still_answers_for_tasks_and_rejects_unknown_rows() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let active = create_active_chore(&db, &product, "active");
    let done = create_active_chore(&db, &product, "done");
    set_status(&db, &done, "done");
    assert!(!db.is_bound_work_item_closed(&active).unwrap());
    assert!(db.is_bound_work_item_closed(&done).unwrap());
    assert!(!db.is_bound_work_item_closed(&product).unwrap());
    assert!(db.is_bound_work_item_closed("task_missing").is_err());
    assert!(db.is_bound_work_item_closed("bogus_id").is_err());
}

#[test]
fn pr_url_and_wait_label_read_exactly_the_card_fields() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let id = create_active_chore(&db, &product, "name me");
    assert_eq!(db.task_pr_url(&id).unwrap(), None);
    db.update_work_item(
        &id,
        WorkItemPatch {
            pr_url: Some("https://github.com/o/r/pull/42".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        db.task_pr_url(&id).unwrap().as_deref(),
        Some("https://github.com/o/r/pull/42")
    );
    assert!(db.task_pr_url("task_missing").is_err());

    let WorkItem::Chore(full) = db.get_work_item(&id).unwrap() else {
        panic!("chore expected");
    };
    let label = db.task_wait_label(&id).unwrap().expect("visible row");
    assert_eq!(label.short_id, full.short_id);
    assert_eq!(label.short_label(), full.short_label());
    assert_eq!(label.name, full.name);
    assert_eq!(label.pr_url, full.pr_url);
    assert_eq!(db.task_wait_label("task_missing").unwrap(), None);

    db.delete_work_item(&id).unwrap();
    assert!(db.task_pr_url(&id).is_err());
    assert_eq!(db.task_wait_label(&id).unwrap(), None);
}

#[test]
fn terminal_execution_refs_cover_recent_local_pids_only() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let live_item = create_active_chore(&db, &product, "live");
    let dead_item = create_active_chore(&db, &product, "dead");
    let _live = create_spawned_execution(&db, &live_item, 4242);
    let dead = create_spawned_execution(&db, &dead_item, 4343);
    db.mark_execution_orphaned(&dead, "test").unwrap();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();

    let refs = db
        .list_recent_terminal_execution_refs_with_local_shell_pid(3600, now)
        .unwrap();
    assert_eq!(refs.len(), 1, "{refs:?}");
    assert_eq!(refs[0].id, dead);
    assert_eq!(refs[0].work_item_id, dead_item);
    assert_eq!(refs[0].status, ExecutionStatus::Orphaned);

    // The age bound excludes history older than the window.
    let refs = db
        .list_recent_terminal_execution_refs_with_local_shell_pid(0, now + 10_000)
        .unwrap();
    assert!(refs.is_empty(), "{refs:?}");
}

/// Copy one `tasks` / `work_executions` row into a new row with a few
/// columns overridden — the fixture builder for the baseline below. Going
/// through the create/update APIs for hundreds of rows is itself quadratic
/// (every update republishes the full projection), which would make the
/// fixture, not the reads, the thing being timed.
fn clone_row(conn: &Connection, table: &str, template_id: &str, overrides: &[(&str, &str)]) {
    let columns: Vec<String> = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let select: Vec<String> = columns
        .iter()
        .map(|column| match overrides.iter().find(|(name, _)| name == column) {
            Some((_, value)) => format!("'{}'", value.replace('\'', "''")),
            None => column.clone(),
        })
        .collect();
    conn.execute(
        &format!(
            "INSERT INTO {table} ({}) SELECT {} FROM {table} WHERE id = ?1",
            columns.join(", "),
            select.join(", ")
        ),
        params![template_id],
    )
    .unwrap();
}

/// Not a correctness test: the before/after cost evidence for the sweep
/// reads, on a synthetic DB shaped like a busy engine (review cycles with
/// revisions and verdicts, which is what makes the full projection's
/// badge query expensive). It asserts only that the narrow reads agree
/// with the full projection; the timings are printed for the PR body and
/// never asserted, so the test cannot flake on a slow host.
#[test]
fn sweep_cost_baseline_full_projection_vs_narrow_reads() {
    const ROOTS: usize = 1500;
    const REVISIONS_PER_ROOT: usize = 3;
    /// The full projection is slow enough on this fixture that timing it
    /// over every root would push the test past its shard budget; a sample
    /// gives the per-id figure.
    const FULL_SAMPLE: usize = 20;
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let template = create_active_chore(&db, &product, "template");
    set_effort(&db, &template, EffortLevel::Medium);
    let template_execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(template.clone()).build())
        .unwrap();
    let fixture_started = Instant::now();
    let mut roots = Vec::with_capacity(ROOTS);
    {
        let conn = db.connect().unwrap();
        for i in 0..ROOTS {
            let root = format!("task_root{i:04}");
            let pr_url = format!("https://github.com/o/r/pull/{i}");
            let head = format!("head{i:04}");
            clone_row(
                &conn,
                "tasks",
                &template,
                &[
                    ("id", &root),
                    ("name", &format!("root {i}")),
                    ("short_id", &format!("{}", 1000 + i)),
                    ("status", "in_review"),
                    ("pr_url", &pr_url),
                    ("pr_head_sha", &head),
                ],
            );
            let mut parent = root.clone();
            for r in 0..REVISIONS_PER_ROOT {
                let revision = format!("task_root{i:04}rev{r}");
                clone_row(
                    &conn,
                    "tasks",
                    &template,
                    &[
                        ("id", &revision),
                        ("name", &format!("root {i} revision {r}")),
                        ("short_id", &format!("{}", 100_000 + i * 10 + r)),
                        ("kind", "revision"),
                        ("parent_task_id", &parent),
                        ("status", "done"),
                    ],
                );
                let execution = format!("exec_root{i:04}rev{r}");
                clone_row(
                    &conn,
                    "work_executions",
                    &template_execution.id,
                    &[("id", &execution), ("work_item_id", &revision), ("status", "completed")],
                );
                conn.execute(
                    "INSERT INTO pr_review_verdicts (id, execution_id, work_item_id, head_sha, findings_count,
                        revision_warranted, gate_outcome, revision_task_id, created_at)
                     VALUES (?1, ?2, ?3, ?4, 2, 1, 'completed_with_findings', ?5, ?6)",
                    params![
                        format!("prv_{i}_{r}"),
                        execution,
                        root,
                        head,
                        revision,
                        format!("{}", 1_700_000_000 + i * 10 + r),
                    ],
                )
                .unwrap();
                parent = revision;
            }
            roots.push(root);
        }
    }
    eprintln!("sweep_cost_baseline: fixture built in {:?}", fixture_started.elapsed());

    // Full projection, one read per id: what every sweep did per slot.
    let full_started = Instant::now();
    let mut full_efforts = HashMap::new();
    let mut full_closed = HashMap::new();
    for id in &roots[..FULL_SAMPLE] {
        match db.get_work_item(id).unwrap() {
            WorkItem::Task(task) | WorkItem::Chore(task) => {
                if let Some(effort) = task.effort_level {
                    full_efforts.insert(id.clone(), effort);
                }
                full_closed.insert(id.clone(), task.status.is_terminal());
            }
            _ => panic!("task expected"),
        }
    }
    let full_elapsed = full_started.elapsed();

    // Narrow batched reads: one query per pass for each field.
    let narrow_started = Instant::now();
    let efforts = db.task_effort_levels(&roots).unwrap();
    let closed = db.task_closed_states(&roots).unwrap();
    let narrow_elapsed = narrow_started.elapsed();

    // Narrow single-row reads: the per-slot shape the timer path keeps.
    let single_started = Instant::now();
    for id in &roots {
        db.task_effort_level(id).unwrap();
        db.task_closed_state(id).unwrap();
    }
    let single_elapsed = single_started.elapsed();

    assert_eq!(efforts.len(), ROOTS);
    assert_eq!(closed.len(), ROOTS);
    for id in &roots[..FULL_SAMPLE] {
        assert_eq!(efforts.get(id), full_efforts.get(id), "{id}");
        assert_eq!(closed.get(id), full_closed.get(id), "{id}");
    }
    eprintln!(
        "sweep_cost_baseline: {ROOTS} in-review roots x {REVISIONS_PER_ROOT} revisions + verdicts\n\
         \x20 full get_work_item per id ({FULL_SAMPLE} sampled): {:?} total, {:?} per id\n\
         \x20 narrow batched (effort + closed, all {ROOTS}): {:?} total\n\
         \x20 narrow single-row per id (effort + closed, all {ROOTS}): {:?} total, {:?} per id",
        full_elapsed,
        full_elapsed / FULL_SAMPLE as u32,
        narrow_elapsed,
        single_elapsed,
        single_elapsed / ROOTS as u32,
    );
}

#[test]
fn corrupt_effort_does_not_disable_other_slots() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let valid = create_active_chore(&db, &product, "valid");
    let corrupt = create_active_chore(&db, &product, "corrupt");
    set_effort(&db, &valid, EffortLevel::Small);
    db.connect().unwrap().execute("UPDATE tasks SET effort_level = 'invalid' WHERE id = ?1", params![corrupt]).unwrap();
    let efforts = db.task_effort_levels(&[valid.clone(), corrupt.clone()]).unwrap();
    assert_eq!(efforts.get(&valid), Some(&EffortLevel::Small));
    assert!(!efforts.contains_key(&corrupt));
}
