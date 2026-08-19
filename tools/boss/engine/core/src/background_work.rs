//! Global background-work snapshot — design "Background task visibility:
//! only long-lived, headless engine work enters the toolbar badge"
//! (`tools/boss/docs/designs/background-task-visibility-toolbar-affordance-for-engine-background-work.md`).
//!
//! Assembles the two v1 sources into [`BackgroundWorkItem`] rows: project
//! planner runs older than [`PLANNER_BACKGROUND_MIN_AGE_SECS`], and
//! conflict attempts whose mechanical rung is executing right now in this
//! process. Neither source gets a new table, tokio task, or activity
//! registry — this is a read-time projection over rows [`crate::populator`]
//! and [`crate::conflict_ladder`] already maintain, called from the
//! `ListEngineAttempts` handler when a caller opts in.
//!
//! The conflict source's eligibility is two-part, matching the design's
//! "Eligibility invariant": a candidate must carry a non-`NULL`
//! `mechanical_rung_in_flight` (the positive inclusion and phase source),
//! *and* its stamped `cube_lease_id`/`cube_workspace_id` must still be
//! held by this process per [`crate::ladder_lease_registry::snapshot`].
//! The marker alone is not enough — it can survive a crash between the
//! rung concluding and the marker clear landing, or the registry can
//! unregister a lease slightly ahead of a failed clear write. The lease
//! check is a stale-marker veto only: lease membership by itself never
//! creates an item, and a candidate with no marker is never included no
//! matter what the registry holds.

use boss_protocol::{BackgroundWorkItem, BackgroundWorkKind};

use crate::work::WorkDb;

/// Anti-flicker gate for the planner source: a `running` row younger than
/// this is omitted even though it is genuinely live, because the common
/// case finishes before a human could act on the badge. Engine-owned and
/// easy to change — see the design's "Risks / open questions".
pub const PLANNER_BACKGROUND_MIN_AGE_SECS: i64 = 15;

const TITLE_PROJECT_PLANNER: &str = "Project planner";
const TITLE_CONFLICT_REMEDIATION: &str = "Conflict remediation";
const PHASE_DETERMINISTIC_RESOLUTION: &str = "Applying deterministic resolution";

/// Mechanical rung value stamped for the deterministic-resolver phase
/// (`crate::conflict_ladder`'s rung 0). Any other stamped value (today,
/// only rung 1 — the engine-direct rebase) renders as the "Rebasing
/// <work item>" phase.
const MECHANICAL_RUNG_DETERMINISTIC: i64 = 0;

/// Build the current global background-work snapshot: every eligible
/// project-planner run plus every eligible mechanical conflict rung,
/// merged into `BackgroundWorkItem`'s stable engine order — known starts
/// oldest first, then items with no start ordered by `source_id`.
///
/// Read-only: this never writes to `planner_runs` or
/// `conflict_resolutions`, and only reads
/// [`crate::ladder_lease_registry`], never mutates it.
pub fn snapshot(work_db: &WorkDb) -> anyhow::Result<Vec<BackgroundWorkItem>> {
    let mut items: Vec<BackgroundWorkItem> = Vec::new();
    items.extend(planner_items(work_db)?);
    items.extend(conflict_remediation_items(work_db)?);
    items.sort_by(order_key);
    Ok(items)
}

fn planner_items(work_db: &WorkDb) -> anyhow::Result<Vec<BackgroundWorkItem>> {
    Ok(work_db
        .list_running_planner_runs_older_than(PLANNER_BACKGROUND_MIN_AGE_SECS)?
        .into_iter()
        .map(|run| BackgroundWorkItem {
            id: format!("project_planner:{}", run.id),
            kind: BackgroundWorkKind::ProjectPlanner,
            phase: format!("Planning {}", run.project_name),
            product_id: run.product_id,
            source_id: run.id,
            title: TITLE_PROJECT_PLANNER.to_owned(),
            project_id: Some(run.project_id),
            started_at: Some(run.created_at),
            work_item_id: None,
        })
        .collect())
}

fn conflict_remediation_items(work_db: &WorkDb) -> anyhow::Result<Vec<BackgroundWorkItem>> {
    // The same global open-attempt baseline Activity already reads —
    // reused here purely as the candidate set, per the design's "reuse
    // the global pending/running conflict query as a candidate set."
    let candidates =
        work_db.list_conflict_resolutions(None, &["pending".to_owned(), "running".to_owned()], None, None)?;
    let live_leases = crate::ladder_lease_registry::snapshot();
    let mut out = Vec::new();
    for c in candidates {
        let Some(rung) = c.mechanical_rung_in_flight else {
            continue;
        };
        let (Some(lease_id), Some(workspace_id)) = (c.cube_lease_id.as_deref(), c.cube_workspace_id.as_deref()) else {
            continue;
        };
        let lease_still_live = live_leases.iter().any(|(l, w)| l == lease_id && w == workspace_id);
        if !lease_still_live {
            continue;
        }
        let phase = if rung == MECHANICAL_RUNG_DETERMINISTIC {
            PHASE_DETERMINISTIC_RESOLUTION.to_owned()
        } else {
            format!("Rebasing {}", c.work_item_id)
        };
        out.push(BackgroundWorkItem {
            id: format!("conflict_remediation:{}", c.id),
            kind: BackgroundWorkKind::ConflictRemediation,
            phase,
            product_id: c.product_id,
            source_id: c.id,
            title: TITLE_CONFLICT_REMEDIATION.to_owned(),
            project_id: None,
            started_at: None,
            work_item_id: Some(c.work_item_id),
        });
    }
    Ok(out)
}

/// Stable ordering contract: items with a known start sort oldest-first
/// (ascending `started_at`), then every item with no start sorts after
/// them, ordered by `source_id`.
fn order_key(a: &BackgroundWorkItem, b: &BackgroundWorkItem) -> std::cmp::Ordering {
    match (&a.started_at, &b.started_at) {
        (Some(sa), Some(sb)) => sa.cmp(sb).then_with(|| a.source_id.cmp(&b.source_id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.source_id.cmp(&b.source_id),
    }
}

#[cfg(test)]
mod tests {
    use boss_protocol::CreateProjectInput;

    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product_with_repo};
    use crate::work::{ClaimPlannerRunInput, ConflictResolutionInsertInput};

    fn open() -> WorkDb {
        WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
    }

    fn product_and_project(db: &WorkDb, project_name: &str) -> (String, String) {
        let product = create_test_product_with_repo(db, "Test", Some("git@github.com:test/test.git"));
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name(project_name)
                    .goal("build it")
                    .build(),
            )
            .unwrap();
        (product.id, project.id)
    }

    fn backdate_planner_run(db: &WorkDb, run_id: &str, secs_ago: i64) {
        let now_secs: i64 = crate::work::now_string().parse().unwrap();
        let new_ts = (now_secs - secs_ago).to_string();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE planner_runs SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![run_id, new_ts],
        )
        .unwrap();
    }

    fn seed_task(db: &WorkDb, product_id: &str, name: &str) -> String {
        create_test_chore_manual(db, product_id.to_owned(), name.to_owned()).id
    }

    #[test]
    fn snapshot_is_empty_with_no_sources() {
        let db = open();
        assert!(snapshot(&db).unwrap().is_empty());
    }

    #[test]
    fn snapshot_includes_a_stale_running_planner_run() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db, "Alpha");
        let run = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "merge_trigger",
            })
            .unwrap()
            .unwrap();
        backdate_planner_run(&db, &run.id, 20);
        let items = snapshot(&db).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, BackgroundWorkKind::ProjectPlanner);
        assert_eq!(items[0].source_id, run.id);
        assert_eq!(items[0].id, format!("project_planner:{}", run.id));
        assert_eq!(items[0].title, TITLE_PROJECT_PLANNER);
        assert_eq!(items[0].phase, "Planning Alpha");
        assert_eq!(items[0].project_id.as_deref(), Some(project_id.as_str()));
        assert!(items[0].started_at.is_some());
        assert!(items[0].work_item_id.is_none());
    }

    #[test]
    fn snapshot_excludes_a_fresh_running_planner_run() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db, "Alpha");
        db.claim_planner_run(ClaimPlannerRunInput {
            project_id: &project_id,
            product_id: &product_id,
            design_task_id: None,
            caller: "merge_trigger",
        })
        .unwrap();
        assert!(snapshot(&db).unwrap().is_empty());
    }

    #[test]
    fn snapshot_excludes_an_open_conflict_attempt_with_no_mechanical_marker() {
        let db = open();
        let (product_id, _project_id) = product_and_project(&db, "Alpha");
        let work_item_id = seed_task(&db, &product_id, "Chore");
        db.insert_conflict_resolution(
            ConflictResolutionInsertInput::builder()
                .product_id(product_id)
                .work_item_id(work_item_id)
                .pr_url("https://github.com/test/test/pull/1")
                .pr_number(1)
                .head_branch("feature")
                .base_branch("main")
                .base_sha_at_trigger("abc")
                .head_sha_before("def")
                .build(),
        )
        .unwrap();
        assert!(
            snapshot(&db).unwrap().is_empty(),
            "an open attempt without a mechanical rung is not executing work"
        );
    }

    #[test]
    fn snapshot_includes_a_mechanical_rung_with_a_live_lease() {
        let db = open();
        let (product_id, _project_id) = product_and_project(&db, "Alpha");
        let work_item_id = seed_task(&db, &product_id, "Chore");
        let attempt = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(product_id)
                    .work_item_id(work_item_id.clone())
                    .pr_url("https://github.com/test/test/pull/1")
                    .pr_number(1)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("abc")
                    .head_sha_before("def")
                    .build(),
            )
            .unwrap()
            .unwrap();
        db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 1, "bgwork_test_lease_1", "bgwork_test_ws_1")
            .unwrap();
        let lease = crate::coordinator::CubeWorkspaceLease {
            lease_id: "bgwork_test_lease_1".to_owned(),
            workspace_id: "bgwork_test_ws_1".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/ws-1"),
            dirty_verified: None,
        };
        crate::ladder_lease_registry::register(&lease);
        let items = snapshot(&db).unwrap();
        crate::ladder_lease_registry::unregister("bgwork_test_lease_1");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, BackgroundWorkKind::ConflictRemediation);
        assert_eq!(items[0].source_id, attempt.id);
        assert_eq!(items[0].id, format!("conflict_remediation:{}", attempt.id));
        assert_eq!(items[0].title, TITLE_CONFLICT_REMEDIATION);
        assert_eq!(items[0].phase, format!("Rebasing {work_item_id}"));
        assert_eq!(items[0].work_item_id.as_deref(), Some(work_item_id.as_str()));
        assert!(items[0].project_id.is_none());
        assert!(items[0].started_at.is_none());
    }

    #[test]
    fn snapshot_labels_rung_zero_as_deterministic_resolution() {
        let db = open();
        let (product_id, _project_id) = product_and_project(&db, "Alpha");
        let work_item_id = seed_task(&db, &product_id, "Chore");
        let attempt = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(product_id)
                    .work_item_id(work_item_id)
                    .pr_url("https://github.com/test/test/pull/1")
                    .pr_number(1)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("abc")
                    .head_sha_before("def")
                    .build(),
            )
            .unwrap()
            .unwrap();
        db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 0, "bgwork_test_lease_2", "bgwork_test_ws_2")
            .unwrap();
        let lease = crate::coordinator::CubeWorkspaceLease {
            lease_id: "bgwork_test_lease_2".to_owned(),
            workspace_id: "bgwork_test_ws_2".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/ws-2"),
            dirty_verified: None,
        };
        crate::ladder_lease_registry::register(&lease);
        let items = snapshot(&db).unwrap();
        crate::ladder_lease_registry::unregister("bgwork_test_lease_2");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].phase, PHASE_DETERMINISTIC_RESOLUTION);
    }

    #[test]
    fn snapshot_vetoes_a_marker_whose_lease_is_not_live() {
        let db = open();
        let (product_id, _project_id) = product_and_project(&db, "Alpha");
        let work_item_id = seed_task(&db, &product_id, "Chore");
        let attempt = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(product_id)
                    .work_item_id(work_item_id)
                    .pr_url("https://github.com/test/test/pull/1")
                    .pr_number(1)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("abc")
                    .head_sha_before("def")
                    .build(),
            )
            .unwrap()
            .unwrap();
        // Stamp the marker but never register the lease — simulates the
        // marker surviving a crash (or a failed clear after unregister)
        // with no process-live lease behind it.
        db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 1, "bgwork_test_lease_3", "bgwork_test_ws_3")
            .unwrap();
        assert!(
            snapshot(&db).unwrap().is_empty(),
            "a marker with no live lease must be vetoed as stale"
        );
    }

    #[test]
    fn snapshot_vetoes_a_mismatched_lease_workspace_pair() {
        let db = open();
        let (product_id, _project_id) = product_and_project(&db, "Alpha");
        let work_item_id = seed_task(&db, &product_id, "Chore");
        let attempt = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(product_id)
                    .work_item_id(work_item_id)
                    .pr_url("https://github.com/test/test/pull/1")
                    .pr_number(1)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("abc")
                    .head_sha_before("def")
                    .build(),
            )
            .unwrap()
            .unwrap();
        db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 1, "bgwork_test_lease_4", "bgwork_test_ws_4")
            .unwrap();
        // The registry holds the same lease id, but paired with a
        // different workspace than the row stamped — must not count as a
        // match.
        let lease = crate::coordinator::CubeWorkspaceLease {
            lease_id: "bgwork_test_lease_4".to_owned(),
            workspace_id: "ws-other".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/ws-other"),
            dirty_verified: None,
        };
        crate::ladder_lease_registry::register(&lease);
        let result = snapshot(&db);
        crate::ladder_lease_registry::unregister("bgwork_test_lease_4");
        assert!(
            result.unwrap().is_empty(),
            "a lease/workspace mismatch must not be treated as a live match"
        );
    }

    #[test]
    fn snapshot_vetoes_after_unregister_even_when_the_marker_clear_failed() {
        let db = open();
        let (product_id, _project_id) = product_and_project(&db, "Alpha");
        let work_item_id = seed_task(&db, &product_id, "Chore");
        let attempt = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(product_id)
                    .work_item_id(work_item_id)
                    .pr_url("https://github.com/test/test/pull/1")
                    .pr_number(1)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("abc")
                    .head_sha_before("def")
                    .build(),
            )
            .unwrap()
            .unwrap();
        db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 1, "bgwork_test_lease_5", "bgwork_test_ws_5")
            .unwrap();
        let lease = crate::coordinator::CubeWorkspaceLease {
            lease_id: "bgwork_test_lease_5".to_owned(),
            workspace_id: "bgwork_test_ws_5".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/ws-5"),
            dirty_verified: None,
        };
        crate::ladder_lease_registry::register(&lease);
        assert_eq!(snapshot(&db).unwrap().len(), 1, "sanity: visible while registered");
        // Simulate the real cleanup order — unregister always runs even
        // when the DB marker-clear write failed — without the DB write.
        crate::ladder_lease_registry::unregister("bgwork_test_lease_5");
        assert!(
            snapshot(&db).unwrap().is_empty(),
            "unregistering the lease must veto the surviving marker"
        );
    }

    #[test]
    fn snapshot_excludes_a_worker_bound_attempt_whose_marker_already_cleared() {
        let db = open();
        let (product_id, _project_id) = product_and_project(&db, "Alpha");
        let work_item_id = seed_task(&db, &product_id, "Chore");
        let attempt = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(product_id)
                    .work_item_id(work_item_id)
                    .pr_url("https://github.com/test/test/pull/1")
                    .pr_number(1)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("abc")
                    .head_sha_before("def")
                    .build(),
            )
            .unwrap()
            .unwrap();
        // Escalated past the mechanical rungs to a full worker: the rung
        // marker is cleared and the attempt now carries a worker lease
        // instead, but it must stay invisible to this badge either way.
        db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 1, "bgwork_test_lease_6", "bgwork_test_ws_6")
            .unwrap();
        db.clear_conflict_resolution_mechanical_rung(&attempt.id).unwrap();
        db.mark_conflict_resolution_running(&attempt.id, "bgwork_test_lease_6", "bgwork_test_ws_6", "worker-1")
            .unwrap();
        assert!(snapshot(&db).unwrap().is_empty());
    }

    #[test]
    fn snapshot_orders_known_starts_before_unstarted_items() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db, "Alpha");
        let run = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "merge_trigger",
            })
            .unwrap()
            .unwrap();
        backdate_planner_run(&db, &run.id, 30);

        let work_item_id = seed_task(&db, &product_id, "Chore");
        let attempt = db
            .insert_conflict_resolution(
                ConflictResolutionInsertInput::builder()
                    .product_id(product_id)
                    .work_item_id(work_item_id)
                    .pr_url("https://github.com/test/test/pull/1")
                    .pr_number(1)
                    .head_branch("feature")
                    .base_branch("main")
                    .base_sha_at_trigger("abc")
                    .head_sha_before("def")
                    .build(),
            )
            .unwrap()
            .unwrap();
        db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 1, "bgwork_test_lease_7", "bgwork_test_ws_7")
            .unwrap();
        let lease = crate::coordinator::CubeWorkspaceLease {
            lease_id: "bgwork_test_lease_7".to_owned(),
            workspace_id: "bgwork_test_ws_7".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/ws-7"),
            dirty_verified: None,
        };
        crate::ladder_lease_registry::register(&lease);
        let items = snapshot(&db).unwrap();
        crate::ladder_lease_registry::unregister("bgwork_test_lease_7");

        assert_eq!(items.len(), 2, "the count must equal the returned list");
        assert_eq!(
            items[0].kind,
            BackgroundWorkKind::ProjectPlanner,
            "the item with a known start must sort before the unstarted item"
        );
        assert_eq!(items[1].kind, BackgroundWorkKind::ConflictRemediation);
    }
}
