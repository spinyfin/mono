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
fn unreferenced_walker_names_orphans_from_entry_names() {
    let dir = tempfile::tempdir().unwrap();
    let orphan = dir.path().join(PACKET_ARTIFACT_DIR).join("zz").join("orphan");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, b"orphan").unwrap();
    let found = unreferenced_packet_blob_paths(dir.path(), &Default::default());
    assert_eq!(
        found,
        vec![format!("{PACKET_ARTIFACT_DIR}/zz/orphan")],
        "walker must report the DB-relative path with slash separators"
    );
}

#[test]
fn periodic_gc_deletes_orphaned_blobs_without_touching_live_ones() {
    let (dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "periodic gc");
    db.persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet("base", "head"))
        .unwrap();
    let exclusive = packet_store_lock(dir.path(), true)
        .unwrap()
        .expect("exclusive GC lock must be free after persist returns");
    drop(exclusive);
    let orphan = dir.path().join(PACKET_ARTIFACT_DIR).join("zz").join("orphan");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, b"orphan").unwrap();
    let live = {
        let conn = db.connect().unwrap();
        live_packet_paths(&conn).unwrap()
    };
    let candidates = unreferenced_packet_blob_paths(dir.path(), &live);
    assert!(
        candidates.iter().any(|path| path.ends_with("/orphan")),
        "orphan must be a GC candidate before the sweep; live={live:?} candidates={candidates:?}"
    );
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert!(
        !orphan.exists(),
        "crash-orphaned blob must be collected by the periodic sweep; live={live:?} candidates={candidates:?}"
    );
    assert_eq!(artifact_count(dir.path()), 1);
}

#[test]
fn unlink_failure_does_not_fail_stored_upgrade_or_stop_gc() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, db) = open_db();
    let root = create_active_chore(&db, &create_product(&db), "unlink failure");
    let incomplete = incomplete_packet("base", "head", "pinned source read failed: timeout");
    let stored = db
        .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &incomplete)
        .unwrap();
    let PrSourceCapturePersistOutcome::Stored(stored) = stored else {
        panic!("expected stored capture");
    };
    let old_path = dir.path().join(stored.packet_path.unwrap());
    // Keep the old packet readable for the upgrade, but prohibit unlink.
    let shard = old_path.parent().unwrap();
    let permissions = fs::metadata(shard).unwrap().permissions();
    fs::set_permissions(shard, fs::Permissions::from_mode(0o555)).unwrap();
    let upgraded = db
        .persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &packet("base", "head"))
        .unwrap();
    assert!(matches!(upgraded, PrSourceCapturePersistOutcome::Stored(_)));
    assert!(
        db.get_latest_pr_review_guide_source_capture(&root)
            .unwrap()
            .unwrap()
            .complete
    );
    assert!(old_path.is_file());
    let orphan = dir.path().join(PACKET_ARTIFACT_DIR).join("zz").join("later-orphan");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, b"orphan").unwrap();
    let swept = db.gc_unreferenced_pr_review_guide_source_artifacts();
    fs::set_permissions(shard, permissions).unwrap();
    swept.unwrap();
    assert!(old_path.is_file());
    assert!(!orphan.exists(), "a failed unlink must not abort the sweep");
}
