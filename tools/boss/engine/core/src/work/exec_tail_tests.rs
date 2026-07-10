//! Behavior tests for the external-issue-tracker reconciliation methods
//! defined in `exec_tail.rs`.
//!
//! These exercise the public `WorkDb` surface the reconciler relies on —
//! `set_external_ref`, `find_by_external_ref`, `get_task_with_external_ref`,
//! `touch_external_ref_synced_at`, the content-checksum baseline/round-trip
//! helpers, `reconciler_update_name_and_description`,
//! `reconciler_attach_pr_url`, and `clear_external_ref` — and assert on
//! observable outcomes via public query methods rather than internal state.

use serde_json::json;

use super::*;
use crate::test_support::{create_test_chore, create_test_product, open_db};

/// Unwrap the inner `Task` from a `WorkItem` returned by
/// `get_task_with_external_ref`. Chores decode as `WorkItem::Chore`.
fn item_task(item: WorkItem) -> Task {
    match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected a task/chore work item, got {other:?}"),
    }
}

/// `get_task_with_external_ref` surfaces the ref (including the
/// unset-until-synced `synced_at`) on a linked work item. The
/// `set_external_ref` -> `find_by_external_ref` round-trip itself (kind,
/// canonical_id, raw blob, derived github `web_url`) is already covered by
/// `t04::set_and_find_external_ref_round_trip` and
/// `t04::derive_external_ref_web_url_github` — this test only exercises the
/// net-new `get_task_with_external_ref` path.
#[test]
fn set_external_ref_links_and_is_findable() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Reconcile me");

    let raw = json!({ "issue_number": 560, "project_item_id": "PVTI_abc" });
    db.set_external_ref(&chore.id, "github", "spinyfin/mono#560", &raw)
        .unwrap();

    let item = item_task(db.get_task_with_external_ref(&chore.id).unwrap());
    let ext = item
        .external_ref
        .expect("external_ref should be present on the work item");
    assert_eq!(ext.canonical_id, "spinyfin/mono#560");
    assert!(ext.synced_at.is_none(), "synced_at is unset until a reconcile tick");
}

/// `touch_external_ref_synced_at` stamps the sync marker without altering
/// the rest of the binding.
#[test]
fn touch_synced_at_sets_marker() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Sync marker");
    db.set_external_ref(&chore.id, "github", "spinyfin/mono#561", &json!({}))
        .unwrap();

    let before = db.find_by_external_ref("github", "spinyfin/mono#561").unwrap().unwrap();
    assert!(
        before.external_ref.unwrap().synced_at.is_none(),
        "synced_at starts unset"
    );

    db.touch_external_ref_synced_at(&chore.id).unwrap();

    let after = db.find_by_external_ref("github", "spinyfin/mono#561").unwrap().unwrap();
    let ext = after.external_ref.expect("external_ref should still be present");
    assert!(ext.synced_at.is_some(), "synced_at should be set after touch");
    assert_eq!(ext.canonical_id, "spinyfin/mono#561", "binding is otherwise unchanged");
}

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

/// `reconciler_update_name_and_description` overwrites the boss-side name and
/// description and records fresh checksums, all visible via a later query.
#[test]
fn reconciler_update_name_and_description_reflected_in_query() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Original name");
    db.set_external_ref(&chore.id, "github", "spinyfin/mono#562", &json!({}))
        .unwrap();

    let updated = db
        .reconciler_update_name_and_description(
            &chore.id,
            "Synced name",
            "Synced description",
            "Upstream Title",
            "Upstream body",
        )
        .unwrap();
    assert!(updated, "update should report a row was written");

    let item = item_task(db.get_task_with_external_ref(&chore.id).unwrap());
    assert_eq!(item.name, "Synced name", "name should reflect the upstream sync");
    assert_eq!(
        item.description, "Synced description",
        "description should reflect the sync"
    );

    // The recorded baseline matches the new name/description and upstream content.
    let (upstream, boss) = db.reconciler_get_content_checksums(&chore.id).unwrap().unwrap();
    assert_eq!(upstream, content_checksum("Upstream Title", "Upstream body"));
    assert_eq!(boss, content_checksum("Synced name", "Synced description"));
}

/// `reconciler_update_name_and_description` is idempotent-safe: it returns
/// `false` for an unknown work item instead of erroring.
#[test]
fn reconciler_update_name_and_description_missing_row_is_noop() {
    let (_dir, db) = open_db();
    let updated = db
        .reconciler_update_name_and_description("task_does_not_exist", "n", "d", "t", "b")
        .unwrap();
    assert!(!updated, "updating a missing row should report no change");
}

/// `reconciler_attach_pr_url` writes the URL only when the column is empty,
/// preserving any pre-existing (more-trusted) value.
#[test]
fn reconciler_attach_pr_url_only_when_empty() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Attach PR");

    // First attach writes the URL.
    let wrote = db
        .reconciler_attach_pr_url(&chore.id, "https://github.com/spinyfin/mono/pull/1")
        .unwrap();
    assert!(wrote, "should write pr_url onto an empty column");
    let item = item_task(db.get_task_with_external_ref(&chore.id).unwrap());
    assert_eq!(item.pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/1"));

    // Second attach is a no-op — the existing URL is preserved.
    let wrote_again = db
        .reconciler_attach_pr_url(&chore.id, "https://github.com/spinyfin/mono/pull/999")
        .unwrap();
    assert!(!wrote_again, "should not overwrite an already-set pr_url");
    let item = item_task(db.get_task_with_external_ref(&chore.id).unwrap());
    assert_eq!(
        item.pr_url.as_deref(),
        Some("https://github.com/spinyfin/mono/pull/1"),
        "the original pr_url must be preserved"
    );
}

/// `clear_external_ref` retains the canonical id (for future re-binding)
/// while stamping `unbound_at` and clearing `synced_at`. The
/// find_by_external_ref-drops-the-row half of unbinding is already covered
/// by `t04::clear_external_ref_hides_from_find`; this test targets the
/// net-new retention/synced_at assertions visible via
/// `get_task_with_external_ref`.
#[test]
fn clear_external_ref_unbinds_but_retains_canonical_id() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Unbind me");
    db.set_external_ref(&chore.id, "github", "spinyfin/mono#563", &json!({}))
        .unwrap();
    db.touch_external_ref_synced_at(&chore.id).unwrap();

    db.clear_external_ref(&chore.id).unwrap();

    // The canonical id is retained (for re-binding) and synced_at is cleared.
    let item = item_task(db.get_task_with_external_ref(&chore.id).unwrap());
    let ext = item.external_ref.expect("ref columns are retained after clear");
    assert_eq!(ext.canonical_id, "spinyfin/mono#563", "canonical id is retained");
    assert!(ext.unbound_at.is_some(), "unbound_at should be stamped");
    assert!(ext.synced_at.is_none(), "synced_at should be cleared on unbind");
}

/// `clear_external_ref` on a missing row surfaces an error rather than
/// silently succeeding.
#[test]
fn clear_external_ref_missing_row_errors() {
    let (_dir, db) = open_db();
    assert!(
        db.clear_external_ref("task_does_not_exist").is_err(),
        "clearing a non-existent work item should error"
    );
}
