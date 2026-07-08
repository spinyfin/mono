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
