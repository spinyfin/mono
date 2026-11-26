//! Durable per-session, per-hour-bucket coordinator token usage storage.
//!
//! Unlike worker runs, the Boss coordinator has no `tasks` or `work_executions`
//! row and stays alive for hours or days across compactions and subagent
//! delegations. Storing one row per `(session_id, bucket_start_epoch_s,
//! model_key)` gives:
//!
//! * exact alignment with `GetTokenUsageSeries` (`hour` is the finest bucket
//!   width, so every hour/day/week/month bucket maps cleanly without splitting),
//! * accurate per-model attribution when the coordinator delegates subagents
//!   to another model (e.g. Haiku/Sonnet under an Opus coordinator),
//! * idempotent upserts across repeated transcript reads and engine restarts
//!   (assignments, never increments — no double counting), and
//! * survival across coordinator restarts, handoffs, and transcript deletion.
//!
//! **Window granularity.** A slice is one whole UTC hour, so a reporting window
//! `[since, until)` contains the slices whose hour *starts* inside it
//! (`bucket_start_epoch_s`). That key never changes once written, so later chunks cannot move a
//! message into a different reporting window. Totals can still grow as delayed
//! chunks arrive. An
//! hour-aligned window is exact and a window edge that falls mid-hour is
//! resolved to the hour boundary rather than splitting the hour.
//!
//! Identified message and duration facts are retained and summed, so a missing
//! subagent does not hide newly appended parent usage. Coverage gaps are durable.
//! Legacy aggregate-only slices remain an explicitly incomplete floor because
//! their missing message identities cannot be reconstructed safely.
//!
//! **Never lose known usage.** A snapshot built from an incomplete read
//! carries a `capture_gap`; it is merged into the stored slices (per-column
//! maximum for legacy aggregate-only sources) and marks them with the gap
//! reason instead of replacing them. A full replacement that would lower a
//! session's stored totals is downgraded to that merge.

use anyhow::Result;
use boss_engine_storage_values::CostRunRecord;
use boss_protocol::{COORDINATOR_USAGE_KIND, coordinator_session_label};
use boss_usage_report::{RecordedDriver, UsageRunRecord};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use std::collections::BTreeMap;

use super::WorkDb;

/// Unique migration id for [`migrate`].
pub(super) const MIGRATION_ID: &str = "20261127_2035_coordinator_usage_slices";

pub use boss_protocol::{COORDINATOR_EXECUTION_ID_PREFIX, COORDINATOR_UNREADABLE_SPAWN_PREFIX};

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS coordinator_usage_slices (
             id TEXT NOT NULL UNIQUE,
             session_id TEXT NOT NULL,
             spawn_token TEXT,
             source TEXT NOT NULL,
             driver TEXT,
             launched_model TEXT,
             host_id TEXT NOT NULL DEFAULT 'local',
             status TEXT NOT NULL DEFAULT 'active',
             bucket_start_epoch_s INTEGER NOT NULL,
             first_event_at INTEGER NOT NULL,
             last_event_at INTEGER NOT NULL,
             model_key TEXT NOT NULL,
             model TEXT,
             input_tokens INTEGER,
             output_tokens INTEGER,
             cache_creation_tokens INTEGER,
             cache_creation_5m_tokens INTEGER,
             cache_creation_1h_tokens INTEGER,
             cache_read_tokens INTEGER,
             rounds INTEGER,
             agent_active_ms INTEGER,
             usage_unavailable TEXT,
             updated_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, bucket_start_epoch_s, model_key)
         );
         CREATE INDEX IF NOT EXISTS coordinator_usage_slices_bucket_idx
             ON coordinator_usage_slices(bucket_start_epoch_s);
         CREATE INDEX IF NOT EXISTS coordinator_usage_slices_spawn_token_idx
             ON coordinator_usage_slices(spawn_token) WHERE spawn_token IS NOT NULL;",
    )?;
    Ok(())
}

pub(super) fn migrate_facts(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS coordinator_usage_facts (
        session_id TEXT NOT NULL, fact_id TEXT NOT NULL, payload TEXT NOT NULL,
        PRIMARY KEY(session_id, fact_id));",
    )?;
    Ok(())
}

pub(super) fn migrate_capture_state(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS coordinator_usage_capture_state (
        session_id TEXT PRIMARY KEY, gap TEXT, legacy_slices TEXT);",
    )?;
    Ok(())
}

/// One durable `(session_id, bucket_start_epoch_s, model)` usage slice for the
/// Boss coordinator. Token columns are `None` when unknown/unavailable — never
/// coerced to zero.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CoordinatorUsageSlice {
    /// Message closed by a duration contribution (not a slice dimension).
    #[serde(default)]
    pub assistant_message_id: Option<String>,
    pub session_id: String,
    pub spawn_token: Option<String>,
    pub source: String,
    pub driver: Option<String>,
    pub launched_model: Option<String>,
    #[builder(default = "local".to_owned())]
    pub host_id: String,
    #[builder(default = "active".to_owned())]
    pub status: String,
    pub bucket_start_epoch_s: i64,
    pub first_event_at: i64,
    pub last_event_at: i64,
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_creation_5m_tokens: Option<i64>,
    pub cache_creation_1h_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub rounds: Option<i64>,
    pub agent_active_ms: Option<i64>,
    pub usage_unavailable: Option<String>,
}

/// Complete or incremental usage update for one coordinator session produced by
/// a usage source (e.g. Claude transcripts today, ACP `usage_update` later).
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct CoordinatorSessionSnapshot {
    pub session_id: String,
    pub spawn_token: Option<String>,
    #[builder(default)]
    pub slices: Vec<CoordinatorUsageSlice>,
    /// When `true`, replaces all stored slices for `session_id` atomically so
    /// transcript truncation or rebuilt buckets never leave stale slices behind.
    /// When `false`, upserts only the supplied slices (suitable for incremental
    /// push sources such as ACP `usage_update`).
    #[builder(default = true)]
    pub full_replacement: bool,
    /// Set when the read that produced this snapshot was incomplete (a
    /// malformed line was skipped, a transcript or subagent file could not be
    /// read). The snapshot is then merged into the stored slices instead of
    /// replacing them, and the reason is recorded on the session's slices as
    /// `usage_unavailable` so the coverage gap is visible while the known
    /// totals are kept.
    pub capture_gap: Option<String>,
    /// Individually identified contributions; absent facts are retained across rereads.
    pub facts: Option<BTreeMap<String, CoordinatorUsageSlice>>,
}

impl CoordinatorUsageSlice {
    fn has_measured_tokens(&self) -> bool {
        [
            self.input_tokens,
            self.output_tokens,
            self.cache_creation_tokens,
            self.cache_read_tokens,
        ]
        .iter()
        .any(Option::is_some)
    }
}

/// Deterministic row id for `(session_id, bucket_start_epoch_s, model_key)`.
/// Uses FNV-1a 64-bit so re-reads and restarts always assign the exact same
/// `run_id` without introducing extra hashing dependencies.
pub fn coordinator_slice_id(session_id: &str, bucket_start_epoch_s: i64, model_key: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for part in [
        session_id.as_bytes(),
        &bucket_start_epoch_s.to_le_bytes(),
        model_key.as_bytes(),
    ] {
        for &byte in part {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("coord_run_{hash:016x}")
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Persist one coordinator session's usage snapshot idempotently.
    ///
    /// Values are assignments, never SQL increments, so re-reading a transcript
    /// or restarting the engine produces identical totals with zero double
    /// counting. When a real session persists with a `spawn_token`, any prior
    /// unmeasured `spawn:<token>` placeholder for that launch is retired in the
    /// same transaction.
    pub fn upsert_coordinator_session_usage(&self, snapshot: &CoordinatorSessionSnapshot) -> Result<usize> {
        let mut conn = self.connect()?;
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let mut resolved = snapshot.clone();
        if let Some(facts) = &snapshot.facts {
            resolve_facts(&tx, &mut resolved, facts)?;
        } else if snapshot.full_replacement && snapshot.capture_gap.is_none() {
            let previous = load_slices(&tx, &snapshot.session_id)?;
            if previous.iter().any(|old| {
                snapshot
                    .slices
                    .iter()
                    .find(|new| new.bucket_start_epoch_s == old.bucket_start_epoch_s && new.model == old.model)
                    .is_none_or(|new| decreases(old, new))
            }) {
                resolved.capture_gap =
                    Some("Previously captured usage is missing from this read; retained known usage".into());
            }
        }
        let snapshot = &resolved;
        let gap_reason = snapshot.capture_gap.clone();
        let merge = gap_reason.is_some() && snapshot.facts.is_none();
        let has_existing_slices: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM coordinator_usage_slices WHERE session_id = ?1)",
            params![snapshot.session_id],
            |row| row.get(0),
        )?;

        if snapshot.full_replacement && !merge {
            tx.execute(
                "DELETE FROM coordinator_usage_slices WHERE session_id = ?1",
                params![snapshot.session_id],
            )?;
        }

        if let Some(spawn_token) = snapshot.spawn_token.as_deref().filter(|t| !t.is_empty())
            && !snapshot.session_id.starts_with(COORDINATOR_UNREADABLE_SPAWN_PREFIX)
        {
            let placeholder = format!("{COORDINATOR_UNREADABLE_SPAWN_PREFIX}{spawn_token}");
            tx.execute(
                "DELETE FROM coordinator_usage_slices WHERE session_id = ?1",
                params![placeholder],
            )?;
        }

        let mut stmt = tx.prepare(&upsert_sql(merge))?;

        let mut written = 0usize;
        for slice in &snapshot.slices {
            // An unmeasured slice only describes a read that produced nothing.
            // It must not add a phantom row next to slices that already hold
            // measured usage.
            if merge && has_existing_slices && !slice.has_measured_tokens() {
                continue;
            }
            let model_key = slice.model.as_deref().unwrap_or("");
            let id = coordinator_slice_id(&slice.session_id, slice.bucket_start_epoch_s, model_key);
            stmt.execute(params![
                id,
                slice.session_id,
                slice.spawn_token,
                slice.source,
                slice.driver,
                slice.launched_model,
                slice.host_id,
                slice.status,
                slice.bucket_start_epoch_s,
                slice.first_event_at,
                slice.last_event_at,
                model_key,
                slice.model,
                slice.input_tokens,
                slice.output_tokens,
                slice.cache_creation_tokens,
                slice.cache_creation_5m_tokens,
                slice.cache_creation_1h_tokens,
                slice.cache_read_tokens,
                slice.rounds,
                slice.agent_active_ms,
                slice.usage_unavailable,
                now,
            ])?;
            written += 1;
        }
        drop(stmt);
        if let Some(reason) = gap_reason.as_deref() {
            // Record the coverage gap on everything stored for the session
            // while leaving its known counts untouched.
            tx.execute(
                "UPDATE coordinator_usage_slices SET usage_unavailable = ?2 WHERE session_id = ?1",
                params![snapshot.session_id, reason],
            )?;
        }
        tx.commit()?;
        Ok(written)
    }

    /// Mark any previously `active` coordinator slices whose `session_id` is no
    /// longer in `active_session_ids` as `completed`. Preserves all historical
    /// token counts across restarts and handoffs.
    pub fn mark_inactive_coordinator_sessions_completed(&self, active_session_ids: &[&str]) -> Result<usize> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = if active_session_ids.is_empty() {
            tx.execute(
                "UPDATE coordinator_usage_slices SET status = 'completed' WHERE status = 'active'",
                [],
            )?
        } else {
            tx.execute(
                "UPDATE coordinator_usage_slices
                 SET status = 'completed'
                 WHERE status = 'active'
                   AND session_id NOT IN (SELECT value FROM json_each(?1))",
                [super::query_ensure::id_array(active_session_ids)],
            )?
        };
        tx.commit()?;
        Ok(updated)
    }

    /// Delete an unmeasured `spawn:<token>` placeholder once a real session is
    /// known to cover that coordinator launch.
    pub fn clear_coordinator_spawn_placeholder(&self, spawn_token: &str) -> Result<usize> {
        let conn = self.connect()?;
        let placeholder = format!("{COORDINATOR_UNREADABLE_SPAWN_PREFIX}{spawn_token}");
        let deleted = conn.execute(
            "DELETE FROM coordinator_usage_slices WHERE session_id = ?1",
            params![placeholder],
        )?;
        Ok(deleted)
    }
}

/// Token-bearing columns of a slice. A merge keeps the larger of the stored
/// and incoming value (a session's usage only grows) and treats NULL as
/// "unknown", so an unknown incoming value never erases a known stored one.
const MERGED_COLUMNS: [&str; 8] = [
    "input_tokens",
    "output_tokens",
    "cache_creation_tokens",
    "cache_creation_5m_tokens",
    "cache_creation_1h_tokens",
    "cache_read_tokens",
    "rounds",
    "agent_active_ms",
];

fn upsert_sql(merge: bool) -> String {
    let column_updates: String = MERGED_COLUMNS
        .iter()
        .map(|column| {
            if merge {
                format!(
                    "{column} = CASE WHEN excluded.{column} IS NULL THEN coordinator_usage_slices.{column} \
                     WHEN coordinator_usage_slices.{column} IS NULL THEN excluded.{column} \
                     ELSE MAX(excluded.{column}, coordinator_usage_slices.{column}) END"
                )
            } else {
                format!("{column} = excluded.{column}")
            }
        })
        .collect::<Vec<_>>()
        .join(",\n                 ");
    let event_updates = if merge {
        "first_event_at = MIN(excluded.first_event_at, coordinator_usage_slices.first_event_at),
                 last_event_at = MAX(excluded.last_event_at, coordinator_usage_slices.last_event_at)"
    } else {
        "first_event_at = excluded.first_event_at,
                 last_event_at = excluded.last_event_at"
    };
    format!(
        "INSERT INTO coordinator_usage_slices (
             id, session_id, spawn_token, source, driver, launched_model,
             host_id, status, bucket_start_epoch_s, first_event_at, last_event_at,
             model_key, model, input_tokens, output_tokens,
             cache_creation_tokens, cache_creation_5m_tokens, cache_creation_1h_tokens,
             cache_read_tokens, rounds, agent_active_ms, usage_unavailable, updated_at
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6,
             ?7, ?8, ?9, ?10, ?11,
             ?12, ?13, ?14, ?15,
             ?16, ?17, ?18,
             ?19, ?20, ?21, ?22, ?23
         )
         ON CONFLICT(session_id, bucket_start_epoch_s, model_key) DO UPDATE SET
             id = excluded.id,
             spawn_token = COALESCE(excluded.spawn_token, coordinator_usage_slices.spawn_token),
             source = excluded.source,
             driver = excluded.driver,
             launched_model = COALESCE(excluded.launched_model, coordinator_usage_slices.launched_model),
             host_id = excluded.host_id,
             status = excluded.status,
             {event_updates},
             model = excluded.model,
             {column_updates},
             usage_unavailable = excluded.usage_unavailable,
             updated_at = excluded.updated_at"
    )
}

// Legacy slices lack message identities. Keep them as an explicit coverage floor,
// never add them to identified facts (which would count the same usage twice).
fn load_slices(conn: &Connection, session: &str) -> Result<Vec<CoordinatorUsageSlice>> {
    let columns = [
        "session_id",
        "spawn_token",
        "source",
        "driver",
        "launched_model",
        "host_id",
        "status",
        "bucket_start_epoch_s",
        "first_event_at",
        "last_event_at",
        "model",
        "input_tokens",
        "output_tokens",
        "cache_creation_tokens",
        "cache_creation_5m_tokens",
        "cache_creation_1h_tokens",
        "cache_read_tokens",
        "rounds",
        "agent_active_ms",
        "usage_unavailable",
    ];
    let pairs = columns
        .iter()
        .map(|c| format!("'{c}', {c}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT json_object({pairs}) FROM coordinator_usage_slices WHERE session_id = ?1"
    ))?;
    let rows = stmt.query_map([session], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn counts(slice: &CoordinatorUsageSlice) -> [Option<i64>; 8] {
    [
        slice.input_tokens,
        slice.output_tokens,
        slice.cache_creation_tokens,
        slice.cache_creation_5m_tokens,
        slice.cache_creation_1h_tokens,
        slice.cache_read_tokens,
        slice.rounds,
        slice.agent_active_ms,
    ]
}

fn decreases(old: &CoordinatorUsageSlice, new: &CoordinatorUsageSlice) -> bool {
    counts(old)
        .into_iter()
        .zip(counts(new))
        .any(|(a, b)| a.is_some_and(|a| b.is_none_or(|b| b < a)))
}

fn retain_counts(new: &mut CoordinatorUsageSlice, old: &CoordinatorUsageSlice) {
    let kept = counts(new)
        .into_iter()
        .zip(counts(old))
        .map(|(a, b)| a.max(b))
        .collect::<Vec<_>>();
    new.input_tokens = kept[0];
    new.output_tokens = kept[1];
    new.cache_creation_tokens = kept[2];
    new.cache_creation_5m_tokens = kept[3];
    new.cache_creation_1h_tokens = kept[4];
    new.cache_read_tokens = kept[5];
    new.rounds = kept[6];
    new.agent_active_ms = kept[7];
}

fn resolve_facts(
    conn: &Connection,
    snapshot: &mut CoordinatorSessionSnapshot,
    facts: &BTreeMap<String, CoordinatorUsageSlice>,
) -> Result<()> {
    let state: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT gap, legacy_slices FROM coordinator_usage_capture_state WHERE session_id = ?1",
            [&snapshot.session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (old_gap, legacy) = match state {
        Some((gap, legacy)) => (
            gap,
            legacy
                .map(|s| serde_json::from_str::<Vec<CoordinatorUsageSlice>>(&s))
                .transpose()?
                .unwrap_or_default(),
        ),
        None => (None, load_slices(conn, &snapshot.session_id)?),
    };
    let mut stored = BTreeMap::<String, CoordinatorUsageSlice>::new();
    let mut stmt = conn.prepare("SELECT fact_id, payload FROM coordinator_usage_facts WHERE session_id = ?1")?;
    for row in stmt.query_map([&snapshot.session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (key, payload) = row?;
        stored.insert(key, serde_json::from_str(&payload)?);
    }
    snapshot.capture_gap = snapshot.capture_gap.clone().or(old_gap);
    if snapshot.capture_gap.is_none() && stored.keys().any(|key| !facts.contains_key(key)) {
        snapshot.capture_gap =
            Some("Previously captured transcript contributions are missing; retained known usage".into());
    }
    if legacy.iter().any(CoordinatorUsageSlice::has_measured_tokens) {
        snapshot.capture_gap = Some("Legacy aggregate usage lacks message identities; totals retain a known floor and may omit missing-source usage".into());
    }
    for (key, incoming) in facts {
        let mut fact = incoming.clone();
        if let Some(previous) = stored.get(key) {
            fact.bucket_start_epoch_s = previous.bucket_start_epoch_s;
            fact.first_event_at = previous.first_event_at;
            fact.model = fact.model.or_else(|| previous.model.clone());
            retain_counts(&mut fact, previous);
        }
        conn.execute("INSERT INTO coordinator_usage_facts VALUES (?1, ?2, ?3) ON CONFLICT(session_id, fact_id) DO UPDATE SET payload = excluded.payload",
            params![snapshot.session_id, key, serde_json::to_string(&fact)?])?;
        stored.insert(key.clone(), fact);
    }
    let mut buckets = BTreeMap::<(i64, Option<String>), CoordinatorUsageSlice>::new();
    for fact in stored.values() {
        let mut fact = fact.clone();
        if let Some(message) = fact
            .assistant_message_id
            .as_ref()
            .and_then(|id| stored.get(&format!("message:{id}")))
        {
            fact.bucket_start_epoch_s = message.bucket_start_epoch_s;
            fact.first_event_at = message.first_event_at;
            fact.last_event_at = message.first_event_at;
            fact.model = message.model.clone();
        }
        let key = (fact.bucket_start_epoch_s, fact.model.clone());
        if let Some(total) = buckets.get_mut(&key) {
            sum_fact(total, &fact);
        } else {
            buckets.insert(key, fact);
        }
    }
    for old in &legacy {
        if !old.has_measured_tokens() {
            continue;
        }
        let key = (old.bucket_start_epoch_s, old.model.clone());
        if let Some(total) = buckets.get_mut(&key) {
            retain_counts(total, old);
        } else {
            buckets.insert(key, old.clone());
        }
    }
    if !buckets.is_empty() {
        snapshot.slices = buckets
            .into_values()
            .map(|mut slice| {
                slice.status = snapshot
                    .slices
                    .first()
                    .map_or_else(|| slice.status.clone(), |s| s.status.clone());
                slice.usage_unavailable = snapshot.capture_gap.clone().or(slice.usage_unavailable);
                slice
            })
            .collect();
    }
    conn.execute("INSERT INTO coordinator_usage_capture_state VALUES (?1, ?2, ?3) ON CONFLICT(session_id) DO UPDATE SET gap = excluded.gap, legacy_slices = excluded.legacy_slices",
        params![snapshot.session_id, snapshot.capture_gap, serde_json::to_string(&legacy)?])?;
    Ok(())
}

fn sum_fact(total: &mut CoordinatorUsageSlice, fact: &CoordinatorUsageSlice) {
    let ttl_unknown = (total.has_measured_tokens() && total.cache_creation_5m_tokens.is_none())
        || (fact.has_measured_tokens() && fact.cache_creation_5m_tokens.is_none());
    total.usage_unavailable = total
        .usage_unavailable
        .clone()
        .or_else(|| fact.usage_unavailable.clone());
    total.first_event_at = total.first_event_at.min(fact.first_event_at);
    total.last_event_at = total.last_event_at.max(fact.last_event_at);
    total.input_tokens = match (total.input_tokens, fact.input_tokens) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    };
    total.output_tokens = match (total.output_tokens, fact.output_tokens) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    };
    total.cache_creation_tokens = match (total.cache_creation_tokens, fact.cache_creation_tokens) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    };
    total.cache_creation_5m_tokens = match (total.cache_creation_5m_tokens, fact.cache_creation_5m_tokens) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    };
    total.cache_creation_1h_tokens = match (total.cache_creation_1h_tokens, fact.cache_creation_1h_tokens) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    };
    total.cache_read_tokens = match (total.cache_read_tokens, fact.cache_read_tokens) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    };
    total.rounds = match (total.rounds, fact.rounds) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    };
    total.agent_active_ms = match (total.agent_active_ms, fact.agent_active_ms) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    };
    if ttl_unknown {
        total.cache_creation_5m_tokens = None;
        total.cache_creation_1h_tokens = None;
    }
}

pub(super) fn query_coordinator_cost_records(
    conn: &Connection,
    since_epoch_s: i64,
    until_epoch_s: i64,
) -> Result<Vec<CostRunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, status, bucket_start_epoch_s, first_event_at,
                model, driver, input_tokens, output_tokens,
                cache_creation_tokens, cache_read_tokens,
                agent_active_ms, usage_unavailable,
                cache_creation_5m_tokens, cache_creation_1h_tokens
         FROM coordinator_usage_slices
         WHERE bucket_start_epoch_s >= ?1 AND bucket_start_epoch_s < ?2
         ORDER BY bucket_start_epoch_s ASC, id ASC",
    )?;
    let rows = stmt.query_map(params![since_epoch_s, until_epoch_s], map_cost_record)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn map_cost_record(row: &Row) -> rusqlite::Result<CostRunRecord> {
    let session_id: String = row.get(1)?;
    let status: String = row.get(2)?;
    Ok(CostRunRecord {
        run_id: row.get(0)?,
        execution_id: format!("{COORDINATOR_EXECUTION_ID_PREFIX}{session_id}"),
        work_item_id: COORDINATOR_USAGE_KIND.to_owned(),
        work_item_name: Some(coordinator_session_label(&session_id)),
        execution_kind: COORDINATOR_USAGE_KIND.to_owned(),
        execution_status: status.clone(),
        run_status: status,
        created_at_epoch_s: row.get(3)?,
        work_item_short_id: None,
        model: row.get(5)?,
        reasoning: None,
        effort_level: None,
        input_tokens: row.get(7)?,
        output_tokens: row.get(8)?,
        cache_creation_tokens: row.get(9)?,
        cache_creation_5m_tokens: row.get(13)?,
        cache_creation_1h_tokens: row.get(14)?,
        cache_read_tokens: row.get(10)?,
        agent_active_ms: row.get(11)?,
        started_at_epoch_s: row.get(4)?,
        driver: row.get(6)?,
        usage_unavailable: row.get(12)?,
    })
}

pub(super) fn query_coordinator_usage_records(
    conn: &Connection,
    since_epoch_s: i64,
    until_epoch_s: i64,
) -> Result<Vec<UsageRunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, status, host_id, driver, launched_model,
                first_event_at, last_event_at,
                MIN(first_event_at) OVER (PARTITION BY session_id),
                MAX(last_event_at) OVER (PARTITION BY session_id),
                model, input_tokens, output_tokens,
                cache_creation_tokens, cache_creation_5m_tokens, cache_creation_1h_tokens,
                cache_read_tokens, rounds, agent_active_ms, usage_unavailable,
                bucket_start_epoch_s
         FROM coordinator_usage_slices
         WHERE bucket_start_epoch_s >= ?1 AND bucket_start_epoch_s < ?2
         ORDER BY bucket_start_epoch_s ASC, id ASC",
    )?;
    let rows = stmt.query_map(params![since_epoch_s, until_epoch_s], map_usage_record)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn map_usage_record(row: &Row) -> rusqlite::Result<UsageRunRecord> {
    let session_id: String = row.get(1)?;
    let status: String = row.get(2)?;
    let host_id: String = row.get(3)?;
    let driver = row
        .get::<_, Option<String>>(4)?
        .filter(|slug| !slug.is_empty())
        .map_or(RecordedDriver::Unknown, RecordedDriver::LaunchTuple);
    let launched_model: Option<String> = row.get(5)?;
    let first_event_at: i64 = row.get(6)?;
    let last_event_at: i64 = row.get(7)?;
    let session_started_at: i64 = row.get(8)?;
    let session_last_event_at: i64 = row.get(9)?;
    let is_completed = status == "completed";

    Ok(UsageRunRecord {
        run_id: row.get(0)?,
        run_status: status.clone(),
        run_created_at_epoch_s: row.get(20)?,
        run_host_id: host_id.clone(),
        execution_id: format!("{COORDINATOR_EXECUTION_ID_PREFIX}{session_id}"),
        work_item_id: format!("{COORDINATOR_USAGE_KIND}:{session_id}"),
        execution_kind: COORDINATOR_USAGE_KIND.to_owned(),
        execution_status: status,
        execution_created_at_epoch_s: session_started_at,
        run_started_at_epoch_s: Some(first_event_at),
        run_finished_at_epoch_s: is_completed.then_some(last_event_at),
        rounds: row.get(17)?,
        agent_active_ms: row.get(18)?,
        run_model: row.get(10)?,
        input_tokens: row.get(11)?,
        output_tokens: row.get(12)?,
        cache_creation_tokens: row.get(13)?,
        cache_creation_5m_tokens: row.get(14)?,
        cache_creation_1h_tokens: row.get(15)?,
        cache_read_tokens: row.get(16)?,
        usage_unavailable: row.get(19)?,
        // Hourly coordinator slices are not worker runs with a first-call context.
        first_call_context_tokens: None,
        first_call_cache_write_tokens: None,
        peak_context_tokens: None,
        compactions: None,
        execution_started_at_epoch_s: Some(session_started_at),
        execution_finished_at_epoch_s: is_completed.then_some(session_last_event_at),
        execution_pr_url: None,
        execution_host_id: Some(host_id),
        driver,
        execution_model: launched_model,
        execution_effort: None,
        owning_task_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_db;

    #[test]
    fn migration_adds_coordinator_usage_slices_on_upgrade() {
        let (dir, db) = open_db();
        let path = db.path().to_path_buf();
        {
            let conn = db.connect().unwrap();
            conn.execute_batch(&format!(
                "DROP TABLE coordinator_usage_slices;
                 DELETE FROM schema_migrations WHERE id = '{MIGRATION_ID}';"
            ))
            .unwrap();
        }
        drop(db);
        let upgraded = WorkDb::open(path).unwrap();
        let conn = upgraded.connect().unwrap();
        let present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'coordinator_usage_slices')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            present,
            "opening an older database must create coordinator_usage_slices"
        );
        drop(dir);
    }

    #[test]
    fn upsert_is_idempotent_and_retires_spawn_placeholder() {
        let (_dir, db) = open_db();
        let placeholder = CoordinatorSessionSnapshot::builder()
            .session_id("spawn:tok-1")
            .spawn_token("tok-1")
            .slices(vec![
                CoordinatorUsageSlice::builder()
                    .session_id("spawn:tok-1")
                    .spawn_token("tok-1")
                    .source("claude_transcript")
                    .driver("claude")
                    .bucket_start_epoch_s(3_600)
                    .first_event_at(3_650)
                    .last_event_at(3_650)
                    .usage_unavailable("Coordinator transcript not found")
                    .build(),
            ])
            .build();
        db.upsert_coordinator_session_usage(&placeholder).unwrap();
        assert_eq!(db.cost_records_for_window(0, 10_000).unwrap().len(), 1);
        assert_eq!(db.cost_records_for_window(0, 10_000).unwrap()[0].input_tokens, None);

        let real = CoordinatorSessionSnapshot::builder()
            .session_id("sess-uuid-1")
            .spawn_token("tok-1")
            .slices(vec![
                CoordinatorUsageSlice::builder()
                    .session_id("sess-uuid-1")
                    .spawn_token("tok-1")
                    .source("claude_transcript")
                    .driver("claude")
                    .launched_model("opus")
                    .bucket_start_epoch_s(3_600)
                    .first_event_at(3_650)
                    .last_event_at(3_900)
                    .model("claude-opus-4-6")
                    .input_tokens(1_000)
                    .output_tokens(200)
                    .cache_creation_tokens(500)
                    .cache_read_tokens(9_000)
                    .rounds(3)
                    .agent_active_ms(12_000)
                    .build(),
            ])
            .build();
        // Upserting twice must produce the exact same row and retire the placeholder.
        db.upsert_coordinator_session_usage(&real).unwrap();
        db.upsert_coordinator_session_usage(&real).unwrap();

        let records = db.cost_records_for_window(0, 10_000).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].execution_kind, COORDINATOR_USAGE_KIND);
        assert_eq!(records[0].input_tokens, Some(1_000));
        assert_eq!(records[0].output_tokens, Some(200));
        assert_eq!(records[0].cache_creation_tokens, Some(500));
        assert_eq!(records[0].cache_read_tokens, Some(9_000));
        assert_eq!(records[0].usage_unavailable, None);
    }

    #[test]
    fn legacy_aggregate_upgrade_preserves_known_floor_with_explicit_gap() {
        let (_dir, db) = open_db();
        let path = db.path().to_owned();
        db.upsert_coordinator_session_usage(
            &CoordinatorSessionSnapshot::builder()
                .session_id("legacy")
                .slices(vec![measured_slice("legacy", 3600, 700, 3900)])
                .build(),
        )
        .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute_batch(
                "DROP TABLE coordinator_usage_facts;
                DROP TABLE coordinator_usage_capture_state;
                DELETE FROM schema_migrations WHERE id IN ('20261127_2215_coordinator_usage_facts', '20261127_2305_coordinator_usage_capture_state');",
            )
            .unwrap();
        }
        drop(db);
        let db = WorkDb::open(path).unwrap();
        let facts = BTreeMap::from([("message:parent".into(), measured_slice("legacy", 3600, 100, 3800))]);
        let update = CoordinatorSessionSnapshot::builder()
            .session_id("legacy")
            .facts(facts)
            .slices(vec![measured_slice("legacy", 3600, 100, 3800)])
            .build();
        db.upsert_coordinator_session_usage(&update).unwrap();
        db.upsert_coordinator_session_usage(&update).unwrap();
        let rows = db.cost_records_for_window(0, 10000).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, Some(700));
        assert!(rows[0].usage_unavailable.as_ref().unwrap().contains("Legacy aggregate"));
    }

    #[test]
    fn capture_state_upgrade_keeps_existing_identified_contributions() {
        let (_dir, db) = open_db();
        let path = db.path().to_owned();
        let slice = measured_slice("session", 3600, 700, 3900);
        let snapshot = CoordinatorSessionSnapshot::builder()
            .session_id("session")
            .facts(BTreeMap::from([("message:old".into(), slice.clone())]))
            .slices(vec![slice])
            .build();
        db.upsert_coordinator_session_usage(&snapshot).unwrap();
        db.connect()
            .unwrap()
            .execute_batch(
                "DROP TABLE coordinator_usage_capture_state;
            DELETE FROM schema_migrations WHERE id = '20261127_2305_coordinator_usage_capture_state';",
            )
            .unwrap();
        drop(db);
        let db = WorkDb::open(path).unwrap();
        db.upsert_coordinator_session_usage(&snapshot).unwrap();
        let rows = db.cost_records_for_window(0, 10000).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, Some(700));
        let count: i64 = db
            .connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM coordinator_usage_facts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn coordinator_window_query_uses_bucket_index() {
        let (_dir, db) = open_db();
        let conn = db.connect().unwrap();
        let plan: String = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT id FROM coordinator_usage_slices
                 WHERE bucket_start_epoch_s >= 1000 AND bucket_start_epoch_s < 2000",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("coordinator_usage_slices_bucket_idx"),
            "coordinator window read must use its index, got:\n{plan}"
        );
    }

    fn measured_slice(session: &str, bucket: i64, input: i64, last_event_at: i64) -> CoordinatorUsageSlice {
        CoordinatorUsageSlice::builder()
            .session_id(session)
            .source("claude_transcript")
            .driver("claude")
            .bucket_start_epoch_s(bucket)
            .first_event_at(bucket + 10)
            .last_event_at(last_event_at)
            .model("claude-opus-4-6")
            .input_tokens(input)
            .output_tokens(1)
            .cache_creation_tokens(0)
            .cache_read_tokens(0)
            .build()
    }

    #[test]
    fn incomplete_read_keeps_known_usage_and_records_the_gap() {
        let (_dir, db) = open_db();
        let good = CoordinatorSessionSnapshot::builder()
            .session_id("sess-1")
            .slices(vec![measured_slice("sess-1", 3_600, 700, 3_900)])
            .build();
        db.upsert_coordinator_session_usage(&good).unwrap();

        // The parent transcript could not be read: only an unmeasured placeholder.
        let unreadable = CoordinatorSessionSnapshot::builder()
            .session_id("sess-1")
            .slices(vec![
                CoordinatorUsageSlice::builder()
                    .session_id("sess-1")
                    .source("claude_transcript")
                    .bucket_start_epoch_s(7_200)
                    .first_event_at(7_200)
                    .last_event_at(7_200)
                    .usage_unavailable("unreadable")
                    .build(),
            ])
            .capture_gap("unreadable")
            .build();
        db.upsert_coordinator_session_usage(&unreadable).unwrap();

        let records = db.cost_records_for_window(0, 100_000).unwrap();
        assert_eq!(records.len(), 1, "no phantom placeholder next to measured usage");
        assert_eq!(records[0].input_tokens, Some(700), "known usage survives");
        assert_eq!(records[0].usage_unavailable.as_deref(), Some("unreadable"));

        // A partial re-read with fewer tokens never lowers the stored value,
        // while a later complete read clears the gap.
        let partial = CoordinatorSessionSnapshot::builder()
            .session_id("sess-1")
            .slices(vec![measured_slice("sess-1", 3_600, 500, 3_900)])
            .capture_gap("subagent unreadable")
            .build();
        db.upsert_coordinator_session_usage(&partial).unwrap();
        let records = db.cost_records_for_window(0, 100_000).unwrap();
        assert_eq!(records[0].input_tokens, Some(700));
        assert_eq!(records[0].usage_unavailable.as_deref(), Some("subagent unreadable"));

        let complete = CoordinatorSessionSnapshot::builder()
            .session_id("sess-1")
            .slices(vec![measured_slice("sess-1", 3_600, 800, 3_950)])
            .build();
        db.upsert_coordinator_session_usage(&complete).unwrap();
        let records = db.cost_records_for_window(0, 100_000).unwrap();
        assert_eq!(records[0].input_tokens, Some(800));
        assert_eq!(records[0].usage_unavailable, None);
    }

    #[test]
    fn full_replacement_that_would_lower_totals_is_downgraded_to_a_merge() {
        let (_dir, db) = open_db();
        db.upsert_coordinator_session_usage(
            &CoordinatorSessionSnapshot::builder()
                .session_id("sess-1")
                .slices(vec![measured_slice("sess-1", 3_600, 700, 3_900)])
                .build(),
        )
        .unwrap();
        // The re-read no longer sees part of the session (for example a
        // removed subagent transcript) yet claims to be complete.
        db.upsert_coordinator_session_usage(
            &CoordinatorSessionSnapshot::builder()
                .session_id("sess-1")
                .slices(vec![measured_slice("sess-1", 3_600, 300, 3_900)])
                .build(),
        )
        .unwrap();
        let records = db.cost_records_for_window(0, 100_000).unwrap();
        assert_eq!(records[0].input_tokens, Some(700));
        assert!(records[0].usage_unavailable.is_some());
    }

    #[test]
    fn window_membership_uses_the_hour_start_so_closed_windows_stay_stable() {
        let (_dir, db) = open_db();
        // Two messages in the hour starting at 3_600, on either side of 5_000.
        db.upsert_coordinator_session_usage(
            &CoordinatorSessionSnapshot::builder()
                .session_id("sess-1")
                .slices(vec![measured_slice("sess-1", 3_600, 100, 4_000)])
                .build(),
        )
        .unwrap();
        let closed_before = db.cost_records_for_window(3_600, 7_200).unwrap();
        assert_eq!(closed_before.len(), 1);
        assert_eq!(closed_before[0].created_at_epoch_s, 3_600);

        // A later message in the same hour (after the window's `until` of 5_000
        // for a sub-hour window) must not move the slice across the boundary.
        db.upsert_coordinator_session_usage(
            &CoordinatorSessionSnapshot::builder()
                .session_id("sess-1")
                .slices(vec![measured_slice("sess-1", 3_600, 250, 6_900)])
                .build(),
        )
        .unwrap();
        let sub_hour = db.cost_records_for_window(3_600, 5_000).unwrap();
        assert_eq!(sub_hour.len(), 1, "slice stays in the window containing its hour start");
        assert_eq!(db.cost_records_for_window(3_600, 7_200).unwrap().len(), 1);
        assert!(db.cost_records_for_window(0, 3_600).unwrap().is_empty());
        assert!(db.cost_records_for_window(7_200, 10_800).unwrap().is_empty());
    }
}
