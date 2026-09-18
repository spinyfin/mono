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

fn publish_ready_guide(db: &WorkDb, series_id: &str, comparison_id: &str, markdown: &str) -> String {
    let attempt = db
        .create_pr_review_guide_attempt(series_id, comparison_id, "review-guide-v1")
        .unwrap();
    let published = db
        .publish_pr_review_guide_version(&attempt.id, markdown, "raw")
        .unwrap();
    let PublishReviewGuideOutcome::Published(version) = published else {
        panic!("must publish")
    };
    version.id
}

fn seed_review_guide_series_for_pr(db: &WorkDb, root: &str, pr_url: &str, base: &str, head: &str) -> (String, String) {
    let mut packet = review_guide_source_packet(base, head);
    packet.canonical_pr_url = pr_url.to_owned();
    let stored = db
        .persist_pr_review_guide_source_capture(root, 1, PrSourceCaptureTrigger::Creation, &packet)
        .unwrap();
    let PrSourceCapturePersistOutcome::Stored(capture) = stored else {
        panic!("capture must persist")
    };
    (capture.series_id, capture.comparison_id)
}

/// Replacing the card's PR attaches a new series and preserves the previous
/// one. The card projection must follow the task's current `pr_url`, not
/// whichever series was touched last.
#[test]
fn attach_review_guide_state_follows_current_pr_not_retired_sibling() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let root = create_active_chore(&db, &product_id, "replaced PR");
    let retired_pr = "https://github.com/acme/widget/pull/9";
    let current_pr = "https://github.com/acme/widget/pull/10";

    let (retired_series, retired_comparison) = seed_review_guide_series_for_pr(&db, &root, retired_pr, "base", "head");
    let retired_version = publish_ready_guide(&db, &retired_series, &retired_comparison, "# Retired\n");

    let (current_series, current_comparison) =
        seed_review_guide_series_for_pr(&db, &root, current_pr, "base-b", "head-b");
    let current_version = publish_ready_guide(&db, &current_series, &current_comparison, "# Current\n");

    // A later capture on the retired PR bumps `updated_at` (and
    // `latest_observation_sequence`) after the current series exists —
    // the exact window a last-touched-wins lookup would mis-bind.
    let mut late_packet = review_guide_source_packet("base-late", "head-late");
    late_packet.canonical_pr_url = retired_pr.to_owned();
    db.persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &late_packet)
        .unwrap();

    let ts = "2026-05-14T00:00:00Z";
    let mut tasks = vec![make_bare_task(&root, "chore", None, Some(current_pr), ts)];
    let mut chores: Vec<Task> = vec![];
    {
        let conn = db.connect().unwrap();
        attach_review_guide_state(&conn, &mut tasks, &mut chores).unwrap();
    }
    let root_task = &tasks[0];
    assert_eq!(root_task.review_guide_lifecycle.as_deref(), Some("ready"));
    assert_eq!(
        root_task.review_guide_readable_version_id.as_deref(),
        Some(current_version.as_str()),
        "projection must follow the current PR, not the retired series even after a late updated_at bump"
    );
    assert_ne!(
        root_task.review_guide_readable_version_id.as_deref(),
        Some(retired_version.as_str())
    );
    assert_eq!(
        root_task.review_guide_selected_comparison_id.as_deref(),
        Some(current_comparison.as_str())
    );
    assert_eq!(root_task.review_guide_stale_source, Some(false));
}

/// Until the replacement PR has a series of its own, the card must not
/// fall back to the retired PR's guide.
#[test]
fn attach_review_guide_state_is_none_until_current_pr_has_a_series() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let root = create_active_chore(&db, &product_id, "replaced PR no capture yet");
    let current_pr = "https://github.com/acme/widget/pull/10";

    let (retired_series, retired_comparison) = seed_review_guide_series(&db, &root);
    let _retired_version = publish_ready_guide(&db, &retired_series, &retired_comparison, "# Retired\n");

    let ts = "2026-05-14T00:00:00Z";
    let mut tasks = vec![make_bare_task(&root, "chore", None, Some(current_pr), ts)];
    let mut chores: Vec<Task> = vec![];
    {
        let conn = db.connect().unwrap();
        attach_review_guide_state(&conn, &mut tasks, &mut chores).unwrap();
    }
    let root_task = &tasks[0];
    assert_eq!(
        root_task.review_guide_lifecycle, None,
        "a replacement PR with no series must not inherit the retired PR's guide"
    );
    assert_eq!(root_task.review_guide_readable_version_id, None);
    assert_eq!(root_task.review_guide_selected_comparison_id, None);
    assert_eq!(root_task.review_guide_stale_source, None);
}
