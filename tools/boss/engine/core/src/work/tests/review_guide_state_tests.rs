//! `attach_review_guide_state` / `WorkDb::root_task_id_for_review_guide_series`
//! — the projection and lookup the completion finalizer and every
//! board/task read path depend on to surface `Task.review_guide_lifecycle`
//! / `review_guide_readable_version_id` / `review_guide_stale_source`. The
//! feature ships inert behind those two fields, so a regression here (a
//! renamed column, a changed join key) would make the whole affordance
//! silently absent with every other test green.

use super::*;
use crate::test_support::{review_guide_source_packet, seed_review_guide_series};

#[test]
fn root_task_id_for_review_guide_series_resolves_the_seeded_root() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let root = create_active_chore(&db, &product_id, "review guide state test");
    let (series_id, _comparison_id) = seed_review_guide_series(&db, &root);

    assert_eq!(db.root_task_id_for_review_guide_series(&series_id).unwrap(), Some(root));
}

#[test]
fn root_task_id_for_review_guide_series_is_none_for_an_unknown_series() {
    let (_dir, db) = open_db();
    assert_eq!(db.root_task_id_for_review_guide_series("prgs_missing").unwrap(), None);
}

/// After a version publishes, `attach_review_guide_state` must set
/// `lifecycle == "ready"` plus the readable version id on the PR-bearing
/// root task, while leaving a non-PR task and a revision row untouched
/// (`None`) — a series is always keyed by the chain-root task id, and the
/// affordance is deliberately absent from every other row shape.
#[test]
fn attach_review_guide_state_sets_ready_lifecycle_on_the_root_only() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let root = create_active_chore(&db, &product_id, "review guide state test");
    let (series_id, comparison_id) = seed_review_guide_series(&db, &root);

    let attempt = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    assert_eq!(attempt.status, "queued");
    let published = db
        .publish_pr_review_guide_version(&attempt.id, "# Guide\n\n## Problem\n", "raw")
        .unwrap();
    let PublishReviewGuideOutcome::Published(version) = published else {
        panic!("must publish")
    };

    let ts = "2026-05-14T00:00:00Z";
    let mut tasks = vec![
        make_bare_task(&root, "chore", None, Some("https://github.com/acme/widget/pull/9"), ts),
        make_bare_task("task_no_pr", "task", None, None, ts),
        // Carries a `pr_url` too, to prove the guard is the id mismatch (a
        // series is always keyed by the chain-root task id, never a
        // revision's own id) rather than merely the absence of a PR URL,
        // which `task_no_pr` above already covers.
        make_bare_task(
            "task_revision",
            "revision",
            Some(&root),
            Some("https://github.com/acme/widget/pull/9"),
            ts,
        ),
    ];
    let mut chores: Vec<Task> = vec![];

    {
        let conn = db.connect().unwrap();
        attach_review_guide_state(&conn, &mut tasks, &mut chores).unwrap();
    }

    let root_task = tasks.iter().find(|t| t.id == root).unwrap();
    assert_eq!(root_task.review_guide_lifecycle.as_deref(), Some("ready"));
    assert_eq!(
        root_task.review_guide_readable_version_id.as_deref(),
        Some(version.id.as_str())
    );
    assert_eq!(root_task.review_guide_stale_source, Some(false));
    assert_eq!(
        root_task.review_guide_selected_comparison_id.as_deref(),
        Some(comparison_id.as_str())
    );

    // A later capture with a different head SHA advances
    // `selected_comparison_id` while `readable_version_id` stays on the
    // published version — the true branch of `stale_source`.
    db.persist_pr_review_guide_source_capture(
        &root,
        2,
        PrSourceCaptureTrigger::Poller,
        &review_guide_source_packet("base2", "head2"),
    )
    .unwrap();
    {
        let conn = db.connect().unwrap();
        attach_review_guide_state(&conn, &mut tasks, &mut chores).unwrap();
    }
    let root_task = tasks.iter().find(|t| t.id == root).unwrap();
    assert_eq!(root_task.review_guide_stale_source, Some(true));
    assert_eq!(
        root_task.review_guide_readable_version_id.as_deref(),
        Some(version.id.as_str()),
        "a later capture must not clear the already-published readable version"
    );
    assert_ne!(
        root_task.review_guide_selected_comparison_id.as_deref(),
        Some(comparison_id.as_str())
    );

    let non_pr_task = tasks.iter().find(|t| t.id == "task_no_pr").unwrap();
    assert_eq!(
        non_pr_task.review_guide_lifecycle, None,
        "a non-PR row must never carry series state"
    );

    let revision_task = tasks.iter().find(|t| t.id == "task_revision").unwrap();
    assert_eq!(
        revision_task.review_guide_lifecycle, None,
        "a revision row's own id is never a series key"
    );
}
