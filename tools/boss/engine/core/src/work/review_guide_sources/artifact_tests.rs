use super::tests::{incomplete_packet, packet};
use super::*;
use crate::test_support::{create_active_chore, create_product, open_db};
fn artifact_count(root: &Path) -> usize {
    let dir = root.join(PACKET_ARTIFACT_DIR);
    let Ok(shards) = fs::read_dir(dir) else {
        return 0;
    };
    shards
        .flatten()
        .filter_map(|shard| fs::read_dir(shard.path()).ok())
        .flat_map(|files| files.flatten())
        .filter(|file| !file.file_name().to_string_lossy().ends_with(".tmp"))
        .count()
}

#[test]
fn stale_observation_does_not_write_an_unreferenced_blob() {
    let (dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "stale blob");
    db.persist_pr_review_guide_source_capture(&root, 7, PrSourceCaptureTrigger::Creation, &packet("base-a", "head-a"))
        .unwrap();
    assert_eq!(artifact_count(dir.path()), 1);
    db.persist_pr_review_guide_source_capture(&root, 6, PrSourceCaptureTrigger::Poller, &packet("base-b", "head-b"))
        .unwrap();
    assert_eq!(
        artifact_count(dir.path()),
        1,
        "a rejected stale observation must not leave an unreferenced packet blob"
    );
}

#[test]
fn upgrading_a_comparison_garbage_collects_the_superseded_blob() {
    let (dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "gc upgraded blob");
    let incomplete = incomplete_packet("base", "head", "pinned source read failed: timeout");
    db.persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &incomplete)
        .unwrap();
    assert_eq!(artifact_count(dir.path()), 1);
    db.persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &packet("base", "head"))
        .unwrap();
    assert_eq!(
        artifact_count(dir.path()),
        1,
        "the superseded incomplete packet blob must be deleted"
    );
    let latest = db.get_latest_pr_review_guide_source_capture(&root).unwrap().unwrap();
    assert!(latest.complete);
    assert!(latest.omission_summary.as_deref().unwrap().contains("[]"));
}

#[test]
fn terminal_symlink_omission_is_stored_as_complete() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "settled symlink");
    let packet = incomplete_packet(
        "base",
        "head",
        "pinned tree entry is a symlink; omitted so Contents API cannot follow it and mis-attribute the target's bytes",
    );
    assert!(
        packet.is_complete(),
        "a structurally impossible omission must settle the packet"
    );
    let stored = db
        .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet)
        .unwrap();
    let PrSourceCapturePersistOutcome::Stored(stored) = stored else {
        panic!("settled packet must persist");
    };
    assert!(stored.complete);
    assert!(
        db.select_complete_pr_review_guide_source_capture(&root, &packet.canonical_pr_url, "base", "head", 2)
            .unwrap(),
        "a settled omission must short-circuit later poller collections"
    );
}

#[test]
fn migrate_drops_packet_json_and_adds_omission_summary_on_existing_tables() {
    let dir = tempfile::tempdir().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE pr_review_guide_source_series (
                id TEXT PRIMARY KEY,
                root_task_id TEXT NOT NULL,
                canonical_pr_url TEXT NOT NULL UNIQUE,
                latest_observation_sequence INTEGER NOT NULL DEFAULT 0,
                selected_comparison_id TEXT,
                last_capture_error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE pr_review_guide_source_comparisons (
                id TEXT PRIMARY KEY,
                series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id),
                observation_sequence INTEGER NOT NULL,
                observed_base_sha TEXT NOT NULL,
                merge_base_sha TEXT NOT NULL,
                head_sha TEXT NOT NULL,
                trigger TEXT NOT NULL,
                packet_hash TEXT NOT NULL,
                complete INTEGER NOT NULL CHECK (complete IN (0, 1)),
                omission_count INTEGER NOT NULL DEFAULT 0,
                packet_path TEXT,
                packet_json TEXT NOT NULL,
                captured_at TEXT NOT NULL,
                UNIQUE(series_id, observed_base_sha, head_sha)
            );
            CREATE TABLE pr_review_guide_source_observation_sequence (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                last_sequence INTEGER NOT NULL
            );",
    )
    .unwrap();
    let packet = packet("base", "head");
    let bytes = serde_json::to_vec(&packet).unwrap();
    let hash = packet.content_hash().unwrap();
    let relative = publish_packet_artifact(dir.path(), &hash, &bytes).unwrap();
    conn.execute(
        "INSERT INTO pr_review_guide_source_series
             (id, root_task_id, canonical_pr_url, latest_observation_sequence, created_at, updated_at)
             VALUES ('prgs1', 'root', ?1, 1, 'now', 'now')",
        [&packet.canonical_pr_url],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO pr_review_guide_source_comparisons
             (id, series_id, observation_sequence, observed_base_sha, merge_base_sha, head_sha,
              trigger, packet_hash, complete, omission_count, packet_path, packet_json, captured_at)
             VALUES ('prgc1', 'prgs1', 1, 'base', 'merge-base', 'head', 'creation', ?1, 1, 0, ?2, ?3, 'now')",
        params![hash, relative, "{\"legacy\":true}"],
    )
    .unwrap();
    assert!(table_has_column(&conn, "pr_review_guide_source_comparisons", "packet_json").unwrap());
    assert!(!table_has_column(&conn, "pr_review_guide_source_comparisons", "omission_summary_json").unwrap());
    migrate_pr_review_guide_source_capture_tables(&conn).unwrap();
    assert!(!table_has_column(&conn, "pr_review_guide_source_comparisons", "packet_json").unwrap());
    assert!(table_has_column(&conn, "pr_review_guide_source_comparisons", "omission_summary_json").unwrap());
    let loaded = read_capture_by_endpoints(&conn, dir.path(), "prgs1", "base", "head")
        .unwrap()
        .expect("pre-existing row must survive DROP COLUMN");
    assert_eq!(loaded.packet, packet);
    migrate_pr_review_guide_source_capture_tables(&conn).unwrap();
    assert!(!table_has_column(&conn, "pr_review_guide_source_comparisons", "packet_json").unwrap());
    assert!(table_has_column(&conn, "pr_review_guide_source_comparisons", "omission_summary_json").unwrap());
    assert!(
        read_capture_by_endpoints(&conn, dir.path(), "prgs1", "base", "head")
            .unwrap()
            .is_some()
    );
}

#[test]
fn periodic_gc_deletes_orphaned_blobs_without_touching_live_ones() {
    let (dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "periodic gc");
    db.persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet("base", "head"))
        .unwrap();
    let orphan = dir.path().join("review-guide-sources/zz/orphan");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, b"orphan").unwrap();
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert!(
        !orphan.exists(),
        "crash-orphaned blob must be collected by the periodic sweep"
    );
    assert_eq!(artifact_count(dir.path()), 1);
}
