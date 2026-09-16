use super::*;
use crate::test_support::{create_active_chore, create_product, open_db};
use crate::work::{FakePrStateChecker, PrOpenState};
use boss_pr_review_sources::{ChangeKind, PinnedSource, SourceFile, SourceOmission, SourceSide};
use boss_protocol::{CreateExecutionInput, CreateRevisionInput, ExecutionKind, ExecutionStatus, WorkItemPatch};

pub(super) fn packet(base: &str, head: &str) -> SourcePacket {
    SourcePacket {
        schema_version: 3,
        canonical_pr_url: "https://github.com/acme/widget/pull/11".to_owned(),
        pr_number: 11,
        title: "Capture immutable comparison".to_owned(),
        body: Some("body".to_owned()),
        base_repository: "acme/widget".to_owned(),
        head_repository: "acme/widget".to_owned(),
        observed_base_sha: base.to_owned(),
        probe_base_sha: None,
        merge_base_sha: "merge-base".to_owned(),
        head_sha: head.to_owned(),
        files: vec![SourceFile {
            path: "src/lib.rs".to_owned(),
            previous_path: None,
            change_kind: ChangeKind::Modified,
            additions: 1,
            deletions: 1,
            patch: Some("@@".to_owned()),
            before: Some(
                PinnedSource::builder()
                    .repository("acme/widget")
                    .sha("merge-base")
                    .path("src/lib.rs")
                    .object_sha("before-object")
                    .content("before\n")
                    .content_hash("before-hash")
                    .byte_count(7)
                    .build(),
            ),
            after: Some(
                PinnedSource::builder()
                    .repository("acme/widget")
                    .sha(head)
                    .path("src/lib.rs")
                    .object_sha("after-object")
                    .content("after\n")
                    .content_hash("after-hash")
                    .byte_count(6)
                    .build(),
            ),
        }],
        omissions: Vec::new(),
    }
}

#[test]
fn captures_series_by_canonical_root_and_rejects_stale_observations() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "capture source packet");
    let stored = db
        .persist_pr_review_guide_source_capture(&root, 7, PrSourceCaptureTrigger::Creation, &packet("base-a", "head-a"))
        .unwrap();
    assert!(matches!(stored, PrSourceCapturePersistOutcome::Stored(_)));
    assert_eq!(
        db.persist_pr_review_guide_source_capture(
            &root,
            6,
            PrSourceCaptureTrigger::Poller,
            &packet("base-b", "head-b")
        )
        .unwrap(),
        PrSourceCapturePersistOutcome::IgnoredStaleObservation
    );
    let latest = db.get_latest_pr_review_guide_source_capture(&root).unwrap().unwrap();
    assert_eq!(latest.observation_sequence, 7);
    assert_eq!(latest.packet.head_sha, "head-a");
    assert!(latest.complete);
}

#[test]
fn duplicate_endpoints_reuse_the_immutable_packet() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "deduplicate source packet");
    let first = db
        .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet("base", "head"))
        .unwrap();
    let second = db
        .persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Completion, &packet("base", "head"))
        .unwrap();
    let PrSourceCapturePersistOutcome::Stored(first) = first else {
        panic!("first capture must persist")
    };
    let PrSourceCapturePersistOutcome::Existing(second) = second else {
        panic!("same endpoints must reuse the original immutable packet")
    };
    assert_eq!(first.packet_hash, second.packet_hash);
    assert_eq!(second.observation_sequence, 1);
}

pub(super) fn incomplete_packet(base: &str, head: &str, reason: &str) -> SourcePacket {
    let terminal = reason.contains("symlink");
    let mut packet = packet(base, head);
    packet.files[0].after = Some(
        PinnedSource::builder()
            .repository("acme/widget")
            .sha(head)
            .path("src/lib.rs")
            .omission(reason)
            .terminal(terminal)
            .build(),
    );
    packet.omissions = vec![SourceOmission {
        path: Some("src/lib.rs".to_owned()),
        side: Some(SourceSide::After),
        reason: reason.to_owned(),
        terminal,
    }];
    packet
}

#[test]
fn incomplete_packet_is_upgraded_when_a_later_collection_is_complete() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "upgrade incomplete packet");
    let incomplete = incomplete_packet("base", "head", "blob is unavailable");
    assert!(!incomplete.is_complete());
    let first = db
        .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &incomplete)
        .unwrap();
    assert!(matches!(first, PrSourceCapturePersistOutcome::Stored(_)));
    let complete = packet("base", "head");
    let upgraded = db
        .persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &complete)
        .unwrap();
    let PrSourceCapturePersistOutcome::Stored(upgraded) = upgraded else {
        panic!("incomplete comparison must be replaced by a complete packet")
    };
    assert!(upgraded.complete);
    assert_eq!(upgraded.observation_sequence, 1);
    assert_eq!(
        db.get_latest_pr_review_guide_source_capture(&root)
            .unwrap()
            .unwrap()
            .packet
            .files[0]
            .after
            .as_ref()
            .unwrap()
            .content
            .as_deref(),
        Some("after\n")
    );
}

#[test]
fn incomplete_packet_stays_sticky_when_a_retry_is_not_better() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "sticky incomplete packet");
    let first_packet = incomplete_packet("base", "head", "blob is unavailable");
    db.persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &first_packet)
        .unwrap();
    let mut worse = first_packet.clone();
    worse.omissions.push(SourceOmission {
        path: Some("src/lib.rs".to_owned()),
        side: Some(SourceSide::Before),
        reason: "second hole".to_owned(),
        terminal: true,
    });
    let reused = db
        .persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &worse)
        .unwrap();
    let PrSourceCapturePersistOutcome::Existing(existing) = reused else {
        panic!("a worse incomplete retry must keep the original packet")
    };
    assert_eq!(existing.packet.omissions.len(), 1);
    assert!(!existing.complete);
}

#[test]
fn returning_to_an_earlier_comparison_selects_it_again() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "reselect earlier comparison");
    db.persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet("base-a", "head-a"))
        .unwrap();
    db.persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &packet("base-b", "head-b"))
        .unwrap();
    assert_eq!(
        db.get_latest_pr_review_guide_source_capture(&root)
            .unwrap()
            .unwrap()
            .packet
            .head_sha,
        "head-b"
    );
    db.persist_pr_review_guide_source_capture(&root, 3, PrSourceCaptureTrigger::Poller, &packet("base-a", "head-a"))
        .unwrap();
    let latest = db.get_latest_pr_review_guide_source_capture(&root).unwrap().unwrap();
    assert_eq!(latest.packet.head_sha, "head-a");
    assert_eq!(latest.observation_sequence, 1);
}

#[test]
fn missing_packet_artifact_is_an_explicit_source_failure() {
    let (dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "missing artifact");
    let stored = db
        .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet("base", "head"))
        .unwrap();
    let PrSourceCapturePersistOutcome::Stored(stored) = stored else {
        panic!("capture must persist")
    };
    let relative = stored.packet_path.expect("artifact path must be recorded");
    fs::remove_file(dir.path().join(&relative)).unwrap();
    let err = db
        .get_latest_pr_review_guide_source_capture(&root)
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing referenced source packet blob"), "{err}");
    assert_recollects(&db, &root, &packet("base", "head"));
}

#[test]
fn valid_json_with_modified_metadata_fails_integrity_validation() {
    let (dir, db) = open_db();
    let original = packet("base", "head");
    db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &original)
        .unwrap();
    let capture = db.get_latest_pr_review_guide_source_capture("root").unwrap().unwrap();
    let mut modified = original.clone();
    modified.title = "corrupted metadata".to_owned();
    fs::write(
        dir.path().join(capture.packet_path.unwrap()),
        serde_json::to_vec(&modified).unwrap(),
    )
    .unwrap();
    assert!(
        db.get_latest_pr_review_guide_source_capture("root")
            .unwrap_err()
            .to_string()
            .contains("integrity failure")
    );
    assert_recollects(&db, "root", &original);
}

fn assert_recollects(db: &WorkDb, root: &str, packet: &SourcePacket) {
    let endpoints = boss_pr_review_sources::PinnedComparison {
        base_sha: packet.observed_base_sha.clone(),
        head_sha: packet.head_sha.clone(),
    };
    let claim = crate::review_guide_capture::prepare_capture(db, root, &packet.canonical_pr_url, &endpoints, 3)
        .unwrap()
        .expect("invalid artifact must permit recollection");
    let error: Option<String> = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT last_capture_error FROM pr_review_guide_source_series WHERE canonical_pr_url = ?1",
            [&packet.canonical_pr_url],
            |row| row.get(0),
        )
        .unwrap();
    assert!(error.is_some(), "artifact failure remains visible until recovery");
    db.persist_pr_review_guide_source_capture(root, 3, PrSourceCaptureTrigger::Poller, packet)
        .unwrap();
    drop(claim);
    assert!(
        crate::review_guide_capture::prepare_capture(db, root, &packet.canonical_pr_url, &endpoints, 4)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.get_latest_pr_review_guide_source_capture(root)
            .unwrap()
            .unwrap()
            .packet,
        *packet
    );
}

#[test]
fn diagnostic_read_selects_the_latest_pr_series() {
    let (_dir, db) = open_db();
    let first = packet("base", "first");
    let mut second = packet("base", "second");
    second.canonical_pr_url = "https://github.com/acme/widget/pull/12".to_owned();
    second.pr_number = 12;
    db.persist_pr_review_guide_source_capture("root", 2, PrSourceCaptureTrigger::Creation, &second)
        .unwrap();
    db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &first)
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("UPDATE pr_review_guide_source_series SET updated_at = CASE canonical_pr_url WHEN ?1 THEN '2026-01-02' ELSE '2026-01-01' END", [&first.canonical_pr_url]).unwrap();
    drop(conn);
    assert_eq!(
        db.get_latest_pr_review_guide_source_capture("root")
            .unwrap()
            .unwrap()
            .packet,
        second
    );
}

#[test]
fn concurrent_same_digest_publications_are_complete() {
    let directory = tempfile::tempdir().unwrap();
    let packet = packet("base", "head");
    let bytes = serde_json::to_vec(&packet).unwrap();
    let hash = packet.content_hash().unwrap();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                barrier.wait();
                let relative = publish_packet_artifact(directory.path(), &hash, &bytes).unwrap();
                assert_eq!(load_packet(directory.path(), Some(&relative), &hash).unwrap(), packet);
            });
        }
    });
}

#[test]
fn complete_reselection_fences_a_delayed_collection() {
    let (_dir, db) = open_db();
    let a = packet("base-a", "head-a");
    let b = packet("base-b", "head-b");
    db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &a)
        .unwrap();
    db.persist_pr_review_guide_source_capture("root", 2, PrSourceCaptureTrigger::Poller, &b)
        .unwrap();
    assert!(
        db.select_complete_pr_review_guide_source_capture("root", &a.canonical_pr_url, "base-a", "head-a", 4)
            .unwrap()
    );
    assert_eq!(
        db.persist_pr_review_guide_source_capture("root", 3, PrSourceCaptureTrigger::Poller, &b)
            .unwrap(),
        PrSourceCapturePersistOutcome::IgnoredStaleObservation
    );
    assert_eq!(
        db.get_latest_pr_review_guide_source_capture("root")
            .unwrap()
            .unwrap()
            .packet,
        a
    );
}

#[test]
fn capture_failure_is_durable_and_monotonic() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "record source failure");
    assert!(
        db.record_pr_review_guide_source_capture_failure(
            &root,
            "https://github.com/acme/widget/pull/15",
            4,
            "GitHub timed out"
        )
        .unwrap()
    );
    assert!(
        !db.record_pr_review_guide_source_capture_failure(
            &root,
            "https://github.com/acme/widget/pull/15",
            3,
            "older error"
        )
        .unwrap()
    );
}

#[test]
fn source_observation_sequence_is_monotonic_before_collection() {
    let (_dir, db) = open_db();
    assert_eq!(db.allocate_pr_review_guide_source_observation_sequence().unwrap(), 1);
    assert_eq!(db.allocate_pr_review_guide_source_observation_sequence().unwrap(), 2);
}

#[test]
fn revision_execution_resolves_to_the_canonical_pr_root() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "root implementation");
    let pr_url = "https://github.com/acme/widget/pull/19";
    db.update_work_item(
        &root,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr_url.to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(root.clone())
                .description("address source feedback")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert_eq!(db.review_guide_source_root_for_execution(&execution.id).unwrap(), root);
}

#[test]
fn migration_removes_inline_packets_and_unreadable_legacy_rows() {
    let (_dir, db) = open_db();
    let packet = packet("base", "head");
    db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &packet)
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "ALTER TABLE pr_review_guide_source_comparisons ADD COLUMN packet_json TEXT NOT NULL DEFAULT '';
         UPDATE pr_review_guide_source_comparisons SET packet_path = NULL;",
    )
    .unwrap();
    migrate_pr_review_guide_source_capture_tables(&conn).unwrap();
    migrate_pr_review_guide_source_capture_tables(&conn).unwrap();
    assert!(!table_has_column(&conn, "pr_review_guide_source_comparisons", "packet_json").unwrap());
    let count: i64 = conn
        .query_row("SELECT count(*) FROM pr_review_guide_source_comparisons", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    drop(conn);
    assert!(db.get_latest_pr_review_guide_source_capture("root").unwrap().is_none());
    db.persist_pr_review_guide_source_capture("root", 2, PrSourceCaptureTrigger::Poller, &packet)
        .unwrap();
    assert_eq!(
        db.get_latest_pr_review_guide_source_capture("root")
            .unwrap()
            .unwrap()
            .packet,
        packet
    );
}

#[test]
fn stale_invalid_comparison_does_not_replace_current_diagnostics() {
    let (dir, db) = open_db();
    let old = packet("old-base", "old-head");
    let new = packet("new-base", "new-head");
    db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &old)
        .unwrap();
    let old_capture = db.get_latest_pr_review_guide_source_capture("root").unwrap().unwrap();
    db.persist_pr_review_guide_source_capture("root", 2, PrSourceCaptureTrigger::Poller, &new)
        .unwrap();
    fs::remove_file(dir.path().join(old_capture.packet_path.unwrap())).unwrap();
    assert!(
        !db.select_complete_pr_review_guide_source_capture("root", &old.canonical_pr_url, "old-base", "old-head", 1)
            .unwrap()
    );
    let error: Option<String> = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT last_capture_error FROM pr_review_guide_source_series WHERE canonical_pr_url = ?1",
            [&old.canonical_pr_url],
            |row| row.get(0),
        )
        .unwrap();
    assert!(error.is_none());
    assert_eq!(
        db.get_latest_pr_review_guide_source_capture("root")
            .unwrap()
            .unwrap()
            .packet,
        new
    );
}

#[test]
fn incomplete_artifacts_recover_for_missing_corrupt_and_identical_replacements() {
    for corrupt in [false, true] {
        for identical in [false, true] {
            let (dir, db) = open_db();
            let original = incomplete_packet("base", "head", "unavailable blob");
            db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &original)
                .unwrap();
            let capture = db.get_latest_pr_review_guide_source_capture("root").unwrap().unwrap();
            let path = dir.path().join(capture.packet_path.unwrap());
            if corrupt {
                fs::write(&path, b"truncated").unwrap();
            } else {
                fs::remove_file(&path).unwrap();
            }
            let replacement = if identical {
                original
            } else {
                incomplete_packet("base", "head", "new omission")
            };
            db.persist_pr_review_guide_source_capture("root", 2, PrSourceCaptureTrigger::Poller, &replacement)
                .unwrap();
            assert_eq!(
                db.get_latest_pr_review_guide_source_capture("root")
                    .unwrap()
                    .unwrap()
                    .packet,
                replacement
            );
        }
    }
}

#[test]
fn blocked_artifact_validation_allows_unrelated_claim_and_database_write() {
    let (_dir, db) = open_db();
    let original = packet("base", "head");
    db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &original)
        .unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            BEFORE_PACKET_READ.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }))
            });
            crate::review_guide_capture::prepare_capture(
                &db,
                "root",
                &original.canonical_pr_url,
                &boss_pr_review_sources::PinnedComparison {
                    base_sha: "base".into(),
                    head_sha: "head".into(),
                },
                2,
            )
            .unwrap()
        });
        entered_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        scope.spawn(|| {
            let sequence = db.allocate_pr_review_guide_source_observation_sequence().unwrap();
            let claim = crate::review_guide_capture::prepare_capture(
                &db,
                "other-root",
                "https://github.com/acme/other/pull/1",
                &boss_pr_review_sources::PinnedComparison {
                    base_sha: "other-base".into(),
                    head_sha: "other-head".into(),
                },
                sequence,
            )
            .unwrap();
            claimed_tx.send(claim.is_some()).unwrap();
        });
        let claimed = claimed_rx.recv_timeout(std::time::Duration::from_secs(10));
        release_tx.send(()).unwrap();
        assert!(reader.join().unwrap().is_none());
        assert!(claimed.unwrap());
    });
}

#[test]
fn migration_replaces_the_legacy_series_index() {
    let (_dir, db) = open_db();
    let conn = db.connect().unwrap();
    conn.execute_batch("DROP INDEX pr_review_guide_source_series_observation_idx;
        CREATE INDEX pr_review_guide_source_series_root_idx ON pr_review_guide_source_series(root_task_id, updated_at DESC);").unwrap();
    migrate_pr_review_guide_source_capture_tables(&conn).unwrap();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = ?1",
            ["pr_review_guide_source_series_observation_idx"],
            |row| row.get(0),
        )
        .unwrap();
    assert!(sql.contains("latest_observation_sequence DESC, id DESC"));
    let old_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = ?1",
            ["pr_review_guide_source_series_root_idx"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_count, 0);
}
