//! Shared `[engine-reconcile]` audit-line helper.
//!
//! Several reconciliation sweeps (dead-pid, stale-worker, spawn-ack) and the
//! transient-recovery path all need to append an `[engine-reconcile]` audit
//! line to a work item's description so an operator can see why a chore was
//! reset or resumed. They previously reimplemented the identical
//! get -> extract-description -> format -> update sequence. This module owns
//! that sequence; each call site supplies only its caller-specific reason.

use std::path::Path;

use boss_protocol::WorkItemPatch;

use crate::work::WorkDb;

/// Append an `[engine-reconcile]` audit line to `work_item_id`'s description.
///
/// The emitted line is:
/// `\n[engine-reconcile] epoch {now_epoch_secs}: {reason}.{recovery_note}`
/// where `recovery_note` is ` Uncommitted work backed up to {path}.` when
/// `recovery_patch` is `Some`, and empty otherwise. The caller formats only
/// `reason` (without the trailing period); this helper owns the fetch,
/// description extraction, recovery-note construction, and update.
pub(crate) fn append_reconcile_audit(
    work_db: &WorkDb,
    work_item_id: &str,
    now_epoch_secs: i64,
    reason: &str,
    recovery_patch: Option<&Path>,
) -> anyhow::Result<()> {
    let item = work_db.get_work_item(work_item_id)?;
    let current_desc = match &item {
        boss_protocol::WorkItem::Product(p) => p.description.as_str(),
        boss_protocol::WorkItem::Project(p) => p.description.as_str(),
        boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.description.as_str(),
    };
    let recovery_note = match recovery_patch {
        Some(path) => format!(" Uncommitted work backed up to {}.", path.display()),
        None => String::new(),
    };
    let audit_line = format!("\n[engine-reconcile] epoch {now_epoch_secs}: {reason}.{recovery_note}");
    let new_desc = format!("{current_desc}{audit_line}");
    work_db.update_work_item(
        work_item_id,
        WorkItemPatch {
            description: Some(new_desc),
            ..WorkItemPatch::default()
        },
    )?;
    Ok(())
}

/// Best-effort variant of [`append_reconcile_audit`] with no recovery note:
/// a lookup/update failure is logged via `tracing::warn!` and swallowed,
/// never propagated. Used by transient-recovery where the audit line is a
/// courtesy on top of the resume and must never block it.
pub(crate) fn append_reconcile_audit_best_effort(
    work_db: &WorkDb,
    work_item_id: &str,
    now_epoch_secs: i64,
    reason: &str,
) {
    if let Err(err) = append_reconcile_audit(work_db, work_item_id, now_epoch_secs, reason, None) {
        tracing::warn!(
            work_item_id,
            ?err,
            "transient-recovery: audit append failed (non-fatal)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use boss_protocol::{CreateProjectInput, WorkItem, WorkItemPatch};

    use crate::test_support::{create_active_chore, create_test_product, open_db};
    use crate::work::WorkDb;

    /// A fixed epoch used across the format assertions so the expected
    /// string is deterministic.
    const EPOCH: i64 = 1_700_000_000;

    /// Read `id` back from the DB and return its description, whatever the
    /// [`WorkItem`] variant. Tests assert on this observable string rather
    /// than on the helper's internals.
    fn description_of(db: &WorkDb, id: &str) -> String {
        match db.get_work_item(id).unwrap() {
            WorkItem::Product(p) => p.description,
            WorkItem::Project(p) => p.description,
            WorkItem::Task(t) | WorkItem::Chore(t) => t.description,
        }
    }

    /// Overwrite `id`'s description with `desc` so a test can start from a
    /// known non-empty value and prove the helper appends rather than
    /// replaces.
    fn set_description(db: &WorkDb, id: &str, desc: &str) {
        db.update_work_item(
            id,
            WorkItemPatch {
                description: Some(desc.to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn appends_audit_line_in_exact_format_without_recovery_note() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_active_chore(&db, &product.id, "reap me");
        set_description(&db, &chore, "original body");

        append_reconcile_audit(&db, &chore, EPOCH, "worker pid 123 exited", None).unwrap();

        assert_eq!(
            description_of(&db, &chore),
            "original body\n[engine-reconcile] epoch 1700000000: worker pid 123 exited.",
        );
    }

    #[test]
    fn includes_recovery_note_when_patch_present() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_active_chore(&db, &product.id, "recover me");
        set_description(&db, &chore, "before");

        let patch = Path::new("/var/tmp/backup-abc.patch");
        append_reconcile_audit(&db, &chore, EPOCH, "resumed after transient error", Some(patch)).unwrap();

        assert_eq!(
            description_of(&db, &chore),
            "before\n[engine-reconcile] epoch 1700000000: resumed after transient error. \
             Uncommitted work backed up to /var/tmp/backup-abc.patch.",
        );
    }

    #[test]
    fn appends_rather_than_replaces_existing_description() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_active_chore(&db, &product.id, "keep history");
        set_description(&db, &chore, "line one\nline two");

        // Two successive appends must both survive, in order, on top of the
        // original body — nothing is overwritten.
        append_reconcile_audit(&db, &chore, EPOCH, "first reason", None).unwrap();
        append_reconcile_audit(&db, &chore, EPOCH + 1, "second reason", None).unwrap();

        assert_eq!(
            description_of(&db, &chore),
            "line one\nline two\
             \n[engine-reconcile] epoch 1700000000: first reason.\
             \n[engine-reconcile] epoch 1700000001: second reason.",
        );
    }

    #[test]
    fn appends_to_product_description() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        set_description(&db, &product.id, "product notes");

        append_reconcile_audit(&db, &product.id, EPOCH, "product reason", None).unwrap();

        assert_eq!(
            description_of(&db, &product.id),
            "product notes\n[engine-reconcile] epoch 1700000000: product reason.",
        );
    }

    #[test]
    fn appends_to_project_description() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("some project")
                    .description("project notes")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();

        append_reconcile_audit(&db, &project.id, EPOCH, "project reason", None).unwrap();

        assert_eq!(
            description_of(&db, &project.id),
            "project notes\n[engine-reconcile] epoch 1700000000: project reason.",
        );
    }

    #[test]
    fn best_effort_swallows_lookup_failure() {
        let (_dir, db) = open_db();
        let missing = "task_does_not_exist";

        // Sanity: the strict helper propagates the lookup failure for an
        // unknown id, so the best-effort variant has something real to
        // swallow.
        assert!(append_reconcile_audit(&db, missing, EPOCH, "no such item", None).is_err());

        // Must not panic and must not propagate — a courtesy audit line
        // never blocks the resume it rides on.
        append_reconcile_audit_best_effort(&db, missing, EPOCH, "no such item");
    }
}
