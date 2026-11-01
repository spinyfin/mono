//! Review-guide source capture's in-flight claim, driven through
//! `prepare_capture`: a missing or tampered packet artifact is an explicit
//! source failure that permits recollection, and a blocked artifact
//! validation never blocks an unrelated claim or database write.

use std::fs;

use boss_pr_review_sources::{ChangeKind, PinnedSource, SourceFile, SourcePacket};

use crate::test_support::{create_active_chore, create_product, open_db};
use crate::work::{PrSourceCapturePersistOutcome, PrSourceCaptureTrigger, WorkDb};

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

fn assert_recollects(db: &WorkDb, root: &str, packet: &SourcePacket) {
    let endpoints = boss_pr_review_sources::PinnedComparison {
        base_sha: packet.observed_base_sha.clone(),
        head_sha: packet.head_sha.clone(),
    };
    let claim = crate::review_guide_capture::prepare_capture(db, root, &packet.canonical_pr_url, &endpoints, 3, false)
        .unwrap()
        .expect("invalid artifact must permit recollection");
    let error = crate::work::test_support::source_capture_error(db, &packet.canonical_pr_url);
    assert!(error.is_some(), "artifact failure remains visible until recovery");
    db.persist_pr_review_guide_source_capture(root, 3, PrSourceCaptureTrigger::Poller, packet)
        .unwrap();
    drop(claim);
    assert!(
        crate::review_guide_capture::prepare_capture(db, root, &packet.canonical_pr_url, &endpoints, 4, false)
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
            crate::work::test_support::set_before_packet_read_hook(Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }));
            crate::review_guide_capture::prepare_capture(
                &db,
                "root",
                &original.canonical_pr_url,
                &boss_pr_review_sources::PinnedComparison {
                    base_sha: "base".into(),
                    head_sha: "head".into(),
                },
                2,
                false,
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
                false,
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
