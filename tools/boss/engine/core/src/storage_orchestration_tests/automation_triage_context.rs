//! Automation triage's layer-0 context: a second automation's preamble must
//! show the first automation's in-flight task, and the skip marker the
//! preamble teaches must parse back.

use boss_protocol::{AutomationTrigger, CreateAutomationInput, WorkItem};

use super::raw;
use crate::automation_triage::{TriageContext, render_triage_preamble, resolve_triage_decision};
use crate::test_support::{create_test_chore_manual, create_test_product_named, open_db};
use crate::work::WorkDb;

/// Stand-in for the engine-owned decision artifact path in preamble tests.
const ARTIFACT_PATH: &str = "/tmp/boss-worker-output/exec_1.triage.json";

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
    raw(db)
        .execute(
            "UPDATE tasks SET source_automation_id = ?1, status = ?2, pr_url = ?3 WHERE id = ?4",
            rusqlite::params![automation_id, status, pr_url, task.id],
        )
        .unwrap();
    match db.get_work_item(&task.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected a task/chore work item, got {other:?}"),
    }
}

/// Regression for the automation-duplicate-work incident (2026-07-13): a
/// second automation's triage run must see the first
/// automation's in-flight task in its preamble, and the
/// `automation: skip — duplicate of <ref>` convention it is told to use
/// must parse back cleanly.
#[test]
fn preamble_surfaces_seeded_open_task_and_skip_marker_round_trips() {
    let (_dir, db) = open_db();
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

    let preamble = render_triage_preamble(&auto_b, "Layer0 Preamble Co", &[], &context, ARTIFACT_PATH, false);

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
    let decision = resolve_triage_decision(
        &crate::driver::ClaudeDriver,
        // No artifact on disk: this exercises the driver's marker fallback.
        std::path::Path::new("/nonexistent-boss-structured-output"),
        "exec_absent",
        Some(&agent_message),
    );
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
    let preamble = render_triage_preamble(
        &automation,
        "My Product",
        &[],
        &TriageContext::default(),
        ARTIFACT_PATH,
        false,
    );
    assert!(
        !preamble.contains("Recently filed / in-flight automation work"),
        "empty context must not render the block"
    );
}
