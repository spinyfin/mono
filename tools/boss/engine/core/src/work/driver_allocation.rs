//! Three-way driver traffic allocation (experiment, operator-controlled).
//!
//! One persisted [`DriverTrafficSplit`] — three shares summing to exactly
//! 100 — decides how ordinary, `standard`-reasoning implementation work
//! (`task_implementation`, `chore_implementation`,
//! `revision_implementation`) is divided between the `grok`, `claude`, and
//! `codex` drivers. This generalises the earlier single Codex percentage;
//! see [`boss_protocol::DriverTrafficSplit`] for why the split is modelled
//! as three explicit integers with a hard sum constraint rather than an
//! implied remainder or normalised weights.
//!
//! Assignment is a deterministic hash of the work item's own id placed on
//! the split's `[0, 100)` bucket line — not a coin flip per dispatch
//! attempt — so the same row under the same split always lands on the same
//! driver across retries, redispatch, and recovery. Changing the split moves
//! the boundaries and therefore reallocates some rows that have not been
//! dispatched yet; that is deliberate, and it cannot make a row flip
//! *between attempts of one dispatch* because the decision is computed once,
//! at [`crate::work::revision_helpers::insert_execution`] time, and
//! persisted per-execution in `execution_driver_decisions` (see
//! [`record_execution_driver_decision`]).
//!
//! An explicit driver pin always wins over allocation: the work item's own
//! `driver` column first, then its product's `default_driver`. Allocation
//! only decides rows that expressed no preference at either level — which is
//! also what makes the shipped default behaviour-preserving (see
//! [`load_driver_traffic_split_conn`]).
//!
//! [`WorkDb::get_execution_driver_decision`] is the read side both real
//! dispatch paths ([`crate::work::driver_lookup`]'s events-socket resolver
//! and [`crate::runner::worker_spawn`]'s spawn-config resolver) consult, so
//! an allocated row is actually routed to its driver rather than the
//! recorded decision being an inert audit trail.

use boss_protocol::DriverTrafficSplit;
use sha2::{Digest, Sha256};

use super::*;

/// Metadata KV key for the persisted split, stored as the JSON encoding of
/// [`DriverTrafficSplit`]. Same shape as `dispatch_concurrency_limit` (see
/// `app/handler_helpers.rs`): read fresh at each decision point rather than
/// cached, so a change takes effect on the very next execution created —
/// without disturbing any execution already dispatched (a decision already
/// recorded on an existing row is never recomputed).
///
/// Deliberately ONE row holding the whole triple rather than three rows: an
/// operator edit is a single atomic write, so a concurrent dispatch can
/// never read a half-applied edit that transits through an invalid split.
const METADATA_KEY_DRIVER_TRAFFIC_SPLIT: &str = "driver_traffic_split";

/// The superseded single-Codex-percentage key. Read only by
/// `migrations_b::migrate_driver_traffic_split_from_codex_percentage`, which
/// folds any persisted value into an equivalent split once and then removes
/// it, so this module has exactly one source of truth.
pub(crate) const METADATA_KEY_CODEX_DISPATCH_PERCENTAGE: &str = "codex_dispatch_percentage";

/// `execution_driver_decisions.reason`: the row (or its product) pinned a
/// driver; the split was not consulted.
pub(crate) const REASON_EXPLICIT: &str = "explicit";
/// `execution_driver_decisions.reason`: an eligible row placed on the
/// split's bucket line. Recorded whichever driver it landed on, so
/// "allocated to claude" stays distinguishable from "never eligible".
pub(crate) const REASON_ALLOCATION: &str = "allocation";
/// `execution_driver_decisions.reason`: ineligible; no override, the row
/// resolves through the ordinary precedence chain.
pub(crate) const REASON_DEFAULT: &str = "default";

/// The reason value written by the superseded single-Codex-percentage
/// scheme. Never written any more; still recognised on read so decisions
/// recorded before this landed keep their meaning instead of silently
/// degrading to `default`.
pub(crate) const REASON_LEGACY_PERCENTAGE: &str = "percentage";

/// The routing decision for one execution. `driver` is `None` only for
/// [`REASON_DEFAULT`] — an ineligible row, which falls through to the normal
/// row → product → engine default resolution. `split_at_decision` is set for
/// [`REASON_ALLOCATION`] so a later analysis can tell which split a given
/// row was placed against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverDecision {
    pub driver: Option<String>,
    pub reason: &'static str,
    pub split_at_decision: Option<DriverTrafficSplit>,
}

impl DriverDecision {
    fn explicit(driver: String) -> Self {
        Self {
            driver: Some(driver),
            reason: REASON_EXPLICIT,
            split_at_decision: None,
        }
    }

    fn default_no_override() -> Self {
        Self {
            driver: None,
            reason: REASON_DEFAULT,
            split_at_decision: None,
        }
    }

    fn allocated(driver: &'static str, split: DriverTrafficSplit) -> Self {
        Self {
            driver: Some(driver.to_owned()),
            reason: REASON_ALLOCATION,
            split_at_decision: Some(split),
        }
    }
}

/// Whether `kind` falls in the narrow eligible slice: ordinary coding work
/// only. A whitelist, not a blocklist, so a future `ExecutionKind` variant
/// defaults to ineligible until someone deliberately opts it in — keeping
/// the experiment's slice from silently widening (design doc: "keep the
/// slice narrow"). Design rows, investigation rows, and PR reviews stay out.
fn is_allocation_eligible(kind: ExecutionKind) -> bool {
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

/// Read the persisted driver traffic split using an already-open
/// `Connection`. Callers holding a `WorkDb::connect()` guard (e.g.
/// `insert_execution`, mid-transaction) MUST use this instead of
/// `WorkDb::get_metadata`, which opens its own guard on the same
/// non-reentrant mutex and would deadlock.
///
/// An absent value yields [`DriverTrafficSplit::default`] — `claude = 100`,
/// `grok = 0`, `codex = 0`. That is the behaviour-preserving state: `claude`
/// is `boss_engine_effort::ENGINE_DEFAULT_DRIVER`, and rows that pinned a
/// driver at row or product level never reach allocation at all, so an
/// engine that has never had a split configured routes exactly where it
/// always did. `grok` starting at 0 also means shipping this sends nothing
/// to the `grok` driver until an operator deliberately raises it.
///
/// A value that is present but does not parse, or parses to a split that
/// fails [`DriverTrafficSplit::validate`], can only come from a hand-edited
/// `state.db` — [`WorkDb::set_driver_traffic_split`] refuses to persist one.
/// It is logged at ERROR and the default is used. Note what this is NOT: the
/// corrupt value is discarded whole, never clamped or normalised into a
/// nearby valid-looking split. Failing the read outright would wedge every
/// subsequent execution insert, which is a far worse outcome than routing to
/// the default driver with a loud log line.
pub(crate) fn load_driver_traffic_split_conn(conn: &Connection) -> Result<DriverTrafficSplit> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![METADATA_KEY_DRIVER_TRAFFIC_SPLIT],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(DriverTrafficSplit::default());
    };
    match serde_json::from_str::<DriverTrafficSplit>(&raw) {
        Ok(split) => match split.validate() {
            Ok(()) => Ok(split),
            Err(err) => {
                tracing::error!(
                    raw = %raw,
                    %err,
                    "driver_traffic_split: persisted split is invalid — discarding it, allocating on the default split",
                );
                Ok(DriverTrafficSplit::default())
            }
        },
        Err(err) => {
            tracing::error!(
                raw = %raw,
                %err,
                "driver_traffic_split: persisted split is unparseable — discarding it, allocating on the default split",
            );
            Ok(DriverTrafficSplit::default())
        }
    }
}

impl WorkDb {
    /// [`load_driver_traffic_split_conn`], but opens its own connection.
    /// For callers (RPC handlers, CLI) that are not already inside a
    /// `connect()` guard.
    pub fn get_driver_traffic_split(&self) -> Result<DriverTrafficSplit> {
        let conn = self.connect()?;
        load_driver_traffic_split_conn(&conn)
    }

    /// Persist `split`, or fail without writing anything when it does not
    /// sum to exactly 100. Nothing is clamped, redistributed, or normalised
    /// — an invalid split is the caller's error to surface, and
    /// all-three-zero is rejected by the same rule as every other bad total.
    ///
    /// Takes effect on the next execution created — never touches an
    /// execution already dispatched, exactly like the interactive
    /// concurrency cap. Written as one metadata row, so no concurrent
    /// dispatch can observe a partially-applied edit.
    pub fn set_driver_traffic_split(&self, split: DriverTrafficSplit) -> Result<DriverTrafficSplit> {
        split.validate()?;
        self.set_metadata(METADATA_KEY_DRIVER_TRAFFIC_SPLIT, &serde_json::to_string(&split)?)?;
        Ok(split)
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
/// `Connection` (see `load_driver_traffic_split_conn`'s doc for why this
/// split exists).
pub(crate) fn get_execution_driver_decision_conn(
    conn: &Connection,
    execution_id: &str,
) -> Result<Option<DriverDecision>> {
    conn.query_row(
        "SELECT driver, reason, split_at_decision
           FROM execution_driver_decisions
          WHERE execution_id = ?1",
        params![execution_id],
        |row| {
            let reason_raw: String = row.get(1)?;
            let reason = match reason_raw.as_str() {
                REASON_EXPLICIT => REASON_EXPLICIT,
                REASON_ALLOCATION => REASON_ALLOCATION,
                REASON_LEGACY_PERCENTAGE => REASON_LEGACY_PERCENTAGE,
                _ => REASON_DEFAULT,
            };
            let split_raw: Option<String> = row.get(2)?;
            Ok(DriverDecision {
                driver: row.get(0)?,
                reason,
                split_at_decision: split_raw.and_then(|raw| serde_json::from_str(&raw).ok()),
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Decide which driver governs a newly-created execution row and why.
/// Called once, from `insert_execution`, using the same open `Connection`
/// (never `WorkDb::connect()` — see `load_driver_traffic_split_conn`).
///
/// Precedence:
/// 1. The work item's own explicit `driver` column wins.
/// 2. Otherwise the product's `default_driver` wins. Both of these are an
///    operator saying "this work goes there"; allocation only decides rows
///    that expressed no preference. Respecting the product pin is also what
///    makes the shipped default split byte-for-byte behaviour-preserving —
///    it is not a new per-product override, it is the existing one.
/// 3. Otherwise, if `kind` is in the eligible slice
///    ([`is_allocation_eligible`]) and the work item's `reasoning` is
///    literally `standard` (NULL/legacy rows are NOT treated as eligible —
///    keeps the slice narrow), place the work item's own id on the current
///    split's bucket line.
/// 4. Otherwise, no override — the row falls through to the normal
///    row → product → engine default resolution.
pub(crate) fn decide_execution_driver(
    conn: &Connection,
    work_item_id: &str,
    kind: ExecutionKind,
) -> Result<DriverDecision> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT t.driver, t.reasoning, p.default_driver
               FROM tasks t
               LEFT JOIN products p ON p.id = t.product_id
              WHERE t.id = ?1",
            params![work_item_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((explicit_driver, reasoning, product_default_driver)) = row else {
        // Not a `tasks` row at all (e.g. an `answer_agent` execution bound
        // to a comment id) — nothing to route.
        return Ok(DriverDecision::default_no_override());
    };
    let pinned = explicit_driver
        .filter(|s| !s.trim().is_empty())
        .or_else(|| product_default_driver.filter(|s| !s.trim().is_empty()));
    if let Some(driver) = pinned {
        return Ok(DriverDecision::explicit(driver));
    }
    if !is_allocation_eligible(kind) || reasoning.as_deref() != Some("standard") {
        return Ok(DriverDecision::default_no_override());
    }
    let split = load_driver_traffic_split_conn(conn)?;
    // `hash_bucket` is in `[0, 100)` and `driver_for_bucket` partitions that
    // space into three half-open intervals sized exactly by the shares — so
    // a driver at 0 owns an empty interval and is literally unreachable, no
    // rounding and no off-by-one leakage.
    Ok(DriverDecision::allocated(
        split.driver_for_bucket(hash_bucket(work_item_id)),
        split,
    ))
}

/// Persist `decision` for `execution_id`. Called once, right after the
/// `work_executions` row is inserted, in the same transaction — this is
/// the durable, queryable record the task requires ("how much went to each
/// driver", "did one driver's rows fail more"), not just a log line.
pub(crate) fn record_execution_driver_decision(
    conn: &Connection,
    execution_id: &str,
    work_item_id: &str,
    decision: &DriverDecision,
    now: &str,
) -> Result<()> {
    let split_json = decision
        .split_at_decision
        .map(|split| serde_json::to_string(&split))
        .transpose()?;
    conn.execute(
        "INSERT INTO execution_driver_decisions
            (execution_id, work_item_id, driver, reason, split_at_decision, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(execution_id) DO UPDATE SET
            work_item_id = excluded.work_item_id,
            driver = excluded.driver,
            reason = excluded.reason,
            split_at_decision = excluded.split_at_decision,
            created_at = excluded.created_at",
        params![
            execution_id,
            work_item_id,
            decision.driver,
            decision.reason,
            split_json,
            now,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
