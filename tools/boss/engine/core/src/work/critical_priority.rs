//! The `critical` urgency level: a human-only escape hatch above `high`.
//!
//! Critical priority bypasses the interactive concurrency cap and an operator
//! dispatch pause, so only the human caller tier may set it.
//!
//! This module is the single authority for the *write* rule. The read
//! side — ordering critical rows first, skipping the interactive cap and
//! the operator dispatch pause — lives in `WorkDb::list_ready_executions`
//! and `ExecutionCoordinator::drain_ready_queue`, both keyed on
//! [`is_critical`].
//!
//! The write rule, enforced in the engine and not just the UI:
//!
//! - Setting or raising a row to `critical` is accepted only from the
//!   human caller tier (`last_status_actor = 'human'`), the same tier
//!   distinction `update_work_item_as_actor` already records. The
//!   coordinator session's CLI tier (`'boss'`), engine sweeps
//!   (`'engine'`), Boothby, and — at the RPC handler, where the worker
//!   registry classification is known — worker panes are refused with
//!   [`critical_refusal`].
//! - Engine-internal creates go through the plain `create_*` /
//!   `insert_*_in_tx` entry points, which resolve to the `'engine'` actor
//!   and therefore can never mint a critical row; only the
//!   `*_as_actor` variants the RPC handlers call can, and only for a
//!   human.
//! - Derived rows never inherit `critical`: a follow-up or a chore minted
//!   from a merged revision takes [`inherited_priority`] of its source,
//!   which clamps `critical` to `high`.
//! - Lowering from `critical` is an ordinary priority change, allowed for
//!   any caller that can already change priority.

use anyhow::Result;
use boss_protocol::LAST_STATUS_ACTOR_HUMAN;
use rusqlite::{Connection, params};

/// The stored `tasks.priority` value for the critical urgency.
pub const PRIORITY_CRITICAL: &str = "critical";

/// The level a derived row receives when its source is critical.
const INHERITED_FROM_CRITICAL: &str = "high";

/// `true` when `priority` is the critical urgency.
pub fn is_critical(priority: &str) -> bool {
    priority == PRIORITY_CRITICAL
}

/// The error every refused critical write surfaces. Names the caller
/// tier so the error identifies which automated caller attempted the write,
/// and says what a human can do instead.
pub fn critical_refusal(caller_tier: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "priority `critical` is reserved for the human owner: refused for caller tier `{caller_tier}`. \
         Only a human running `boss task update <id> --priority critical` (or `boss chore update`, \
         or the Boss app's priority picker) from their own session may set it; `high` is the \
         highest level an automated caller may request, and lowering from `critical` is always allowed."
    )
}

/// Gate a priority write. `requested` is the normalized incoming value,
/// `current` the row's existing priority (`None` for a create), `actor`
/// the resolved caller tier. Only a *raise to* `critical` is gated: an
/// already-critical row being re-asserted as critical is not a raise,
/// and every other level is unrestricted.
pub fn authorize_priority_write(requested: &str, current: Option<&str>, actor: &str) -> Result<()> {
    if !is_critical(requested) || current.is_some_and(is_critical) || actor == LAST_STATUS_ACTOR_HUMAN {
        return Ok(());
    }
    Err(critical_refusal(actor))
}

/// Handler-side companion to [`authorize_priority_write`] for the one
/// tier the DB layer cannot see: a socket peer the worker registry
/// classifies as a worker pane. `resolve_status_actor` reports such a
/// peer as `'human'` (worker panes are not descendants of the
/// coordinator pid), so the handler must refuse before the actor string
/// ever reaches the DB.
pub fn refuse_worker_critical_raise(
    requested: Option<&str>,
    current: Option<&str>,
    peer_is_worker: bool,
) -> Result<()> {
    if !peer_is_worker {
        return Ok(());
    }
    let Some(requested) = requested.map(|p| p.trim().to_ascii_lowercase()) else {
        return Ok(());
    };
    if !is_critical(&requested) || current.is_some_and(is_critical) {
        return Ok(());
    }
    Err(critical_refusal("worker"))
}

/// Priority for a row derived from `source` (a follow-up chore, a chore
/// minted when a merged revision is re-filed). Never `critical`: the
/// escape hatch is per-row and human-granted, so nothing automated may
/// propagate it.
pub fn inherited_priority(source: &str) -> String {
    if is_critical(source) {
        INHERITED_FROM_CRITICAL.to_owned()
    } else {
        source.to_owned()
    }
}

/// Record who raised `task_id` to critical and when. Used by the create
/// path after the row is inserted; the update path writes the same two
/// columns in its single `UPDATE`.
pub(crate) fn stamp_create_provenance(conn: &Connection, task_id: &str, actor: &str, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET critical_set_by = ?2, critical_set_at = ?3 WHERE id = ?1",
        params![task_id, actor, now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::create_test_product;
    use crate::work::WorkDb;
    use boss_protocol::{
        CreateChoreInput, CreateInvestigationInput, LAST_STATUS_ACTOR_BOOTHBY, LAST_STATUS_ACTOR_BOSS,
        LAST_STATUS_ACTOR_ENGINE, Task, WorkItem, WorkItemPatch,
    };

    fn open_db() -> (tempfile::TempDir, WorkDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        (dir, db)
    }

    fn task_of(item: WorkItem) -> Task {
        match item {
            WorkItem::Task(t) | WorkItem::Chore(t) => t,
            other => panic!("expected a task/chore, got {other:?}"),
        }
    }

    fn priority_patch(priority: &str) -> WorkItemPatch {
        WorkItemPatch {
            priority: Some(priority.to_owned()),
            ..WorkItemPatch::default()
        }
    }

    #[test]
    fn automated_tier_callers_cannot_raise_to_critical() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("c")
                    .build(),
            )
            .unwrap();
        for actor in [
            LAST_STATUS_ACTOR_BOSS,
            LAST_STATUS_ACTOR_ENGINE,
            LAST_STATUS_ACTOR_BOOTHBY,
        ] {
            let err = db
                .update_work_item_as_actor(&chore.id, priority_patch("critical"), actor)
                .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("reserved for the human owner"), "{actor}: {msg}");
            assert!(msg.contains(&format!("`{actor}`")), "{actor}: {msg}");
        }
        let err = db.update_work_item(&chore.id, priority_patch("CRITICAL")).unwrap_err();
        assert!(err.to_string().contains("`engine`"), "{err}");
        let row = task_of(db.get_work_item(&chore.id).unwrap());
        assert_eq!(row.priority, "medium", "a refused raise must leave the row untouched");
        assert_eq!(row.critical_set_by, None);
    }

    #[test]
    fn human_tier_caller_raises_to_critical_with_provenance_and_anyone_can_lower() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("c")
                    .build(),
            )
            .unwrap();
        let raised = db
            .update_work_item_as_actor(&chore.id, priority_patch("CRITICAL"), LAST_STATUS_ACTOR_HUMAN)
            .unwrap();
        let raised = task_of(raised);
        assert_eq!(raised.priority, "critical");
        assert_eq!(raised.critical_set_by.as_deref(), Some("human"));
        assert!(raised.critical_set_at.is_some(), "the raise must be timestamped");

        // Re-asserting critical from an automated tier is not a raise.
        db.update_work_item_as_actor(&chore.id, priority_patch("critical"), LAST_STATUS_ACTOR_BOSS)
            .unwrap();

        // Lowering is an ordinary priority change for any caller, and
        // clears the provenance so it never outlives the level.
        let lowered = db
            .update_work_item_as_actor(&chore.id, priority_patch("high"), LAST_STATUS_ACTOR_BOSS)
            .unwrap();
        let lowered = task_of(lowered);
        assert_eq!(lowered.priority, "high");
        assert_eq!(lowered.critical_set_by, None);
        assert_eq!(lowered.critical_set_at, None);
    }

    #[test]
    fn creates_mint_critical_only_for_the_human_actor() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let critical_chore = || {
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("c")
                .priority("critical")
                .build()
        };
        let critical_investigation = || {
            CreateInvestigationInput::builder()
                .product_id(product.id.clone())
                .name("i")
                .priority("critical")
                .build()
        };

        // The plain entry points are what sweeps, automations and
        // follow-up minting use — they resolve to the engine actor.
        let err = db.create_chore(critical_chore()).unwrap_err().to_string();
        assert!(err.contains("`engine`"), "{err}");
        let err = db
            .create_investigation(critical_investigation())
            .unwrap_err()
            .to_string();
        assert!(err.contains("`engine`"), "{err}");
        let err = db
            .create_chore_as_actor(critical_chore(), LAST_STATUS_ACTOR_BOSS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`boss`"), "{err}");

        let chore = db
            .create_chore_as_actor(critical_chore(), LAST_STATUS_ACTOR_HUMAN)
            .unwrap();
        assert_eq!(chore.priority, "critical");
        assert_eq!(chore.critical_set_by.as_deref(), Some("human"));
        assert!(chore.critical_set_at.is_some());
        let investigation = db
            .create_investigation_as_actor(critical_investigation(), LAST_STATUS_ACTOR_HUMAN)
            .unwrap();
        assert_eq!(investigation.priority, "critical");
        assert_eq!(investigation.critical_set_by.as_deref(), Some("human"));

        // Non-critical creates are unrestricted for every actor.
        db.create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("h")
                .priority("high")
                .build(),
        )
        .unwrap();
    }

    #[test]
    fn worker_peer_refusal_only_bites_on_a_raise_to_critical() {
        assert!(refuse_worker_critical_raise(Some("critical"), Some("medium"), false).is_ok());
        assert!(refuse_worker_critical_raise(Some("high"), Some("medium"), true).is_ok());
        assert!(refuse_worker_critical_raise(None, Some("medium"), true).is_ok());
        assert!(refuse_worker_critical_raise(Some("critical"), Some("critical"), true).is_ok());
        assert!(refuse_worker_critical_raise(Some("high"), Some("critical"), true).is_ok());
        let err = refuse_worker_critical_raise(Some(" Critical "), Some("high"), true).unwrap_err();
        assert!(err.to_string().contains("`worker`"), "{err}");
        let err = refuse_worker_critical_raise(Some("critical"), None, true).unwrap_err();
        assert!(err.to_string().contains("`worker`"), "{err}");
    }

    #[test]
    fn derived_rows_never_inherit_critical() {
        assert_eq!(inherited_priority("critical"), "high");
        assert_eq!(inherited_priority("high"), "high");
        assert_eq!(inherited_priority("low"), "low");
    }
}
