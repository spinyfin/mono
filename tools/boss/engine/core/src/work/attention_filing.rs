//! The shared filing boundary for `work_attention_items`.
//!
//! Every producer that raises an operator-visible signal passes through one
//! of the helpers here, so two invariants hold no matter which filer is
//! used:
//!
//! 1. **A kind with no declared lifecycle is surfaced.**
//!    [`warn_if_lifecycle_undeclared`] runs on every filing path — the
//!    work-item upsert, the execution-scoped `create_attention_item` /
//!    `insert_attention_item_row` (which `finish_execution_run` also routes
//!    through), and the bespoke raw-INSERT helpers for `repo_unresolved` /
//!    `revision_archived`. The original defect was that raising went through
//!    shared plumbing while lowering was left to each producer to remember;
//!    a guard wired into only one of the filers would reproduce it in
//!    miniature.
//! 2. **A re-raise onto an already-open row is stamped.**
//!    [`reraise_open_work_item_attention`] and
//!    [`reraise_open_execution_attention`] are the dedup half of the
//!    "one open row per (scope, kind)" contract every filer implements, and
//!    they refresh `last_raised_at` while they are there. See
//!    [`crate::work::attention_reconcile`] for why the reconciler must
//!    compare its evidence against that column rather than `created_at`.

use super::*;

/// Emit a trace warning when an attention `kind` has no entry in
/// [`crate::attention_lifecycle::ATTENTION_LIFECYCLES`].
///
/// The original defect was structural, not local: raising a signal went
/// through shared, well-tested plumbing while lowering it was left to each
/// producer to remember, so kinds added later simply never got a resolution
/// path and could be raised but never lowered. The registry is the fix; this
/// is its enforcement at the filing boundary. Deliberately a warning rather
/// than an error — refusing to file would trade an un-clearable signal for a
/// missing one, which is strictly worse — and deliberately not a `panic!`,
/// since callers file attentions on best-effort paths that must not abort.
///
/// This is also the only check that catches a *wholly new* kind constant:
/// `every_attention_kind_constant_in_the_crate_is_registered` compares a
/// hand-maintained list against the registry, so a constant added to neither
/// slips past it. A filing call cannot be forgotten the same way.
pub fn warn_if_lifecycle_undeclared(kind: &str) {
    if crate::attention_lifecycle::lifecycle_for(kind).is_none() {
        tracing::warn!(
            attention_kind = kind,
            "attention filed with no declared lifecycle — add an entry to \
             crate::attention_lifecycle::ATTENTION_LIFECYCLES saying what lowers it (ClearedBy::HumanDecision \
             is a valid answer; leaving it undeclared means it can never be lowered automatically)",
        );
    }
}

/// Record that an already-`open` attention row's condition tripped again.
///
/// Every filer deduplicates onto the open row rather than inserting a
/// second one, which is the right UI behaviour — an operator wants one
/// entry per live condition, not one per occurrence. But it means
/// `created_at` records the *first* occurrence forever, and the reconciler's
/// "evidence must postdate the signal" rule would then accept evidence that
/// predates the current occurrence. Concretely for `pane_death_reconcile`:
/// the pane dies (t0); the orphan sweep redispatches and a run starts (t1);
/// the replacement pane dies too (t2), no new row is written — and a sweep
/// anchored on t0 accepts the t1 run start and resolves a signal whose
/// condition is true right now.
///
/// Stamping a separate `last_raised_at` (rather than mutating `created_at`)
/// keeps the "open since" fact the surfaces already show intact, while
/// giving the reconciler the occurrence timestamp it actually needs.
fn stamp_attention_reraise(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE work_attention_items SET last_raised_at = ?2 WHERE id = ?1",
        params![id, now_string()],
    )?;
    Ok(())
}

/// The id of the open `kind` row for `work_item_id`, stamping its re-raise,
/// or `None` when there is none and the caller should insert.
///
/// Callers must run [`warn_if_lifecycle_undeclared`] themselves before
/// inserting; this helper is only the dedup lookup.
pub(crate) fn reraise_open_work_item_attention(
    conn: &Connection,
    work_item_id: &str,
    kind: &str,
) -> Result<Option<String>> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM work_attention_items
             WHERE work_item_id = ?1 AND kind = ?2 AND status = 'open'
             ORDER BY created_at ASC, id ASC
             LIMIT 1",
            params![work_item_id, kind],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = &existing {
        stamp_attention_reraise(conn, id)?;
    }
    Ok(existing)
}

/// The execution-scoped counterpart of [`reraise_open_work_item_attention`].
/// Used by the nudge breaker, whose dedup check is "does this execution
/// already have a non-resolved item of this kind".
pub(crate) fn reraise_open_execution_attention(
    conn: &Connection,
    execution_id: &str,
    kind: &str,
) -> Result<Option<String>> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM work_attention_items
             WHERE execution_id = ?1 AND kind = ?2 AND status != 'resolved'
             ORDER BY created_at ASC, id ASC
             LIMIT 1",
            params![execution_id, kind],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = &existing {
        stamp_attention_reraise(conn, id)?;
    }
    Ok(existing)
}

impl WorkDb {
    /// Stamp a re-raise onto the open execution-scoped `kind` item for
    /// `execution_id`, returning its id when one existed.
    ///
    /// The nudge breaker files at most one item per execution per kind and
    /// silently skips filing when one is already open; calling this instead
    /// of a bare "is one already open?" read is what keeps the reconciler's
    /// evidence rule honest for a breaker that trips repeatedly on the same
    /// execution. See [`reraise_open_execution_attention`].
    pub(crate) fn reraise_open_execution_attention(&self, execution_id: &str, kind: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        reraise_open_execution_attention(&conn, execution_id, kind)
    }
}
