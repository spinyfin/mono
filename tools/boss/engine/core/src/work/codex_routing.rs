//! Percentage-based Codex dispatch routing (experiment, operator-controlled).
//!
//! One persisted integer (0-100) decides what share of ordinary,
//! `standard`-reasoning implementation work (`task_implementation`,
//! `chore_implementation`, `revision_implementation`) dispatches to the
//! `codex` driver instead of the row's normal default. Assignment is a
//! deterministic hash of the work item's own id against the threshold —
//! not a coin flip per dispatch attempt — so the same row always lands on
//! the same driver across retries, redispatch, and recovery. A row with an
//! explicit `driver` set always wins over the percentage; only rows that
//! expressed no preference are eligible.
//!
//! The decision is computed once, at [`crate::work::revision_helpers::insert_execution`]
//! time, and persisted per-execution in `execution_driver_decisions` — see
//! [`record_execution_driver_decision`]. [`WorkDb::get_execution_driver_decision`]
//! is the read side both real dispatch paths ([`crate::work::driver_lookup`]'s
//! events-socket resolver and [`crate::runner::worker_spawn`]'s spawn-config
//! resolver) consult so a percentage-assigned row is actually routed to
//! Codex, not just recorded as an inert audit trail.

use sha2::{Digest, Sha256};

use super::*;

/// Metadata KV key for the persisted percentage. Same shape as
/// `dispatch_concurrency_limit` (see `app/handler_helpers.rs`): a plain
/// integer stored as text, read fresh at each decision point rather than
/// cached, so a change takes effect on the very next execution created —
/// without disturbing any execution already dispatched (a decision already
/// recorded on an existing row is never recomputed).
const METADATA_KEY_CODEX_DISPATCH_PERCENTAGE: &str = "codex_dispatch_percentage";

/// The one alternate driver this experiment routes to. Deliberately a
/// single hardcoded slug, not a configurable list — see the module doc and
/// the task's non-goals (no multi-driver weighting).
pub(crate) const CODEX_DRIVER_SLUG: &str = "codex";

/// `execution_driver_decisions.reason` values. A row's explicit `driver`
/// column always produces `EXPLICIT`; an eligible row with no explicit
/// driver produces `PERCENTAGE` regardless of which side of the threshold
/// it landed on (so "rolled but stayed on the default driver" is still
/// distinguishable from "never eligible"); everything else is `DEFAULT`.
pub(crate) const REASON_EXPLICIT: &str = "explicit";
pub(crate) const REASON_PERCENTAGE: &str = "percentage";
pub(crate) const REASON_DEFAULT: &str = "default";

/// The routing decision for one execution. `driver` is `None` when the
/// decision does not override the row's normal default → product default →
/// engine default resolution (an ineligible row, or a percentage roll that
/// landed on "stay default"). `percentage_at_decision` is only set for
/// `REASON_PERCENTAGE`, so a later analysis can tell which threshold a
/// given row's roll was measured against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverDecision {
    pub driver: Option<String>,
    pub reason: &'static str,
    pub percentage_at_decision: Option<u8>,
}

impl DriverDecision {
    fn explicit(driver: String) -> Self {
        Self {
            driver: Some(driver),
            reason: REASON_EXPLICIT,
            percentage_at_decision: None,
        }
    }

    fn default_no_override() -> Self {
        Self {
            driver: None,
            reason: REASON_DEFAULT,
            percentage_at_decision: None,
        }
    }

    fn percentage(driver: Option<String>, percentage: u8) -> Self {
        Self {
            driver,
            reason: REASON_PERCENTAGE,
            percentage_at_decision: Some(percentage),
        }
    }
}

/// Whether `kind` falls in the narrow eligible slice: ordinary coding work
/// only. A whitelist, not a blocklist, so a future `ExecutionKind` variant
/// defaults to ineligible until someone deliberately opts it in — keeping
/// the experiment's slice from silently widening (design doc: "keep the
/// slice narrow").
fn is_percentage_eligible(kind: ExecutionKind) -> bool {
    matches!(
        kind,
        ExecutionKind::TaskImplementation | ExecutionKind::ChoreImplementation | ExecutionKind::RevisionImplementation
    )
}

/// Deterministic hash of `work_item_id` into `[0, 100)`. SHA-256 rather
/// than `std::hash::Hasher` so the mapping is stable across engine
/// versions/restarts, not just within one process — the whole point is
/// that the same row always gets the same answer. Truncation to the first
/// 8 bytes (big-endian) before `% 100` introduces negligible bias for this
/// non-adversarial use.
fn hash_bucket(work_item_id: &str) -> u8 {
    let digest = Sha256::digest(work_item_id.as_bytes());
    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&digest[..8]);
    (u64::from_be_bytes(first8) % 100) as u8
}

/// Read the persisted Codex dispatch percentage using an already-open
/// `Connection`. Callers holding a `WorkDb::connect()` guard (e.g.
/// `insert_execution`, mid-transaction) MUST use this instead of
/// `WorkDb::get_metadata`, which opens its own guard on the same
/// non-reentrant mutex and would deadlock.
///
/// Absent or unparseable values default to `0` — the safe "experiment is
/// off" state, matching the requirement that a fresh install sends
/// literally nothing to Codex until an operator opts in. Clamped to
/// `0..=100` defensively in case of hand-edited state.
pub(crate) fn load_codex_dispatch_percentage_conn(conn: &Connection) -> Result<u8> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![METADATA_KEY_CODEX_DISPATCH_PERCENTAGE],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|v| v.min(100) as u8)
        .unwrap_or(0))
}

impl WorkDb {
    /// `load_codex_dispatch_percentage_conn`, but opens its own connection.
    /// For callers (RPC handlers, CLI) that are not already inside a
    /// `connect()` guard.
    pub fn get_codex_dispatch_percentage(&self) -> Result<u8> {
        let conn = self.connect()?;
        load_codex_dispatch_percentage_conn(&conn)
    }

    /// Persist the Codex dispatch percentage, clamped to `0..=100`. Takes
    /// effect on the next execution created — never touches an execution
    /// already dispatched, exactly like the interactive concurrency cap.
    /// Returns the clamped value actually stored.
    pub fn set_codex_dispatch_percentage(&self, percentage: u8) -> Result<u8> {
        let applied = percentage.min(100);
        self.set_metadata(METADATA_KEY_CODEX_DISPATCH_PERCENTAGE, &applied.to_string())?;
        Ok(applied)
    }

    /// Look up the recorded routing decision for `execution_id`, or `None`
    /// when no row exists (an execution created before this feature
    /// landed, or a kind this module never records for). Callers must not
    /// already hold a `connect()` guard.
    pub(crate) fn get_execution_driver_decision(&self, execution_id: &str) -> Result<Option<DriverDecision>> {
        let conn = self.connect()?;
        get_execution_driver_decision_conn(&conn, execution_id)
    }
}

/// `WorkDb::get_execution_driver_decision`, but reusing an already-open
/// `Connection` (see `load_codex_dispatch_percentage_conn`'s doc for why
/// this split exists).
pub(crate) fn get_execution_driver_decision_conn(
    conn: &Connection,
    execution_id: &str,
) -> Result<Option<DriverDecision>> {
    conn.query_row(
        "SELECT driver, reason, percentage_at_decision
           FROM execution_driver_decisions
          WHERE execution_id = ?1",
        params![execution_id],
        |row| {
            let reason_raw: String = row.get(1)?;
            let reason = match reason_raw.as_str() {
                REASON_EXPLICIT => REASON_EXPLICIT,
                REASON_PERCENTAGE => REASON_PERCENTAGE,
                _ => REASON_DEFAULT,
            };
            Ok(DriverDecision {
                driver: row.get(0)?,
                reason,
                percentage_at_decision: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Decide which driver governs a newly-created execution row and why.
/// Called once, from `insert_execution`, using the same open `Connection`
/// (never `WorkDb::connect()` — see `load_codex_dispatch_percentage_conn`).
///
/// Precedence:
/// 1. The work item's own explicit `driver` column always wins.
/// 2. Otherwise, if `kind` is in the eligible slice
///    ([`is_percentage_eligible`]) and the work item's `reasoning` is
///    literally `standard` (NULL/legacy rows are NOT treated as eligible —
///    keeps the slice narrow), roll the work item's own id against the
///    current percentage.
/// 3. Otherwise, no override — the row falls through to the normal
///    default → product default → engine default resolution.
pub(crate) fn decide_execution_driver(
    conn: &Connection,
    work_item_id: &str,
    kind: ExecutionKind,
) -> Result<DriverDecision> {
    let row: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT driver, reasoning FROM tasks WHERE id = ?1",
            params![work_item_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((explicit_driver, reasoning)) = row else {
        // Not a `tasks` row at all (e.g. an `answer_agent` execution bound
        // to a comment id) — nothing to route.
        return Ok(DriverDecision::default_no_override());
    };
    if let Some(driver) = explicit_driver.filter(|s| !s.trim().is_empty()) {
        return Ok(DriverDecision::explicit(driver));
    }
    if !is_percentage_eligible(kind) || reasoning.as_deref() != Some("standard") {
        return Ok(DriverDecision::default_no_override());
    }
    let percentage = load_codex_dispatch_percentage_conn(conn)?;
    // `hash_bucket` is in `[0, 100)`; `bucket < percentage` means exactly
    // `percentage` of the `[0, 100)` space is true, e.g. percentage == 0
    // matches no bucket at all (0 is never < 0) — zero really is zero, no
    // off-by-one leakage.
    let assigned_codex = hash_bucket(work_item_id) < percentage;
    Ok(DriverDecision::percentage(
        assigned_codex.then(|| CODEX_DRIVER_SLUG.to_owned()),
        percentage,
    ))
}

/// Persist `decision` for `execution_id`. Called once, right after the
/// `work_executions` row is inserted, in the same transaction — this is
/// the durable, queryable record the task requires ("how much went to
/// Codex", "did Codex rows fail more"), not just a log line.
pub(crate) fn record_execution_driver_decision(
    conn: &Connection,
    execution_id: &str,
    work_item_id: &str,
    decision: &DriverDecision,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO execution_driver_decisions
            (execution_id, work_item_id, driver, reason, percentage_at_decision, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(execution_id) DO UPDATE SET
            work_item_id = excluded.work_item_id,
            driver = excluded.driver,
            reason = excluded.reason,
            percentage_at_decision = excluded.percentage_at_decision,
            created_at = excluded.created_at",
        params![
            execution_id,
            work_item_id,
            decision.driver,
            decision.reason,
            decision.percentage_at_decision,
            now,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
    use boss_protocol::WorkItemPatch;

    #[test]
    fn hash_bucket_is_stable_across_calls() {
        let id = "task_abc123";
        assert_eq!(hash_bucket(id), hash_bucket(id));
    }

    #[test]
    fn hash_bucket_is_in_range() {
        for id in ["task_a", "task_b", "task_c", "exec_zzz", ""] {
            assert!(hash_bucket(id) < 100, "bucket out of range for {id:?}");
        }
    }

    #[test]
    fn percentage_defaults_to_zero_and_persists_across_get() {
        let (_dir, db) = open_db();
        assert_eq!(db.get_codex_dispatch_percentage().unwrap(), 0);
        db.set_codex_dispatch_percentage(37).unwrap();
        assert_eq!(db.get_codex_dispatch_percentage().unwrap(), 37);
    }

    #[test]
    fn percentage_set_clamps_above_100() {
        let (_dir, db) = open_db();
        let applied = db.set_codex_dispatch_percentage(255).unwrap();
        assert_eq!(applied, 100);
        assert_eq!(db.get_codex_dispatch_percentage().unwrap(), 100);
    }

    #[test]
    fn is_percentage_eligible_whitelists_implementation_kinds_only() {
        assert!(is_percentage_eligible(ExecutionKind::TaskImplementation));
        assert!(is_percentage_eligible(ExecutionKind::ChoreImplementation));
        assert!(is_percentage_eligible(ExecutionKind::RevisionImplementation));
        assert!(!is_percentage_eligible(ExecutionKind::ProductDesign));
        assert!(!is_percentage_eligible(ExecutionKind::ProjectDesign));
        assert!(!is_percentage_eligible(ExecutionKind::InvestigationImplementation));
        assert!(!is_percentage_eligible(ExecutionKind::PrReview));
        assert!(!is_percentage_eligible(ExecutionKind::AutomationTriage));
        assert!(!is_percentage_eligible(ExecutionKind::CiRemediation));
        assert!(!is_percentage_eligible(ExecutionKind::ConflictResolution));
        assert!(!is_percentage_eligible(ExecutionKind::AnswerAgent));
    }

    /// End-to-end: percentage 0 must send literally zero eligible rows to
    /// Codex, no rounding or off-by-one leakage, across a wide spread of
    /// work item ids (not just a hand-picked one that happens to hash low).
    #[test]
    fn zero_percentage_never_assigns_codex_end_to_end() {
        let (_dir, db) = open_db();
        db.set_codex_dispatch_percentage(0).unwrap();
        let product = create_test_product(&db);
        for i in 0..200 {
            let chore = create_test_chore(&db, &product.id, format!("chore {i}"));
            let execution = create_ready_chore_execution(&db, &chore.id);
            let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
            assert_eq!(decision.driver, None, "percentage 0 must never assign codex");
            assert_eq!(decision.reason, REASON_PERCENTAGE);
            assert_eq!(decision.percentage_at_decision, Some(0));
        }
    }

    /// The mirror image: percentage 100 must send every eligible row to
    /// Codex.
    #[test]
    fn hundred_percentage_always_assigns_codex_end_to_end() {
        let (_dir, db) = open_db();
        db.set_codex_dispatch_percentage(100).unwrap();
        let product = create_test_product(&db);
        for i in 0..50 {
            let chore = create_test_chore(&db, &product.id, format!("chore {i}"));
            let execution = create_ready_chore_execution(&db, &chore.id);
            let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
            assert_eq!(decision.driver.as_deref(), Some(CODEX_DRIVER_SLUG));
            assert_eq!(decision.reason, REASON_PERCENTAGE);
        }
    }

    /// An explicit `--driver` on the row always wins, regardless of the
    /// percentage.
    #[test]
    fn explicit_driver_overrides_percentage() {
        let (_dir, db) = open_db();
        db.set_codex_dispatch_percentage(100).unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "explicit-driver chore");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                driver: Some("claude".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = create_ready_chore_execution(&db, &chore.id);
        let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        assert_eq!(decision.driver.as_deref(), Some("claude"));
        assert_eq!(decision.reason, REASON_EXPLICIT);
    }

    /// A row whose reasoning is not `standard` (e.g. `investigation`) is
    /// never eligible, even at 100%.
    #[test]
    fn non_standard_reasoning_is_ineligible() {
        let (_dir, db) = open_db();
        db.set_codex_dispatch_percentage(100).unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "investigation-shaped chore");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                reasoning: Some("investigation".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = create_ready_chore_execution(&db, &chore.id);
        let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        assert_eq!(decision.driver, None);
        assert_eq!(decision.reason, REASON_DEFAULT);
    }

    /// Same work item id → same decision, deterministically, across
    /// repeated (re-)dispatch of independent execution rows — the whole
    /// point of hashing the id instead of rolling per attempt.
    #[test]
    fn same_work_item_id_is_stable_across_repeated_dispatch() {
        let (_dir, db) = open_db();
        db.set_codex_dispatch_percentage(50).unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "retried chore");
        let first = create_ready_chore_execution(&db, &chore.id);
        let second = create_ready_chore_execution(&db, &chore.id);
        let d1 = db.get_execution_driver_decision(&first.id).unwrap().unwrap();
        let d2 = db.get_execution_driver_decision(&second.id).unwrap().unwrap();
        assert_eq!(
            d1.driver, d2.driver,
            "same work item id must resolve to the same driver every time"
        );
    }
}
