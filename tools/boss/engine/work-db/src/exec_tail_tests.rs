//! Private checksum tests for the crate-private `content_checksum` helper
//! defined in `exec_tail.rs`.
//!
//! Public WorkDb/event-bus behavior for the reconciler lives in the
//! external `storage_behavior_test` target.

use super::*;
use crate::test_support::{create_test_chore, create_test_product, open_db};

/// Content-checksum baseline: nothing stored until a baseline is written;
/// after `reconciler_set_content_checksums_baseline` the round-trip returns
/// the exact SHA-256 of the canonical title+body pairs.
#[test]
fn content_checksum_baseline_round_trip() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Drift baseline");

    // Pre-migration items have NULL checksums → get returns None.
    assert!(
        db.reconciler_get_content_checksums(&chore.id).unwrap().is_none(),
        "checksums should be absent before a baseline is set"
    );

    db.reconciler_set_content_checksums_baseline(
        &chore.id,
        "Upstream Title",
        "Upstream body",
        "Boss Name",
        "Boss description",
    )
    .unwrap();

    let (upstream, boss) = db
        .reconciler_get_content_checksums(&chore.id)
        .unwrap()
        .expect("checksums should be present after baseline");
    assert_eq!(
        upstream,
        content_checksum("Upstream Title", "Upstream body"),
        "upstream checksum must match SHA-256 of canonical title+body"
    );
    assert_eq!(
        boss,
        content_checksum("Boss Name", "Boss description"),
        "boss checksum must match SHA-256 of canonical name+description"
    );
}

/// Change detection: re-baselining with different upstream content yields a
/// different stored checksum, which is how the reconciler notices drift.
#[test]
fn content_checksum_detects_change() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Drift change");

    db.reconciler_set_content_checksums_baseline(&chore.id, "Title v1", "Body v1", "Name v1", "Desc v1")
        .unwrap();
    let (upstream_v1, _) = db.reconciler_get_content_checksums(&chore.id).unwrap().unwrap();

    db.reconciler_set_content_checksums_baseline(&chore.id, "Title v2", "Body v2", "Name v1", "Desc v1")
        .unwrap();
    let (upstream_v2, _) = db.reconciler_get_content_checksums(&chore.id).unwrap().unwrap();

    assert_ne!(
        upstream_v1, upstream_v2,
        "a changed upstream title/body must produce a different checksum"
    );
}
