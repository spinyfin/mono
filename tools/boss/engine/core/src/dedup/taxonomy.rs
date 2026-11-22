//! Taxonomy-aware comparison set for notification dedup.
//!
//! When `notification_dedup_taxonomy` is on, the creation path and the
//! startup sweep populate
//! [`boss_protocol::DedupInput::existing_work_items`] from this module.
//! The query is same-product, non-terminal work items (tasks, chores,
//! revisions, projects). Overflow is logged, never silently dropped.
//!
//! ## R15 settlement
//!
//! Combined cap K = 48 (24 attentions + 24 work items). The work-item
//! side applies a most-recently-updated prefilter: each of the four
//! kinds keeps up to [`TAXONOMY_PER_KIND_CAP`] (6) newest rows, then
//! unused combined-cap slots fill from the remaining most-recently-updated
//! items so a product with many tasks and few chores still contributes
//! chores. Always applying MRU-N is a no-op under the cap and the right
//! prefilter when a product has 50+ open tasks (revisions multiply the
//! `tasks` table; a busy product regularly exceeds that threshold).
//!
//! Work-item ids use the engine's friendly forms: `T{n}` for every
//! `tasks`-table row (chores and revisions included) and `P{n}` for
//! projects. Those are the ids a `WorkItemDup` verdict can name.

use std::collections::BTreeMap;

use anyhow::Result;

use boss_protocol::{Project, ProjectStatus, Task, TaskKind, WorkItemBrief, short_id_label};

use crate::work::WorkDb;

/// Per-kind slice of the work-item comparison set (task / chore /
/// revision / project). Four kinds × 6 = [`TAXONOMY_WORK_ITEM_CAP`].
pub const TAXONOMY_PER_KIND_CAP: usize = 6;

/// Combined work-item cap (R15). Paired with a 24-attention cap for
/// K ≤ 48 total in the LLM call.
pub const TAXONOMY_WORK_ITEM_CAP: usize = 24;

/// First ~200 chars of a description go into [`WorkItemBrief`].
const DESCRIPTION_SNIPPET_CHARS: usize = 200;

/// Kind order used when taking the per-kind guarantee. Matches the
/// design's four work-item kinds.
const KIND_ORDER: [&str; 4] = ["task", "chore", "revision", "project"];

/// One work item considered for the comparison set, before capping.
#[derive(Debug, Clone)]
pub struct TaxonomyCandidate {
    pub kind: String,
    pub updated_at: i64,
    pub brief: WorkItemBrief,
}

/// Count of items of one kind that did not fit the cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxonomyOverflow {
    pub kind: String,
    pub dropped: usize,
}

/// Load the work-item side of [`boss_protocol::DedupInput`].
///
/// Returns an empty vec when `taxonomy_enabled` is false (the
/// `notification_dedup_taxonomy` sub-flag is off, or the parent
/// `notification_dedup` flag is off — callers pass the conjunction).
/// Shared by the creation path and the startup sweep.
pub fn load_existing_work_items(db: &WorkDb, product_id: &str, taxonomy_enabled: bool) -> Result<Vec<WorkItemBrief>> {
    let (briefs, overflows) = load_existing_work_items_with_caps(
        db,
        product_id,
        taxonomy_enabled,
        TAXONOMY_PER_KIND_CAP,
        TAXONOMY_WORK_ITEM_CAP,
    )?;
    for overflow in overflows {
        tracing::warn!(
            product_id,
            kind = %overflow.kind,
            dropped = overflow.dropped,
            per_kind_cap = TAXONOMY_PER_KIND_CAP,
            combined_cap = TAXONOMY_WORK_ITEM_CAP,
            "dedup taxonomy: work-item comparison set overflow; dropped least-recently-updated items"
        );
    }
    Ok(briefs)
}

/// Like [`load_existing_work_items`] with injectable caps so tests can
/// exercise overflow without inserting dozens of rows.
pub fn load_existing_work_items_with_caps(
    db: &WorkDb,
    product_id: &str,
    taxonomy_enabled: bool,
    per_kind_cap: usize,
    combined_cap: usize,
) -> Result<(Vec<WorkItemBrief>, Vec<TaxonomyOverflow>)> {
    if !taxonomy_enabled {
        return Ok((Vec::new(), Vec::new()));
    }
    let candidates = candidates_from_db(db, product_id)?;
    Ok(select_taxonomy_briefs(candidates, per_kind_cap, combined_cap))
}

/// Rank and cap a pre-built candidate list. Pure: no DB, no logging.
///
/// Each kind keeps up to `per_kind_cap` most-recently-updated items.
/// Unused `combined_cap` slots fill from the remaining items, newest
/// first. Leftovers become [`TaxonomyOverflow`] rows.
pub fn select_taxonomy_briefs(
    candidates: Vec<TaxonomyCandidate>,
    per_kind_cap: usize,
    combined_cap: usize,
) -> (Vec<WorkItemBrief>, Vec<TaxonomyOverflow>) {
    let mut by_kind: BTreeMap<String, Vec<TaxonomyCandidate>> = BTreeMap::new();
    for candidate in candidates {
        by_kind.entry(candidate.kind.clone()).or_default().push(candidate);
    }

    let mut selected: Vec<TaxonomyCandidate> = Vec::new();
    let mut remainder: Vec<TaxonomyCandidate> = Vec::new();

    for kind in KIND_ORDER {
        let Some(mut items) = by_kind.remove(kind) else {
            continue;
        };
        items.sort_by(mru_then_id);
        let take = per_kind_cap.min(items.len());
        selected.extend(items.drain(..take));
        remainder.extend(items);
    }
    for (_, mut items) in by_kind {
        items.sort_by(mru_then_id);
        remainder.extend(items);
    }

    remainder.sort_by(mru_then_id);
    let extra_take = combined_cap.saturating_sub(selected.len()).min(remainder.len());
    selected.extend(remainder.drain(..extra_take));
    selected.sort_by(mru_then_id);

    let mut dropped_by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for candidate in &remainder {
        *dropped_by_kind.entry(candidate.kind.clone()).or_default() += 1;
    }
    let overflows = dropped_by_kind
        .into_iter()
        .map(|(kind, dropped)| TaxonomyOverflow { kind, dropped })
        .collect();

    let briefs = selected.into_iter().map(|candidate| candidate.brief).collect();
    (briefs, overflows)
}

fn candidates_from_db(db: &WorkDb, product_id: &str) -> Result<Vec<TaxonomyCandidate>> {
    let mut out = Vec::new();
    for task in db.list_tasks(product_id, None, None, false)? {
        if task.status.is_terminal() {
            continue;
        }
        let Some(brief) = brief_from_task(&task) else {
            continue;
        };
        out.push(TaxonomyCandidate {
            kind: brief.kind.clone(),
            updated_at: parse_epoch(&task.updated_at),
            brief,
        });
    }
    for project in db.list_projects(product_id, None)? {
        if project_is_terminal(project.status) {
            continue;
        }
        let Some(brief) = brief_from_project(&project) else {
            continue;
        };
        out.push(TaxonomyCandidate {
            kind: brief.kind.clone(),
            updated_at: parse_epoch(&project.updated_at),
            brief,
        });
    }
    Ok(out)
}

fn brief_from_task(task: &Task) -> Option<WorkItemBrief> {
    let work_item_id = short_id_label(task.short_id)?;
    Some(WorkItemBrief {
        work_item_id,
        kind: taxonomy_kind(task).to_owned(),
        title: task.name.clone(),
        status: task.status.as_str().to_owned(),
        description_snippet: snippet(&task.description),
    })
}

fn brief_from_project(project: &Project) -> Option<WorkItemBrief> {
    let short_id = project.short_id?;
    Some(WorkItemBrief {
        work_item_id: format!("P{short_id}"),
        kind: "project".to_owned(),
        title: project.name.clone(),
        status: project.status.as_str().to_owned(),
        description_snippet: snippet(&project.description),
    })
}

/// Map a `tasks` row onto the design's four comparison-set kinds.
///
/// Chores (and unscoped followups, which `list_chores` treats as chores)
/// are `"chore"`. Revisions are `"revision"`. Every other non-terminal
/// `tasks` row — including design, investigation, project_task — is
/// `"task"`, because it is still scheduled work a human is tracking.
fn taxonomy_kind(task: &Task) -> &'static str {
    match task.kind {
        TaskKind::Revision => "revision",
        TaskKind::Chore => "chore",
        TaskKind::Followup if task.project_id.is_none() => "chore",
        TaskKind::Task
        | TaskKind::ProjectTask
        | TaskKind::Design
        | TaskKind::Investigation
        | TaskKind::DesignPostmortem
        | TaskKind::Followup => "task",
    }
}

fn project_is_terminal(status: ProjectStatus) -> bool {
    matches!(status, ProjectStatus::Done | ProjectStatus::Archived)
}

fn snippet(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= DESCRIPTION_SNIPPET_CHARS {
        return trimmed.to_owned();
    }
    let mut out: String = trimmed.chars().take(DESCRIPTION_SNIPPET_CHARS).collect();
    out.push('…');
    out
}

fn parse_epoch(raw: &str) -> i64 {
    raw.parse().unwrap_or(0)
}

fn mru_then_id(a: &TaxonomyCandidate, b: &TaxonomyCandidate) -> std::cmp::Ordering {
    b.updated_at
        .cmp(&a.updated_at)
        .then_with(|| a.brief.work_item_id.cmp(&b.brief.work_item_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        create_test_chore_manual, create_test_product, create_test_product_named, insert_host_capability, open_db,
    };
    use boss_protocol::{CreateProjectInput, CreateTaskInput, TaskStatus, WorkItemPatch};

    fn candidate(kind: &str, id: &str, updated_at: i64) -> TaxonomyCandidate {
        TaxonomyCandidate {
            kind: kind.to_owned(),
            updated_at,
            brief: WorkItemBrief {
                work_item_id: id.to_owned(),
                kind: kind.to_owned(),
                title: id.to_owned(),
                status: TaskStatus::Todo.as_str().to_owned(),
                description_snippet: String::new(),
            },
        }
    }

    #[test]
    fn flag_off_returns_empty_without_querying_shape() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        create_test_chore_manual(&db, product.id.clone(), "open chore");
        let (briefs, overflows) =
            load_existing_work_items_with_caps(&db, &product.id, false, TAXONOMY_PER_KIND_CAP, TAXONOMY_WORK_ITEM_CAP)
                .unwrap();
        assert!(briefs.is_empty());
        assert!(overflows.is_empty());
    }

    #[test]
    fn includes_non_terminal_task_chore_revision_and_project() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(&product.id)
                    .name("Taxonomy project")
                    .no_design_task(true)
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        insert_host_capability(&db, "local", "driver=claude", "auto");
        let task = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(&product.id)
                    .project_id(&project.id)
                    .name("open task")
                    .description("task body")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let chore = create_test_chore_manual(&db, product.id.clone(), "open chore");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET kind = 'revision' WHERE id = ?1",
                rusqlite::params![chore.id],
            )
            .unwrap();
        let chore = create_test_chore_manual(&db, product.id.clone(), "still a chore");

        let briefs = load_existing_work_items(&db, &product.id, true).unwrap();
        let kinds: Vec<_> = briefs.iter().map(|b| b.kind.as_str()).collect();
        assert!(kinds.contains(&"task"), "{kinds:?}");
        assert!(kinds.contains(&"chore"), "{kinds:?}");
        assert!(kinds.contains(&"revision"), "{kinds:?}");
        assert!(kinds.contains(&"project"), "{kinds:?}");

        let by_id: std::collections::HashMap<_, _> = briefs.into_iter().map(|b| (b.work_item_id.clone(), b)).collect();
        let task_id = short_id_label(task.short_id).unwrap();
        let chore_id = short_id_label(chore.short_id).unwrap();
        let project_id = format!("P{}", project.short_id.unwrap());
        assert_eq!(by_id[&task_id].title, "open task");
        assert_eq!(by_id[&task_id].status, "todo");
        assert_eq!(by_id[&task_id].description_snippet, "task body");
        assert_eq!(by_id[&chore_id].kind, "chore");
        assert_eq!(by_id[&project_id].kind, "project");
        assert_eq!(by_id[&project_id].status, "planned");
    }

    #[test]
    fn excludes_terminal_rows_and_other_products() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let other = create_test_product_named(&db, "Other");
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(&product.id)
                    .name("keep")
                    .no_design_task(true)
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        insert_host_capability(&db, "local", "driver=claude", "auto");
        let open = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(&product.id)
                    .project_id(&project.id)
                    .name("stays open")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let done = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(&product.id)
                    .project_id(&project.id)
                    .name("finished")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        db.update_work_item(&done.id, WorkItemPatch::builder().status("done").build())
            .unwrap();
        db.create_project(
            CreateProjectInput::builder()
                .product_id(&other.id)
                .name("other product project")
                .no_design_task(true)
                .autostart(false)
                .build(),
        )
        .unwrap();
        create_test_chore_manual(&db, other.id.clone(), "other product chore");

        let archived = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(&product.id)
                    .name("archived project")
                    .no_design_task(true)
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        db.update_work_item(&archived.id, WorkItemPatch::builder().status("archived").build())
            .unwrap();

        let briefs = load_existing_work_items(&db, &product.id, true).unwrap();
        let ids: Vec<_> = briefs.iter().map(|b| b.work_item_id.as_str()).collect();
        let open_id = short_id_label(open.short_id).unwrap();
        let done_id = short_id_label(done.short_id).unwrap();
        let archived_id = format!("P{}", archived.short_id.unwrap());
        assert!(ids.contains(&open_id.as_str()), "{ids:?}");
        assert!(!ids.contains(&done_id.as_str()), "{ids:?}");
        assert!(!ids.contains(&archived_id.as_str()), "{ids:?}");
        assert!(
            briefs
                .iter()
                .all(|b| b.title != "other product chore" && b.title != "other product project"),
            "{briefs:?}"
        );
    }

    #[test]
    fn description_snippet_truncates_long_text() {
        let long = "x".repeat(DESCRIPTION_SNIPPET_CHARS + 40);
        let got = snippet(&long);
        assert_eq!(got.chars().count(), DESCRIPTION_SNIPPET_CHARS + 1);
        assert!(got.ends_with('…'));
        assert_eq!(snippet("  short  "), "short");
    }

    #[test]
    fn per_kind_cap_then_fills_unused_combined_slots() {
        // 8 tasks + 1 chore, per-kind 2, combined 5:
        // guarantee 2 tasks + 1 chore, then fill 2 more tasks from remainder.
        let mut items = Vec::new();
        for i in 0..8 {
            items.push(candidate("task", &format!("T{i}"), 100 + i));
        }
        items.push(candidate("chore", "C1", 50));
        let (briefs, overflows) = select_taxonomy_briefs(items, 2, 5);
        assert_eq!(briefs.len(), 5);
        let tasks = briefs.iter().filter(|b| b.kind == "task").count();
        let chores = briefs.iter().filter(|b| b.kind == "chore").count();
        assert_eq!(tasks, 4);
        assert_eq!(chores, 1);
        // Newest tasks kept (updated_at 107..104 plus the two guaranteed 107, 106
        // then fill 105, 104).
        let task_ids: Vec<_> = briefs
            .iter()
            .filter(|b| b.kind == "task")
            .map(|b| b.work_item_id.as_str())
            .collect();
        assert!(task_ids.contains(&"T7"));
        assert!(task_ids.contains(&"T6"));
        assert!(task_ids.contains(&"T5"));
        assert!(task_ids.contains(&"T4"));
        assert_eq!(
            overflows,
            vec![TaxonomyOverflow {
                kind: "task".to_owned(),
                dropped: 4,
            }]
        );
    }

    #[test]
    fn under_cap_keeps_everything_and_reports_no_overflow() {
        let items = vec![
            candidate("task", "T1", 3),
            candidate("chore", "C1", 2),
            candidate("project", "P1", 1),
        ];
        let (briefs, overflows) = select_taxonomy_briefs(items, 6, 24);
        assert_eq!(briefs.len(), 3);
        assert!(overflows.is_empty());
    }

    #[test]
    fn db_overflow_uses_most_recently_updated() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(&product.id)
                    .name("cap project")
                    .no_design_task(true)
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        insert_host_capability(&db, "local", "driver=claude", "auto");
        let mut created = Vec::new();
        for i in 0..5 {
            let task = db
                .create_task(
                    CreateTaskInput::builder()
                        .product_id(&product.id)
                        .project_id(&project.id)
                        .name(format!("cap-task-{i}"))
                        .autostart(false)
                        .build(),
                )
                .unwrap();
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![(1000 + i).to_string(), task.id],
                )
                .unwrap();
            created.push(task);
        }
        let (briefs, overflows) = load_existing_work_items_with_caps(&db, &product.id, true, 1, 2).unwrap();
        // per-kind 1: 1 task + 1 project guaranteed = combined 2, rest overflow.
        assert_eq!(briefs.len(), 2);
        assert!(briefs.iter().any(|b| b.kind == "project"));
        let kept_task = briefs.iter().find(|b| b.kind == "task").unwrap();
        let newest = short_id_label(created[4].short_id).unwrap();
        assert_eq!(kept_task.work_item_id, newest);
        assert!(overflows.iter().any(|o| o.kind == "task" && o.dropped == 4));
    }

    #[test]
    fn unscoped_followup_is_classified_as_chore() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let followup = create_test_chore_manual(&db, product.id.clone(), "followup-shaped");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET kind = 'followup', project_id = NULL WHERE id = ?1",
                rusqlite::params![followup.id],
            )
            .unwrap();
        let briefs = load_existing_work_items(&db, &product.id, true).unwrap();
        let id = short_id_label(followup.short_id).unwrap();
        let brief = briefs.iter().find(|b| b.work_item_id == id).unwrap();
        assert_eq!(brief.kind, "chore");
    }
}
