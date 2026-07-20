use super::*;

use crate::automation_triage::{TriageContext, parse_triage_decision, render_triage_preamble};

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

/// Regression for the automation-duplicate-work incident (2026-07-13,
/// T2572/T2574): a second automation's triage run must see the first
/// automation's in-flight task in its preamble, and the
/// `automation: skip — duplicate of <ref>` convention it is told to use
/// must parse back cleanly.
#[test]
fn preamble_surfaces_seeded_open_task_and_skip_marker_round_trips() {
    let db = WorkDb::open(temp_db_path("layer0-preamble-overlap")).unwrap();
    let product = create_test_product_named(&db, "Layer0 Preamble Co");

    let auto_a = make_test_automation(&db, &product.id, "Dedup sweep A");
    let auto_b = make_test_automation(&db, &product.id, "Dedup sweep B");

    let open_task = make_automation_task(
        &db,
        &product.id,
        &auto_a.id,
        "dedup runner extract_pr_number",
        "in_review",
        Some("https://github.com/spinyfin/mono/pull/1963"),
    );

    let open_tasks = db.list_open_automation_tasks_for_product(&product.id).unwrap();
    let merged_tasks = Vec::new();
    let context = TriageContext::from_rows(open_tasks, merged_tasks);

    let preamble = render_triage_preamble(&auto_b, "Layer0 Preamble Co", &[], &context);

    let expected_ref = format!("T{}", open_task.short_id.expect("task should carry a short_id"));
    assert!(
        preamble.contains("Recently filed / in-flight automation work"),
        "preamble must contain the layer-0 context block heading"
    );
    assert!(
        preamble.contains(&expected_ref),
        "preamble must cite the seeded open task's short ref: {preamble}"
    );
    assert!(
        preamble.contains("dedup runner extract_pr_number"),
        "preamble must include the overlapping task's name"
    );
    assert!(
        preamble.contains("skip — duplicate of <ref>"),
        "preamble must teach the skip-duplicate convention"
    );

    // The agent following that instruction emits this marker; it must parse
    // back as a Skip decision whose reason cites the duplicate ref.
    let agent_message = format!("Nothing new here.\n\nautomation: skip — duplicate of {expected_ref}");
    let decision = parse_triage_decision(&agent_message);
    assert_eq!(
        decision,
        crate::automation_triage::TriageDecision::Skip(format!("duplicate of {expected_ref}"))
    );
}

#[test]
fn preamble_omits_context_block_when_nothing_in_flight() {
    let automation = boss_protocol::Automation::builder()
        .id("auto_empty")
        .short_id(1i64)
        .product_id("prod_1")
        .name("clippy sweep")
        .trigger(boss_protocol::AutomationTrigger::Schedule {
            cron: "0 14 * * *".to_owned(),
            timezone: "UTC".to_owned(),
        })
        .standing_instruction("fix any clippy warnings")
        .created_at("2026-01-01")
        .updated_at("2026-01-01")
        .build();
    let preamble = render_triage_preamble(&automation, "My Product", &[], &TriageContext::default());
    assert!(
        !preamble.contains("Recently filed / in-flight automation work"),
        "empty context must not render the block"
    );
}
