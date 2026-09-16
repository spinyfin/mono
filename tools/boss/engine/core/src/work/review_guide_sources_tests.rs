use super::*;

#[test]
fn publication_releases_database_locks_and_rechecks_observation_order() {
    let (dir, db) = open_db();
    let candidate = packet("base", "head");
    let result = db
        .persist_source_capture_with_publisher(
            "root",
            1,
            PrSourceCaptureTrigger::Poller,
            &candidate,
            |root, hash, bytes| {
                // Fail immediately rather than hanging if publication holds the mutex.
                drop(
                    db.conn
                        .try_lock()
                        .expect("publication must release the pooled connection"),
                );
                let mut independent = db.connect_new()?;
                let tx = independent.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute(
                    "UPDATE pr_review_guide_source_series SET latest_observation_sequence = 2",
                    [],
                )?;
                tx.commit()?;
                publish_packet_artifact(root, hash, bytes)
            },
        )
        .unwrap();
    assert_eq!(result, PrSourceCapturePersistOutcome::IgnoredStaleObservation);
    assert!(db.get_latest_pr_review_guide_source_capture("root").unwrap().is_none());
    assert_eq!(artifact_count(dir.path()), 1);
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert_eq!(artifact_count(dir.path()), 0);
}

#[test]
fn publication_rechecks_a_concurrent_complete_endpoint() {
    let (_dir, db) = open_db();
    let candidate = incomplete_packet("base", "head", "pinned source read failed: timeout");
    let result = db
        .persist_source_capture_with_publisher(
            "root",
            1,
            PrSourceCaptureTrigger::Poller,
            &candidate,
            |root, hash, bytes| {
                db.persist_pr_review_guide_source_capture(
                    "root",
                    1,
                    PrSourceCaptureTrigger::Poller,
                    &packet("base", "head"),
                )?;
                publish_packet_artifact(root, hash, bytes)
            },
        )
        .unwrap();
    let PrSourceCapturePersistOutcome::Existing(existing) = result else {
        panic!("reuse concurrent endpoint");
    };
    assert!(existing.complete);
    assert!(
        db.get_latest_pr_review_guide_source_capture("root")
            .unwrap()
            .unwrap()
            .complete
    );
}

#[test]
fn retryable_omissions_stop_after_three_attempts_and_new_head_can_retry() {
    let (_dir, db) = open_db();
    let mut packet = incomplete_packet("base", "head", "pinned source read failed: timeout");
    packet.probe_base_sha = Some("probe".to_owned());
    for sequence in 1..=MAX_CAPTURE_ATTEMPTS {
        assert!(
            !db.select_probe_pr_review_guide_source_capture(
                "root",
                &packet.canonical_pr_url,
                "probe",
                "head",
                sequence
            )
            .unwrap()
        );
        db.persist_pr_review_guide_source_capture("root", sequence, PrSourceCaptureTrigger::Poller, &packet)
            .unwrap();
    }
    assert!(
        db.select_probe_pr_review_guide_source_capture("root", &packet.canonical_pr_url, "probe", "head", 4)
            .unwrap()
    );
    assert!(
        db.select_complete_pr_review_guide_source_capture("root", &packet.canonical_pr_url, "base", "head", 5)
            .unwrap()
    );
    assert!(
        !db.select_probe_pr_review_guide_source_capture("root", &packet.canonical_pr_url, "probe", "new-head", 6)
            .unwrap()
    );
    assert!(
        !db.get_latest_pr_review_guide_source_capture("root")
            .unwrap()
            .unwrap()
            .complete
    );
}

#[test]
fn failed_retries_consume_the_attempt_bound_without_marking_evidence_complete() {
    let (_dir, db) = open_db();
    let packet = incomplete_packet("base", "head", "pinned source read failed: timeout");
    db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Poller, &packet)
        .unwrap();
    let endpoints = boss_pr_review_sources::PinnedComparison {
        base_sha: "base".to_owned(),
        head_sha: "head".to_owned(),
    };
    for _ in 1..MAX_CAPTURE_ATTEMPTS {
        db.record_pr_review_guide_source_retry_error(&packet.canonical_pr_url, &endpoints)
            .unwrap();
    }
    let stored = db.get_latest_pr_review_guide_source_capture("root").unwrap().unwrap();
    assert_eq!(stored.attempt_count, MAX_CAPTURE_ATTEMPTS);
    assert!(!stored.complete);
    assert!(
        db.select_complete_pr_review_guide_source_capture("root", &packet.canonical_pr_url, "base", "head", 2)
            .unwrap()
    );
}

#[test]
fn retention_keeps_selection_and_recent_history_then_expires_terminal_series() {
    let (dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "source retention");
    for sequence in 1..=9 {
        db.persist_pr_review_guide_source_capture(
            &root,
            sequence,
            PrSourceCaptureTrigger::Poller,
            &packet("base", &format!("head-{sequence}")),
        )
        .unwrap();
    }
    db.select_complete_pr_review_guide_source_capture(
        &root,
        &packet("base", "head-1").canonical_pr_url,
        "base",
        "head-1",
        10,
    )
    .unwrap();
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert_eq!(artifact_count(dir.path()), 6);
    assert_eq!(
        db.get_latest_pr_review_guide_source_capture(&root)
            .unwrap()
            .unwrap()
            .packet
            .head_sha,
        "head-1"
    );
    db.connect()
        .unwrap()
        .execute("UPDATE pr_review_guide_source_comparisons SET captured_at = '1'", [])
        .unwrap();
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert_eq!(
        artifact_count(dir.path()),
        6,
        "active roots preserve evidence regardless of age"
    );
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&root])
        .unwrap();
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert_eq!(artifact_count(dir.path()), 0);
    assert!(db.get_latest_pr_review_guide_source_capture(&root).unwrap().is_none());
    assert_eq!(
        db.connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pr_review_guide_source_series", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn gc_preserves_in_progress_publication_and_live_staging_files() {
    let (dir, db) = open_db();
    let _publisher = packet_store_lock(dir.path(), false).unwrap();
    let relative = publish_packet_artifact(
        dir.path(),
        &packet("base", "head").content_hash().unwrap(),
        &serde_json::to_vec(&packet("base", "head")).unwrap(),
    )
    .unwrap();
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert!(dir.path().join(&relative).exists());
    drop(_publisher);
    let own = dir.path().join(format!("{relative}.{}.1.tmp", std::process::id()));
    let live = dir.path().join(format!("{relative}.1.1.tmp"));
    let dead = dir.path().join(format!("{relative}.2147483647.1.tmp"));
    let unknown = dir.path().join(format!("{relative}.unknown.tmp"));
    for path in [&own, &live, &dead, &unknown] {
        fs::write(path, "staged").unwrap();
    }
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
    assert!(own.exists());
    assert!(live.exists());
    assert!(unknown.exists());
    assert!(!dead.exists());
    assert!(!dir.path().join(relative).exists());
}
use crate::test_support::{create_active_chore, create_product, open_db};
use crate::work::{FakePrStateChecker, PrOpenState};
use boss_pr_review_sources::{ChangeKind, PinnedSource, SourceFile, SourceOmission, SourceSide};
use boss_protocol::{CreateExecutionInput, CreateRevisionInput, ExecutionKind, ExecutionStatus, WorkItemPatch};

fn packet(base: &str, head: &str) -> SourcePacket {
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

fn incomplete_packet(base: &str, head: &str, reason: &str) -> SourcePacket {
    let terminal = !reason.contains("pinned source read failed");
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
    let incomplete = incomplete_packet("base", "head", "pinned source read failed: timeout");
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
    let first_packet = incomplete_packet("base", "head", "pinned source read failed: timeout");
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
    assert!(
        db.persist_pr_review_guide_source_capture("root", 2, PrSourceCaptureTrigger::Poller, &original)
            .unwrap_err()
            .to_string()
            .contains("integrity failure")
    );
}

#[test]
fn diagnostic_read_selects_the_latest_pr_series() {
    let (_dir, db) = open_db();
    let first = packet("base", "first");
    let mut second = packet("base", "second");
    second.canonical_pr_url = "https://github.com/acme/widget/pull/12".to_owned();
    second.pr_number = 12;
    db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &first)
        .unwrap();
    db.persist_pr_review_guide_source_capture("root", 2, PrSourceCaptureTrigger::Creation, &second)
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("UPDATE pr_review_guide_source_series SET updated_at = CASE canonical_pr_url WHEN ?1 THEN '2026-01-01' ELSE '2026-01-02' END", [&first.canonical_pr_url]).unwrap();
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
    db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
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
