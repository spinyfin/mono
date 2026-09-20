use super::*;
use crate::test_support::{create_active_chore, create_product, open_db, seed_review_guide_series};

fn publish(db: &WorkDb, series: &str, comparison: &str) -> super::super::review_guide_jobs::PrReviewGuideVersion {
    let attempt = db
        .create_pr_review_guide_attempt(series, comparison, "review-guide-v1")
        .unwrap();
    let PublishReviewGuideOutcome::Published(version) = db
        .publish_pr_review_guide_version(&attempt.id, "# Guide\n\nOriginal quote", "raw")
        .unwrap()
    else {
        panic!("expected published guide")
    };
    *version
}

fn input(series: &str) -> CreateCommentInput {
    CreateCommentInput::builder()
        .artifact_kind("pr_review_guide")
        .artifact_id(series)
        .anchor(CommentAnchor {
            exact: "Original quote".into(),
            ..Default::default()
        })
        .body("Keep the behavior")
        .author("user:test")
        .doc_version("original-projection-hash")
        .plain_text_projection_version(1)
        .build()
}

#[test]
fn guide_comment_context_survives_regeneration_resolution_and_reopen() {
    let (dir, db) = open_db();
    let root = create_active_chore(&db, &create_product(&db), "guide comments");
    let (series, comparison) = seed_review_guide_series(&db, &root);
    let old = publish(&db, &series, &comparison);
    let comment = db
        .create_comment_with_guide_version(input(&series), Some(&old.id))
        .unwrap();
    let context = comment.guide_context.as_ref().unwrap();
    assert_eq!(context.version_id, old.id);
    assert_eq!(context.comparison_id, comparison);
    assert_eq!(context.head_sha, "head");
    assert_eq!(context.base_sha, "base");
    assert!(!context.packet_hash.is_empty());
    let new = publish(&db, &series, &comparison);
    assert_ne!(old.id, new.id);
    let config = CommentFuzzyConfig::from_env();
    assert!(
        db.resolve_guide_comments(&series, Some(&new.id), "Original quote", &config)
            .unwrap()
            .is_empty()
    );
    let exact = db
        .resolve_guide_comments(&series, Some(&old.id), "Original quote", &config)
        .unwrap();
    assert_eq!(exact[0].resolution.kind, RESOLVED_WITH_EXACT);
    let orphan = db
        .resolve_guide_comments(&series, Some(&old.id), "unrelated", &config)
        .unwrap();
    assert_eq!(orphan[0].resolution.kind, RESOLVED_WITH_ORPHAN);
    assert_eq!(db.get_comment(&comment.id).unwrap().unwrap(), comment);
    assert!(
        db.update_comment_anchor(&comment.id, &CommentAnchor::default(), "new-hash", 2)
            .is_err()
    );
    db.create_comment_thread_entry(
        &comment.id,
        THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP,
        "user:test",
        "Original feedback still matters",
        None,
        None,
    )
    .unwrap();
    let listed = db.list_comments_with_thread("pr_review_guide", &series, false).unwrap();
    assert_eq!(listed[0].comment, comment);
    assert_eq!(listed[0].thread_entries.len(), 1);
    assert!(!db.comments_banner_state("pr_review_guide", &series).unwrap().revisable);
    db.set_comment_status(&comment.id, COMMENT_STATUS_RESOLVED, Some("user:test"))
        .unwrap();
    assert!(db.list_comments("pr_review_guide", &series, false).unwrap().is_empty());
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&root])
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE pr_review_guide_source_comparisons SET captured_at = '1'", [])
        .unwrap();
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert!(db.get_pr_review_guide_comparison_by_id(&comparison).unwrap().is_some());
    let reopened = WorkDb::open(dir.path().join("state.db")).unwrap();
    // Reopening must preserve authored evidence even on a resolved comment.
    assert_eq!(
        reopened.list_comments("pr_review_guide", &series, true).unwrap()[0].guide_context,
        comment.guide_context
    );
}

#[test]
fn terminal_aged_published_guide_without_comments_is_collected() {
    let (dir, db) = open_db();
    let root = create_active_chore(&db, &create_product(&db), "uncommented guide retention");
    let (series, comparison) = seed_review_guide_series(&db, &root);
    let version = publish(&db, &series, &comparison);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&root])
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE pr_review_guide_source_comparisons SET captured_at = '1'", [])
        .unwrap();
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert!(db.get_pr_review_guide_version(&version.id).unwrap().is_none());
    assert!(db.get_pr_review_guide_comparison_by_id(&comparison).unwrap().is_none());
    assert!(db.get_latest_pr_review_guide_source_capture(&root).unwrap().is_none());
    let leftover = std::fs::read_dir(dir.path().join("review-guide-sources")).map(|entries| {
        entries
            .flatten()
            .filter_map(|shard| std::fs::read_dir(shard.path()).ok())
            .flatten()
            .flatten()
            .filter(|file| !file.file_name().to_string_lossy().ends_with(".tmp"))
            .count()
    });
    assert_eq!(leftover.unwrap_or(0), 0);
}

#[test]
fn create_and_resolve_reject_missing_or_cross_series_version() {
    let (_dir, db) = open_db();
    let root = create_active_chore(&db, &create_product(&db), "guide validation");
    let (series, comparison) = seed_review_guide_series(&db, &root);
    let version = publish(&db, &series, &comparison);
    let missing = input(&series);
    assert!(db.create_comment(missing).is_err());
    assert!(
        db.create_comment_with_guide_version(input("other-series"), Some(&version.id))
            .is_err()
    );
    assert!(
        db.create_comment_with_guide_version(input(&series), Some("missing-version"))
            .is_err()
    );
    let mut wrong_kind = input(&series);
    wrong_kind.artifact_kind = "work_item".into();
    assert!(
        db.create_comment_with_guide_version(wrong_kind, Some(&version.id))
            .is_err()
    );
    let cfg = CommentFuzzyConfig::from_env();
    assert!(
        db.resolve_guide_comments(&series, None, "Original quote", &cfg)
            .is_err()
    );
    assert!(
        db.resolve_guide_comments("other-series", Some(&version.id), "Original quote", &cfg)
            .is_err()
    );
    assert!(
        db.resolve_comments("pr_review_guide", &series, "Original quote", 1, &cfg)
            .is_err()
    );
    assert!(db.list_comments("pr_review_guide", &series, true).unwrap().is_empty());
}

#[test]
fn additive_migration_preserves_legacy_comments_and_is_repeatable() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE work_comments (
            id TEXT PRIMARY KEY, artifact_kind TEXT, artifact_id TEXT, anchor_json TEXT,
            doc_version TEXT, plain_text_projection_version INTEGER, body TEXT);
         INSERT INTO work_comments VALUES ('legacy', 'work_item', 'task', '{}', 'hash', 1, 'Keep me');",
    )
    .unwrap();
    migrate_guide_comments(&conn).unwrap();
    migrate_guide_comments(&conn).unwrap();
    let row: (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT body, guide_version_id, guide_context_json FROM work_comments WHERE id = 'legacy'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("Keep me".into(), None, None));
}
