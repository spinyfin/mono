use super::*;

// ── layer-0 context injection: cross-automation open/merged task queries ──

fn make_test_automation(db: &WorkDb, product_id: &str, name: &str) -> boss_protocol::Automation {
    db.create_automation(CreateAutomationInput {
        product_id: product_id.to_owned(),
        name: name.to_owned(),
        repo_remote_url: None,
        trigger: AutomationTrigger::Schedule {
            cron: "0 14 * * 1-5".to_owned(),
            timezone: "UTC".to_owned(),
        },
        standing_instruction: "dedup runner extract_pr_number".to_owned(),
        open_task_limit: 5,
        catch_up_window_secs: None,
        enabled: true,
        created_via: None,
    })
    .unwrap()
}

/// Create a chore, stamp it as produced by `automation_id`, and set its
/// status (and optionally `pr_url`) directly via SQL — mirrors the pattern
/// the existing `count_open_tasks_for_automation` tests use.
fn make_automation_task(
    db: &WorkDb,
    product_id: &str,
    automation_id: &str,
    name: &str,
    status: &str,
    pr_url: Option<&str>,
) -> boss_protocol::Task {
    let task = create_test_chore_manual(db, product_id, name);
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET source_automation_id = ?1, status = ?2, pr_url = ?3 WHERE id = ?4",
        rusqlite::params![automation_id, status, pr_url, task.id],
    )
    .unwrap();
    drop(conn);
    match db.get_work_item(&task.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected a task/chore work item, got {other:?}"),
    }
}

#[test]
fn list_open_automation_tasks_for_product_spans_automations() {
    let db = WorkDb::open(temp_db_path("layer0-open-cross-auto")).unwrap();
    let product = create_test_product_named(&db, "Layer0 Co");

    let auto_a = make_test_automation(&db, &product.id, "Dedup sweep A");
    let auto_b = make_test_automation(&db, &product.id, "Dedup sweep B");

    let open_a = make_automation_task(
        &db,
        &product.id,
        &auto_a.id,
        "dedup runner extract_pr_number",
        "in_review",
        Some("https://github.com/spinyfin/mono/pull/1963"),
    );
    let _done_a = make_automation_task(&db, &product.id, &auto_a.id, "already merged", "done", None);
    let open_b = make_automation_task(&db, &product.id, &auto_b.id, "another open one", "todo", None);

    let open = db.list_open_automation_tasks_for_product(&product.id).unwrap();
    let ids: Vec<&str> = open.iter().map(|t| t.id.as_str()).collect();

    assert_eq!(open.len(), 2, "both automations' open tasks surfaced, done excluded");
    assert!(ids.contains(&open_a.id.as_str()));
    assert!(ids.contains(&open_b.id.as_str()));
}

#[test]
fn list_recently_completed_automation_tasks_filters_by_window_and_pr() {
    let db = WorkDb::open(temp_db_path("layer0-merged-window")).unwrap();
    let product = create_test_product_named(&db, "Layer0 Merged Co");
    let auto = make_test_automation(&db, &product.id, "Dedup sweep");

    let merged = make_automation_task(
        &db,
        &product.id,
        &auto.id,
        "dedup runner extract_pr_number",
        "done",
        Some("https://github.com/spinyfin/mono/pull/1963"),
    );
    // Done but never had a PR — must not appear (nothing to point the agent at).
    let _done_no_pr = make_automation_task(&db, &product.id, &auto.id, "done without pr", "done", None);

    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let within_window = db
        .list_recently_completed_automation_tasks_for_product(&product.id, now - 3600)
        .unwrap();
    assert_eq!(within_window.len(), 1);
    assert_eq!(within_window[0].id, merged.id);

    // A window starting after the task's updated_at excludes it.
    let outside_window = db
        .list_recently_completed_automation_tasks_for_product(&product.id, now + 3600)
        .unwrap();
    assert!(
        outside_window.is_empty(),
        "window starting in the future excludes everything"
    );
}

// ── layer-0: preamble renders the block and the skip marker round-trips ──
