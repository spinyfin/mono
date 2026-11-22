//! Shared reconciliation audit writers. Reconciliation and parking narratives
//! are recorded in `task_status_audit` (property `reconciliation`) without
//! changing the authored brief or recovery interface.

use crate::work::WorkDb;

/// Record an `[engine-reconcile]` narrative in the work-item audit ledger.
///
/// The emitted line is:
/// `[engine-reconcile] epoch {now_epoch_secs}: {reason}.`
/// The caller formats only `reason` (without the trailing period); this helper
/// owns formatting; storage owns the durable ledger write.
pub fn append_reconcile_audit(
    work_db: &WorkDb,
    work_item_id: &str,
    now_epoch_secs: i64,
    reason: &str,
) -> anyhow::Result<()> {
    let audit_line = format!("{ENGINE_RECONCILE_MARKER} epoch {now_epoch_secs}: {reason}.");
    work_db.record_reconcile_audit(work_item_id, now_epoch_secs, &audit_line)
}

pub use boss_engine_storage_values::audit_text::*;

/// Record a `[parking]` audit line in the work-item audit ledger: one line of
/// the un-park reconciliation trail.
///
/// The emitted line is `[parking] epoch {now_epoch_secs}: {step}: {detail}.`
/// where `step` is the reconciliation step's stable name. The caller formats
/// `detail` (without the trailing period); this helper owns the marker and the
/// shape, so the vocabulary stays registered in [`ENGINE_AUDIT_LINE_PREFIXES`].
pub fn append_parking_audit(
    work_db: &WorkDb,
    work_item_id: &str,
    now_epoch_secs: i64,
    step: &str,
    detail: &str,
) -> anyhow::Result<()> {
    work_db.record_reconcile_audit(
        work_item_id,
        now_epoch_secs,
        &format!("{PARKING_MARKER} epoch {now_epoch_secs}: {step}: {detail}."),
    )
}

/// Append `line` verbatim to `work_item_id`'s description for description-trail
/// writers such as [`crate::WorkerCompletionHandler::record_deferred_scope_item`].
/// [`WorkDb::append_work_item_description_line`] owns the update so
/// the proposal path can append its post-commit line without calling back
/// into engine core.
pub fn append_description_line(work_db: &WorkDb, work_item_id: &str, line: &str) -> anyhow::Result<()> {
    work_db.append_work_item_description_line(work_item_id, line)
}

/// Best-effort variant of [`append_reconcile_audit`]:
/// a lookup/update failure is logged via `tracing::warn!` and swallowed,
/// never propagated. Used by transient-recovery where the audit line is a
/// courtesy on top of the resume and must never block it.
pub fn append_reconcile_audit_best_effort(work_db: &WorkDb, work_item_id: &str, now_epoch_secs: i64, reason: &str) {
    if let Err(err) = append_reconcile_audit(work_db, work_item_id, now_epoch_secs, reason) {
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

    fn audit_of(db: &WorkDb, id: &str) -> String {
        db.list_task_field_audit(id)
            .unwrap()
            .into_iter()
            .filter(|row| row.property == crate::work::RECONCILIATION_PROPERTY)
            .map(|row| row.detail.unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Overwrite `id`'s description with `desc` so a test can start from a
    /// known non-empty value and prove reconciliation leaves it unchanged.
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
    fn records_audit_without_changing_the_brief() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_active_chore(&db, &product.id, "reap me");
        set_description(&db, &chore, "original body");

        append_reconcile_audit(&db, &chore, EPOCH, "worker pid 123 exited").unwrap();
        assert_eq!(description_of(&db, &chore), "original body");

        assert_eq!(
            audit_of(&db, &chore),
            "[engine-reconcile] epoch 1700000000: worker pid 123 exited.",
        );
    }

    #[test]
    fn repeated_reconciliation_keeps_history_out_of_the_brief() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_active_chore(&db, &product.id, "keep history");
        set_description(&db, &chore, "line one\nline two");

        // Both events survive in order; the authored brief stays byte-for-byte intact.
        append_reconcile_audit(&db, &chore, EPOCH, "first reason").unwrap();
        append_reconcile_audit(&db, &chore, EPOCH + 1, "second reason").unwrap();
        assert_eq!(description_of(&db, &chore), "line one\nline two");

        assert_eq!(
            audit_of(&db, &chore),
            "[engine-reconcile] epoch 1700000000: first reason.\
             \n[engine-reconcile] epoch 1700000001: second reason.",
        );
    }

    #[test]
    fn records_product_history_without_changing_description() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        set_description(&db, &product.id, "product notes");

        append_reconcile_audit(&db, &product.id, EPOCH, "product reason").unwrap();
        assert_eq!(description_of(&db, &product.id), "product notes");

        assert_eq!(
            audit_of(&db, &product.id),
            "[engine-reconcile] epoch 1700000000: product reason.",
        );
    }

    #[test]
    fn records_project_history_without_changing_description() {
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

        append_reconcile_audit(&db, &project.id, EPOCH, "project reason").unwrap();
        assert_eq!(description_of(&db, &project.id), "project notes");

        assert_eq!(
            audit_of(&db, &project.id),
            "[engine-reconcile] epoch 1700000000: project reason.",
        );
    }

    #[test]
    fn parking_audit_line_has_the_registered_marker_and_shape() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_active_chore(&db, &product.id, "unparked");
        set_description(&db, &chore, "brief");

        append_parking_audit(&db, &chore, EPOCH, "refresh_pr", "PR is still open").unwrap();

        assert_eq!(description_of(&db, &chore), "brief");
        assert_eq!(
            audit_of(&db, &chore),
            "[parking] epoch 1700000000: refresh_pr: PR is still open."
        );
    }

    #[test]
    fn best_effort_swallows_lookup_failure() {
        let (_dir, db) = open_db();
        let missing = "task_does_not_exist";

        // Sanity: the strict helper propagates the lookup failure for an
        // unknown id, so the best-effort variant has something real to
        // swallow.
        assert!(append_reconcile_audit(&db, missing, EPOCH, "no such item").is_err());

        // Must not panic and must not propagate — a courtesy audit line
        // never blocks the resume it rides on.
        append_reconcile_audit_best_effort(&db, missing, EPOCH, "no such item");
    }

    #[test]
    fn strip_engine_audit_lines_drops_doc_detector_reconcile_and_deferred_scope() {
        let description = "Implement the widget importer.\n\
             \n\
             [doc-detector] no doc pointer auto-populated for this PR because it did not \
             touch exactly one docs/designs|investigations|postmortems file.\n\
             [engine-reconcile] epoch 1700000000: worker pid 123 exited.\n\
             [deferred-scope] epoch 1700000001: summary=\"broker\" reason=\"needs a pipeline\"\n";
        assert_eq!(
            strip_engine_audit_lines(description).trim(),
            "Implement the widget importer."
        );
    }

    #[test]
    fn strip_engine_audit_lines_treats_audit_only_description_as_empty() {
        let description = "\n[doc-detector] no doc pointer auto-populated for this PR.\n\
             [engine-reconcile] epoch 1700000000: resumed after transient error.\n";
        assert!(strip_engine_audit_lines(description).trim().is_empty());
    }

    #[test]
    fn is_engine_audit_line_matches_indented_prefixes() {
        assert!(is_engine_audit_line("  [doc-detector] leftover"));
        assert!(is_engine_audit_line("[engine-reconcile] epoch 1: x."));
        assert!(is_engine_audit_line("[deferred-scope] summary=\"x\" reason=\"y\""));
        assert!(is_engine_audit_line("[pr-review-skip] epoch 1: reason=pure_rebase"));
        assert!(is_engine_audit_line(
            "[postmortem-followups] review surfaced no uncompleted work."
        ));
        assert!(!is_engine_audit_line("Implement the widget importer."));
        assert!(!is_engine_audit_line("mentions [doc-detector] in prose"));
    }

    /// The explicitly listed marker constants must match [`ENGINE_AUDIT_LINE_PREFIXES`],
    /// covering both active `append_description_line` writers and legacy markers
    /// (`ENGINE_RECONCILE_MARKER`, `PARKING_MARKER`) that now write only to
    /// `task_status_audit` so legacy description text is still recognised and
    /// stripped. New writers must be added to both lists; this test does not
    /// discover writers or detect unregistered markers elsewhere in the code.
    #[test]
    fn engine_audit_line_prefixes_cover_every_known_writer() {
        let known_writer_markers: &[&str] = &[
            DOC_DETECTOR_MARKER,
            ENGINE_RECONCILE_MARKER,
            crate::deferred_scope::DEFERRED_SCOPE_MARKER,
            PR_REVIEW_SKIP_MARKER,
            crate::postmortem_followups::PROCESSED_MARKER,
            PARKING_MARKER,
        ];
        for marker in known_writer_markers {
            assert!(
                ENGINE_AUDIT_LINE_PREFIXES.contains(marker),
                "writer marker {marker:?} is missing from ENGINE_AUDIT_LINE_PREFIXES"
            );
        }
        assert_eq!(
            ENGINE_AUDIT_LINE_PREFIXES.len(),
            known_writer_markers.len(),
            "ENGINE_AUDIT_LINE_PREFIXES has an entry with no corresponding pinned writer marker"
        );
    }
}
