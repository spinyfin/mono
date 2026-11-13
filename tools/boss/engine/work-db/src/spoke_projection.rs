//! Hub-side projection of durable spoke observations: the application cursor,
//! the effect outbox that makes each effect happen once, and the latest pane
//! observation of every assignment.
//!
//! `spoke_ingress` (see [`crate::spoke_ingress`]) is the *receipt*: original
//! frames and a contiguous cursor, committed before the hub acknowledges
//! anything. This module is the *application* of those frames to work state,
//! and it keeps the two independent. A frame is received long before, and
//! whether or not, it is ever applied; a crash between the two replays locally
//! and never asks the spoke to resend.
//!
//! Three durable facts live here:
//!
//! * **The cursor** (`spoke_projection.applied`): the highest ingress sequence
//!   whose effects have all been applied or recorded. It advances only in
//!   [`WorkDb::advance_spoke_projection`], one record at a time and in order,
//!   in the same transaction that records the effects the record owes.
//! * **Effects** (`spoke_effects`): work an applied record owes that may need a
//!   retry or the spoke's reachability, such as the decision for a queued
//!   proposal. Each has a stable `(host, kind, key)` identity, so two records
//!   that describe the same thing yield one effect and a replay never creates a
//!   second. An effect is recorded before the cursor passes its record, and is
//!   settled independently of the cursor.
//! * **Pane observations** (`spoke_pane_observations`): the newest
//!   `PaneState` the host reported for an assignment, kept so a restart does
//!   not forget `Exited(code)` versus `Missing` versus `Indeterminate`.
//!
//! Nothing here decides what an observation means for work status. The reducers
//! in `boss-spoke-hub` and the engine's existing completion, proposal and
//! liveness paths own that; this module only makes their inputs and outputs
//! durable and replay-safe.
use crate::*;

pub(crate) const MIGRATION_ID: &str = "20261112_1400_spoke_projection";
/// Turn state for parked remote workers, and a persisted halt reason per host.
pub(crate) const TURN_STATE_MIGRATION_ID: &str = "20261112_1900_spoke_turn_state";
/// Submission-keyed run-write results, projection stall columns, and
/// tombstones for unretained sequences that were receipted without a row.
pub(crate) const APPLIED_WRITES_MIGRATION_ID: &str = "20261112_2315_spoke_applied_writes";

/// Effect rows live in `spoke_effects.state`.
const PENDING: &str = "pending";
const DONE: &str = "done";
const FAILED: &str = "failed";

fn now_ms() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_ms() as i64
}

pub(crate) fn get_spoke_applied_write_on(
    conn: &Connection,
    application_key: &str,
) -> Result<Option<SpokeAppliedWrite>> {
    Ok(conn
        .query_row(
            "SELECT application_key, host_id, contract, accepted, reason, result_json, created_at_ms
               FROM spoke_applied_writes WHERE application_key = ?1",
            [application_key],
            |row| {
                Ok(SpokeAppliedWrite {
                    application_key: row.get(0)?,
                    host_id: row.get(1)?,
                    contract: row.get(2)?,
                    accepted: row.get::<_, i64>(3)? != 0,
                    reason: row.get(4)?,
                    result_json: row.get(5)?,
                    created_at_ms: row.get(6)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn insert_spoke_applied_write_on(conn: &Connection, write: &SpokeAppliedWrite) -> Result<()> {
    conn.execute(
        "INSERT INTO spoke_applied_writes
            (application_key, host_id, contract, accepted, reason, result_json, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(application_key) DO NOTHING",
        params![
            write.application_key,
            write.host_id,
            write.contract,
            if write.accepted { 1 } else { 0 },
            write.reason,
            write.result_json,
            write.created_at_ms,
        ],
    )?;
    Ok(())
}

/// Record a spoke submission's accepted result on `conn`, so it commits in the
/// same transaction as the domain write it describes. A no-op for a local write.
pub(crate) fn record_application_on(
    conn: &Connection,
    application: Option<crate::SpokeWriteApplication<'_>>,
) -> Result<()> {
    let Some(application) = application else {
        return Ok(());
    };
    insert_spoke_applied_write_on(
        conn,
        &SpokeAppliedWrite {
            application_key: application.key.to_owned(),
            host_id: application.host_id.to_owned(),
            contract: application.contract.to_owned(),
            accepted: true,
            reason: String::new(),
            result_json: serde_json::json!({"state": "applied"}).to_string(),
            created_at_ms: boss_engine_utils::epoch_time::now_epoch_ms() as i64,
        },
    )
}

/// Why the projection refused to move its cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SpokeProjectionFault {
    /// The record is not the one after the cursor. The projector applies in
    /// order, so this means two projectors are racing for one host.
    #[error("projection cursor is not at the record before this one")]
    OutOfOrder,
    /// The record is beyond the durable receipt, so it is not stored.
    #[error("record is beyond the durable receipt")]
    NotReceived,
    /// The host's cursor belongs to another journal. Never reset automatically.
    #[error("the host's projection cursor belongs to another journal")]
    JournalChanged,
}

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS spoke_projection (
            host_id    TEXT PRIMARY KEY,
            journal_id TEXT NOT NULL,
            applied    INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS spoke_effects (
            host_id       TEXT NOT NULL,
            journal_id    TEXT NOT NULL,
            sequence      INTEGER NOT NULL,
            effect_kind   TEXT NOT NULL,
            effect_key    TEXT NOT NULL,
            state         TEXT NOT NULL,
            payload       BLOB NOT NULL,
            attempts      INTEGER NOT NULL DEFAULT 0,
            rejections    INTEGER NOT NULL DEFAULT 0,
            last_error    TEXT,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY (host_id, journal_id, sequence, effect_kind, effect_key)
         );
         CREATE UNIQUE INDEX IF NOT EXISTS spoke_effects_key_idx
            ON spoke_effects(host_id, effect_kind, effect_key);
         CREATE INDEX IF NOT EXISTS spoke_effects_pending_idx
            ON spoke_effects(host_id, sequence) WHERE state = 'pending';
         CREATE TABLE IF NOT EXISTS spoke_pane_observations (
            execution_id          TEXT NOT NULL,
            assignment_generation INTEGER NOT NULL,
            host_id               TEXT NOT NULL,
            life                  INTEGER NOT NULL,
            session               TEXT NOT NULL,
            pane_id               TEXT NOT NULL,
            pid                   INTEGER NOT NULL,
            exit_code             INTEGER,
            boot_id               TEXT NOT NULL,
            source_sequence       INTEGER NOT NULL,
            observed_at_ms        INTEGER NOT NULL,
            PRIMARY KEY (execution_id, assignment_generation)
         );",
    )?;
    Ok(())
}

pub(crate) fn migrate_turn_state(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS spoke_turn_state (
            execution_id          TEXT PRIMARY KEY,
            assignment_generation INTEGER NOT NULL,
            parked                INTEGER NOT NULL
         );",
    )?;
    let has_halt: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('spoke_projection') WHERE name = 'halt_reason'",
        [],
        |row| row.get(0),
    )?;
    if has_halt == 0 {
        conn.execute_batch("ALTER TABLE spoke_projection ADD COLUMN halt_reason TEXT")?;
    }
    Ok(())
}

pub(crate) fn migrate_applied_writes(conn: &Connection) -> Result<()> {
    let has_stall: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('spoke_projection') WHERE name = 'stall_reason'",
        [],
        |row| row.get(0),
    )?;
    if has_stall == 0 {
        conn.execute_batch(
            "ALTER TABLE spoke_projection ADD COLUMN stall_reason TEXT;
             ALTER TABLE spoke_projection ADD COLUMN stalled_since_ms INTEGER",
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS spoke_applied_writes (
            application_key TEXT PRIMARY KEY,
            host_id TEXT NOT NULL,
            contract TEXT NOT NULL,
            accepted INTEGER NOT NULL,
            reason TEXT NOT NULL,
            result_json TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
         )",
    )?;
    // Backfill the holes the previous schema left above `pruned_through`.
    // Before tombstones, the only way a sequence above the prune point could be
    // receipted without a stored row was an unretained pane capture
    // (`retain = false` advanced `received` and stored nothing), and the only
    // `DELETE FROM spoke_ingress` is the prune, which moves `pruned_through`
    // past what it removes. So at upgrade time every such hole is evidence of a
    // live-only capture and nothing else, and it is filled as a frameless skip.
    // Sequences at or below `pruned_through` are never touched. A retained
    // frame that disappears after this migration is a hole with no tombstone,
    // which still halts the host.
    conn.execute_batch(
        "WITH RECURSIVE seqs(host_id, journal_id, seq, received) AS (
            SELECT host_id, journal_id, pruned_through + 1, received
              FROM spoke_journals
             WHERE received > pruned_through
            UNION ALL
            SELECT host_id, journal_id, seq + 1, received
              FROM seqs
             WHERE seq < received
         )
         INSERT OR IGNORE INTO spoke_ingress
            (host_id, journal_id, sequence, frame, epoch, features, observed_at_ms)
         SELECT host_id, journal_id, seq, X'', 0, '', 0
           FROM seqs
          WHERE NOT EXISTS (
                SELECT 1 FROM spoke_ingress i
                 WHERE i.host_id = seqs.host_id
                   AND i.journal_id = seqs.journal_id
                   AND i.sequence = seqs.seq
          )",
    )?;
    Ok(())
}

/// Where one host's projection stands against its durable receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpokeProjectionPosition {
    pub host_id: String,
    pub journal_id: String,
    /// Highest ingress sequence the hub has durably received.
    pub received: u64,
    /// Highest ingress sequence whose effects are applied or recorded.
    pub applied: u64,
    /// The cursor's journal, if a cursor exists. Differs from [`Self::journal_id`]
    /// when the host opened a new journal; projection then stops rather than
    /// re-applying the new journal from sequence 1.
    pub cursor_journal_id: Option<String>,
}

impl SpokeProjectionPosition {
    /// Records received but not yet applied.
    pub fn lag(&self) -> u64 {
        self.received.saturating_sub(self.applied)
    }
}

/// One stored ingress frame, exactly as received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpokeIngressRow {
    pub sequence: u64,
    pub frame: Vec<u8>,
    pub epoch: u32,
    pub features: String,
    pub observed_at_ms: i64,
}

/// An effect a record owes, supplied when its record's cursor advances.
#[derive(Debug, Clone, Copy)]
pub struct NewSpokeEffect<'a> {
    pub kind: &'a str,
    /// The effect's identity within `(host, kind)`. A second record that yields
    /// the same key adds nothing.
    pub key: &'a str,
    pub payload: &'a [u8],
    /// Where the effect starts. `Pending` is work still owed; `Failed` records
    /// something that is never retried, such as a frame that cannot be decoded.
    pub state: SpokeEffectState,
    /// Why, for a `Failed` effect.
    pub error: Option<&'a str>,
}

impl<'a> NewSpokeEffect<'a> {
    /// Work still owed.
    pub fn pending(kind: &'a str, key: &'a str, payload: &'a [u8]) -> Self {
        Self {
            kind,
            key,
            payload,
            state: SpokeEffectState::Pending,
            error: None,
        }
    }

    /// A record that could not be applied. Stored failed and never retried.
    pub fn failed(kind: &'a str, key: &'a str, payload: &'a [u8], error: &'a str) -> Self {
        Self {
            kind,
            key,
            payload,
            state: SpokeEffectState::Failed,
            error: Some(error),
        }
    }
}

/// What [`WorkDb::advance_spoke_projection`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpokeAdvance {
    /// The cursor moved. `duplicates` effects already existed under their
    /// `(host, kind, key)` and were not recorded again.
    Advanced { recorded: usize, duplicates: usize },
    /// The cursor was already at or past this record; nothing changed.
    AlreadyApplied,
}

/// Where an effect stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpokeEffectState {
    /// Recorded and not yet settled; it is retried until it is.
    Pending,
    Done,
    /// Settled without success. Never retried.
    Failed,
}

impl SpokeEffectState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => PENDING,
            Self::Done => DONE,
            Self::Failed => FAILED,
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            DONE => Self::Done,
            FAILED => Self::Failed,
            _ => Self::Pending,
        }
    }
}

/// A recorded effect.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct SpokeEffect {
    pub host_id: String,
    pub journal_id: String,
    /// The record that owes this effect.
    pub sequence: u64,
    pub kind: String,
    pub key: String,
    pub state: SpokeEffectState,
    pub payload: Vec<u8>,
    /// Delivery attempts of any outcome.
    pub attempts: u32,
    /// Attempts the host answered with a definite refusal. A refusal is the
    /// host's answer, unlike an unreachable host, which says nothing.
    pub rejections: u32,
    pub last_error: Option<String>,
}

/// How a delivery attempt of an effect ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpokeEffectOutcome {
    /// Delivered and acknowledged.
    Done,
    /// Not delivered and nothing learned (host unreachable, outcome uncertain).
    Retry(String),
    /// The host answered with a definite refusal. Counts toward `rejections`.
    Rejected(String),
    /// Settled without success; never retried.
    Failed(String),
}

/// Per-host projection health, for logs and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct SpokeProjectionStatus {
    pub position: SpokeProjectionPosition,
    pub pending_effects: u64,
    pub failed_effects: u64,
    /// Why this host's projector is halted, if it is. Cleared once the host
    /// catches up.
    pub halt_reason: Option<String>,
    /// Why the current record is being retried in place, once the retry has
    /// lasted long enough to surface on engine health. Cleared when the record
    /// applies or the host halts.
    pub stall_reason: Option<String>,
    pub stalled_since_ms: Option<i64>,
}

/// Durable result of a run-scoped write, keyed by the submission identity.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct SpokeAppliedWrite {
    pub application_key: String,
    pub host_id: String,
    pub contract: String,
    pub accepted: bool,
    pub reason: String,
    pub result_json: String,
    pub created_at_ms: i64,
}

/// Latest main-session turn for a spoke-supervised run, rebuilt into memory
/// after a hub restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpokeTurnState {
    pub execution_id: String,
    pub assignment_generation: u64,
    pub parked: bool,
}

/// The newest `PaneState` a host reported for an assignment.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct SpokePaneObservation {
    pub execution_id: String,
    pub assignment_generation: u64,
    pub host_id: String,
    /// The wire `PaneLife` number; permanent in the schema.
    pub life: i32,
    pub session: String,
    pub pane_id: String,
    pub pid: u64,
    pub exit_code: Option<i32>,
    pub boot_id: String,
    /// The ingress sequence that carried it. Observations apply in order, so a
    /// lower sequence never replaces a higher one.
    pub source_sequence: u64,
    pub observed_at_ms: i64,
}

fn fault(fault: SpokeProjectionFault) -> anyhow::Error {
    fault.into()
}

fn map_effect(row: &Row<'_>) -> rusqlite::Result<SpokeEffect> {
    Ok(SpokeEffect {
        host_id: row.get(0)?,
        journal_id: row.get(1)?,
        sequence: row.get::<_, i64>(2)? as u64,
        kind: row.get(3)?,
        key: row.get(4)?,
        state: SpokeEffectState::parse(&row.get::<_, String>(5)?),
        payload: row.get(6)?,
        attempts: row.get::<_, i64>(7)? as u32,
        rejections: row.get::<_, i64>(8)? as u32,
        last_error: row.get(9)?,
    })
}

const EFFECT_COLUMNS: &str =
    "host_id, journal_id, sequence, effect_kind, effect_key, state, payload, attempts, rejections, last_error";

#[work_db_boundary::instrument]
impl WorkDb {
    /// Every host with received records its projection has not yet applied,
    /// least-recently-served ordering left to the caller.
    pub fn spoke_hosts_behind(&self) -> Result<Vec<SpokeProjectionPosition>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT j.host_id, j.journal_id, j.received,
                    CASE WHEN p.journal_id = j.journal_id THEN p.applied ELSE 0 END,
                    p.journal_id
               FROM spoke_journals j LEFT JOIN spoke_projection p ON p.host_id = j.host_id
              WHERE j.received >
                    COALESCE(CASE WHEN p.journal_id = j.journal_id THEN p.applied END, 0)
              ORDER BY j.host_id",
        )?;
        let rows = statement.query_map([], position_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// One host's receipt and application cursors, if it has a journal.
    pub fn spoke_projection_position(&self, host_id: &str) -> Result<Option<SpokeProjectionPosition>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT j.host_id, j.journal_id, j.received,
                        CASE WHEN p.journal_id = j.journal_id THEN p.applied ELSE 0 END,
                        p.journal_id
                   FROM spoke_journals j LEFT JOIN spoke_projection p ON p.host_id = j.host_id
                  WHERE j.host_id = ?1",
                [host_id],
                position_row,
            )
            .optional()?)
    }

    /// Up to `limit` stored frames after `after`, in sequence order.
    pub fn spoke_ingress_after(
        &self,
        host_id: &str,
        journal_id: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<SpokeIngressRow>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT sequence, frame, epoch, features, observed_at_ms FROM spoke_ingress
              WHERE host_id = ?1 AND journal_id = ?2 AND sequence > ?3
              ORDER BY sequence LIMIT ?4",
        )?;
        let rows = statement.query_map(params![host_id, journal_id, after as i64, limit], |row| {
            Ok(SpokeIngressRow {
                sequence: row.get::<_, i64>(0)? as u64,
                frame: row.get(1)?,
                epoch: row.get::<_, i64>(2)? as u32,
                features: row.get(3)?,
                observed_at_ms: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether advancing the cursor past `sequence` would succeed, without
    /// recording effects. Call this before applying a record so a journal
    /// mismatch or a gap is detected before side effects run.
    pub fn check_spoke_projection_advance(
        &self,
        host_id: &str,
        journal_id: &str,
        sequence: u64,
    ) -> Result<SpokeAdvance> {
        let conn = self.connect()?;
        match projection_cursor(&conn, host_id, journal_id, sequence)? {
            CursorCheck::AlreadyApplied => Ok(SpokeAdvance::AlreadyApplied),
            CursorCheck::Ready => Ok(SpokeAdvance::Advanced {
                recorded: 0,
                duplicates: 0,
            }),
        }
    }

    /// Record the effects `sequence` owes and move the cursor past it, in one
    /// transaction. The cursor moves one record at a time: `sequence` must be
    /// the record after it, and must already be durably received.
    ///
    /// Re-advancing a record the cursor has passed changes nothing, so a
    /// projector that crashed after this commit and replays the record is
    /// harmless. An effect whose `(host, kind, key)` already exists is counted
    /// as a duplicate and not recorded again: that is how two records that
    /// describe the same proposal yield one decision.
    pub fn advance_spoke_projection(
        &self,
        host_id: &str,
        journal_id: &str,
        sequence: u64,
        effects: &[NewSpokeEffect<'_>],
    ) -> Result<SpokeAdvance> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if matches!(
            projection_cursor(&tx, host_id, journal_id, sequence)?,
            CursorCheck::AlreadyApplied
        ) {
            return Ok(SpokeAdvance::AlreadyApplied);
        }
        let now = now_ms();
        let (mut recorded, mut duplicates) = (0, 0);
        for effect in effects {
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO spoke_effects
                    (host_id, journal_id, sequence, effect_kind, effect_key, state, payload,
                     last_error, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                params![
                    host_id,
                    journal_id,
                    sequence as i64,
                    effect.kind,
                    effect.key,
                    effect.state.as_str(),
                    effect.payload,
                    effect.error,
                    now
                ],
            )?;
            if inserted == 0 {
                duplicates += 1;
            } else {
                recorded += 1;
            }
        }
        tx.execute(
            "INSERT INTO spoke_projection(host_id, journal_id, applied) VALUES (?1, ?2, ?3)
             ON CONFLICT(host_id) DO UPDATE SET applied = excluded.applied",
            params![host_id, journal_id, sequence as i64],
        )?;
        // Capture tombstones (empty frames) have no further use once the
        // cursor has passed them. They are outside the ingress quota, so
        // nothing else would delete them.
        tx.execute(
            "DELETE FROM spoke_ingress
              WHERE host_id = ?1 AND journal_id = ?2 AND sequence = ?3 AND LENGTH(frame) = 0",
            params![host_id, journal_id, sequence as i64],
        )?;
        tx.commit()?;
        Ok(SpokeAdvance::Advanced { recorded, duplicates })
    }

    /// Whether an effect with this identity has ever been recorded for the
    /// host, in any state. A reducer asks before re-applying a thing that a
    /// different record already carried.
    pub fn spoke_effect_exists(&self, host_id: &str, kind: &str, key: &str) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM spoke_effects
              WHERE host_id = ?1 AND effect_kind = ?2 AND effect_key = ?3)",
            params![host_id, kind, key],
            |row| row.get(0),
        )?)
    }

    /// The stored effect with this identity, if any.
    pub fn spoke_effect(&self, host_id: &str, kind: &str, key: &str) -> Result<Option<SpokeEffect>> {
        Ok(self
            .connect()?
            .query_row(
                &format!(
                    "SELECT {EFFECT_COLUMNS} FROM spoke_effects
                      WHERE host_id = ?1 AND effect_kind = ?2 AND effect_key = ?3"
                ),
                params![host_id, kind, key],
                map_effect,
            )
            .optional()?)
    }

    /// Up to `limit` pending effects per host, oldest record first within each
    /// host. A bound per host so one unreachable host cannot fill the batch
    /// and starve every later host.
    pub fn pending_spoke_effects(&self, limit: u32) -> Result<Vec<SpokeEffect>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(&format!(
            "SELECT {EFFECT_COLUMNS} FROM (
                SELECT {EFFECT_COLUMNS},
                       ROW_NUMBER() OVER (PARTITION BY host_id ORDER BY sequence, effect_kind) AS rn
                  FROM spoke_effects
                 WHERE state = 'pending'
             ) WHERE rn <= ?1
             ORDER BY host_id, sequence, effect_kind"
        ))?;
        let rows = statement.query_map([limit], map_effect)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Settle one delivery attempt. A settled effect is never reopened, so a
    /// late or repeated report cannot undo `Done`.
    pub fn settle_spoke_effect(
        &self,
        host_id: &str,
        kind: &str,
        key: &str,
        outcome: &SpokeEffectOutcome,
    ) -> Result<()> {
        let conn = self.connect()?;
        let now = now_ms();
        let (state, rejected, error): (&str, i64, Option<&str>) = match outcome {
            SpokeEffectOutcome::Done => (DONE, 0, None),
            SpokeEffectOutcome::Retry(reason) => (PENDING, 0, Some(reason.as_str())),
            SpokeEffectOutcome::Rejected(reason) => (PENDING, 1, Some(reason.as_str())),
            SpokeEffectOutcome::Failed(reason) => (FAILED, 0, Some(reason.as_str())),
        };
        conn.execute(
            "UPDATE spoke_effects
                SET state = ?4, attempts = attempts + 1, rejections = rejections + ?5,
                    last_error = ?6, updated_at_ms = ?7
              WHERE host_id = ?1 AND effect_kind = ?2 AND effect_key = ?3 AND state = 'pending'",
            params![host_id, kind, key, state, rejected, error, now],
        )?;
        Ok(())
    }

    /// Replace the payload of a still-pending effect. Delivery uses this when a
    /// successful worker reply is rewritten to a data-free rejection so the
    /// stored command matches what the outbox will admit.
    pub fn replace_pending_spoke_effect_payload(
        &self,
        host_id: &str,
        kind: &str,
        key: &str,
        payload: &[u8],
    ) -> Result<()> {
        let conn = self.connect()?;
        let now = now_ms();
        let rows = conn.execute(
            "UPDATE spoke_effects
                SET payload = ?4, updated_at_ms = ?5
              WHERE host_id = ?1 AND effect_kind = ?2 AND effect_key = ?3 AND state = 'pending'",
            params![host_id, kind, key, payload, now],
        )?;
        if rows == 0 {
            bail!("no pending spoke effect {kind}/{key} on {host_id}");
        }
        Ok(())
    }

    /// Per-host projection health. Hosts with no journal are omitted.
    pub fn spoke_projection_status(&self) -> Result<Vec<SpokeProjectionStatus>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT j.host_id, j.journal_id, j.received,
                    CASE WHEN p.journal_id = j.journal_id THEN p.applied ELSE 0 END,
                    p.journal_id,
                    (SELECT COUNT(*) FROM spoke_effects e WHERE e.host_id = j.host_id AND e.state = 'pending'),
                    (SELECT COUNT(*) FROM spoke_effects e WHERE e.host_id = j.host_id AND e.state = 'failed'),
                    p.halt_reason,
                    p.stall_reason,
                    p.stalled_since_ms
               FROM spoke_journals j LEFT JOIN spoke_projection p ON p.host_id = j.host_id
              ORDER BY j.host_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SpokeProjectionStatus {
                position: position_row(row)?,
                pending_effects: row.get::<_, i64>(5)? as u64,
                failed_effects: row.get::<_, i64>(6)? as u64,
                halt_reason: row.get(7)?,
                stall_reason: row.get(8)?,
                stalled_since_ms: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Keep the newest pane observation of an assignment. Returns whether it
    /// replaced (or created) the stored one; a lower sequence is ignored.
    pub fn record_spoke_pane_observation(&self, observation: &SpokePaneObservation) -> Result<bool> {
        let changed = self.connect()?.execute(
            "INSERT INTO spoke_pane_observations
                (execution_id, assignment_generation, host_id, life, session, pane_id, pid,
                 exit_code, boot_id, source_sequence, observed_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(execution_id, assignment_generation) DO UPDATE SET
                host_id = excluded.host_id, life = excluded.life, session = excluded.session,
                pane_id = excluded.pane_id, pid = excluded.pid, exit_code = excluded.exit_code,
                boot_id = excluded.boot_id, source_sequence = excluded.source_sequence,
                observed_at_ms = excluded.observed_at_ms
              WHERE excluded.source_sequence > spoke_pane_observations.source_sequence",
            params![
                observation.execution_id,
                observation.assignment_generation as i64,
                observation.host_id,
                observation.life,
                observation.session,
                observation.pane_id,
                observation.pid as i64,
                observation.exit_code,
                observation.boot_id,
                observation.source_sequence as i64,
                observation.observed_at_ms,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Persist why this host's projector is halted. Cleared when the host
    /// catches up.
    pub fn set_spoke_projection_halt(&self, host_id: &str, reason: &str) -> Result<()> {
        let conn = self.connect()?;
        let updated = conn.execute(
            "UPDATE spoke_projection SET halt_reason = ?2 WHERE host_id = ?1",
            params![host_id, reason],
        )?;
        if updated == 0 {
            conn.execute(
                "INSERT INTO spoke_projection(host_id, journal_id, applied, halt_reason)
                 SELECT host_id, journal_id, 0, ?2 FROM spoke_journals WHERE host_id = ?1",
                params![host_id, reason],
            )?;
        }
        Ok(())
    }

    /// Clear a persisted halt once the host has no remaining lag.
    pub fn clear_spoke_projection_halt(&self, host_id: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE spoke_projection SET halt_reason = NULL, stall_reason = NULL, stalled_since_ms = NULL
              WHERE host_id = ?1",
            [host_id],
        )?;
        Ok(())
    }

    /// Persist why this host's current record is retrying in place.
    pub fn set_spoke_projection_stall(&self, host_id: &str, reason: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_ms();
        let updated = conn.execute(
            "UPDATE spoke_projection
                SET stall_reason = ?2, stalled_since_ms = COALESCE(stalled_since_ms, ?3)
              WHERE host_id = ?1",
            params![host_id, reason, now],
        )?;
        if updated == 0 {
            conn.execute(
                "INSERT INTO spoke_projection(host_id, journal_id, applied, stall_reason, stalled_since_ms)
                 SELECT host_id, journal_id, 0, ?2, ?3 FROM spoke_journals WHERE host_id = ?1",
                params![host_id, reason, now],
            )?;
        }
        Ok(())
    }

    /// Clear a stall once the record applies.
    pub fn clear_spoke_projection_stall(&self, host_id: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE spoke_projection SET stall_reason = NULL, stalled_since_ms = NULL WHERE host_id = ?1",
            [host_id],
        )?;
        Ok(())
    }

    pub fn get_spoke_applied_write(&self, application_key: &str) -> Result<Option<SpokeAppliedWrite>> {
        let conn = self.connect()?;
        get_spoke_applied_write_on(&conn, application_key)
    }

    pub fn record_spoke_applied_write(&self, write: &SpokeAppliedWrite) -> Result<()> {
        let conn = self.connect()?;
        insert_spoke_applied_write_on(&conn, write)
    }

    /// Persist the latest main-session turn for a spoke-supervised run.
    pub fn upsert_spoke_turn_state(&self, state: &SpokeTurnState) -> Result<()> {
        self.connect()?.execute(
            "INSERT INTO spoke_turn_state(execution_id, assignment_generation, parked)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(execution_id) DO UPDATE SET
                assignment_generation = excluded.assignment_generation,
                parked = excluded.parked",
            params![
                state.execution_id,
                state.assignment_generation as i64,
                if state.parked { 1 } else { 0 }
            ],
        )?;
        Ok(())
    }

    /// Every stored main-session turn, for rebuilding memory after a restart.
    pub fn spoke_turn_states(&self) -> Result<Vec<SpokeTurnState>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare("SELECT execution_id, assignment_generation, parked FROM spoke_turn_state")?;
        let rows = statement.query_map([], |row| {
            Ok(SpokeTurnState {
                execution_id: row.get(0)?,
                assignment_generation: row.get::<_, i64>(1)? as u64,
                parked: row.get::<_, i64>(2)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Drop stored turn state when the assignment is released.
    pub fn forget_spoke_turn_state(&self, execution_id: &str) -> Result<()> {
        self.connect()?
            .execute("DELETE FROM spoke_turn_state WHERE execution_id = ?1", [execution_id])?;
        Ok(())
    }

    /// The newest pane observation of the execution's latest assignment.
    pub fn spoke_pane_observation(&self, execution_id: &str) -> Result<Option<SpokePaneObservation>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT execution_id, assignment_generation, host_id, life, session, pane_id, pid,
                        exit_code, boot_id, source_sequence, observed_at_ms
                   FROM spoke_pane_observations WHERE execution_id = ?1
                  ORDER BY assignment_generation DESC LIMIT 1",
                [execution_id],
                |row| {
                    Ok(SpokePaneObservation {
                        execution_id: row.get(0)?,
                        assignment_generation: row.get::<_, i64>(1)? as u64,
                        host_id: row.get(2)?,
                        life: row.get(3)?,
                        session: row.get(4)?,
                        pane_id: row.get(5)?,
                        pid: row.get::<_, i64>(6)? as u64,
                        exit_code: row.get(7)?,
                        boot_id: row.get(8)?,
                        source_sequence: row.get::<_, i64>(9)? as u64,
                        observed_at_ms: row.get(10)?,
                    })
                },
            )
            .optional()?)
    }
}

fn position_row(row: &Row<'_>) -> rusqlite::Result<SpokeProjectionPosition> {
    Ok(SpokeProjectionPosition {
        host_id: row.get(0)?,
        journal_id: row.get(1)?,
        received: row.get::<_, i64>(2)? as u64,
        applied: row.get::<_, i64>(3)? as u64,
        cursor_journal_id: row.get(4)?,
    })
}

enum CursorCheck {
    Ready,
    AlreadyApplied,
}

fn projection_cursor(conn: &Connection, host_id: &str, journal_id: &str, sequence: u64) -> Result<CursorCheck> {
    let journal: Option<(String, i64)> = conn
        .query_row(
            "SELECT journal_id, received FROM spoke_journals WHERE host_id = ?1",
            [host_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((stored_journal, received)) = journal else {
        return Err(fault(SpokeProjectionFault::NotReceived));
    };
    if stored_journal != journal_id {
        return Err(fault(SpokeProjectionFault::JournalChanged));
    }
    if sequence == 0 || sequence > received as u64 {
        return Err(fault(SpokeProjectionFault::NotReceived));
    }
    let cursor: Option<(String, i64)> = conn
        .query_row(
            "SELECT journal_id, applied FROM spoke_projection WHERE host_id = ?1",
            [host_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let applied = match cursor {
        None => 0,
        Some((cursor_journal, applied)) if cursor_journal == journal_id => applied as u64,
        Some(_) => return Err(fault(SpokeProjectionFault::JournalChanged)),
    };
    if sequence <= applied {
        return Ok(CursorCheck::AlreadyApplied);
    }
    if sequence != applied + 1 {
        return Err(fault(SpokeProjectionFault::OutOfOrder));
    }
    Ok(CursorCheck::Ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
    }

    fn receive(db: &WorkDb, host: &str, count: u64) -> SpokeJournalAdmission {
        let admission = db.open_spoke_journal(host, "journal", 0, 1).unwrap();
        for sequence in 1..=count {
            db.commit_spoke_record(
                SpokeIngressCommit::builder()
                    .host_id(host)
                    .generation(admission.generation)
                    .journal_id(&admission.journal_id)
                    .sequence(sequence)
                    .frame(format!("frame-{sequence}").as_bytes())
                    .epoch(1)
                    .features("execution=1")
                    .observed_at_ms(1)
                    .runs(&[])
                    .build(),
            )
            .unwrap();
        }
        admission
    }

    fn fault_of(error: &anyhow::Error) -> Option<SpokeProjectionFault> {
        error.downcast_ref::<SpokeProjectionFault>().copied()
    }

    fn effect<'a>(kind: &'a str, key: &'a str, payload: &'a [u8]) -> NewSpokeEffect<'a> {
        NewSpokeEffect::pending(kind, key, payload)
    }

    #[test]
    fn a_host_is_behind_until_every_received_record_is_applied() {
        let db = db();
        receive(&db, "host-a", 3);
        receive(&db, "host-b", 1);
        let behind = db.spoke_hosts_behind().unwrap();
        assert_eq!(
            behind.iter().map(|p| (p.host_id.as_str(), p.lag())).collect::<Vec<_>>(),
            [("host-a", 3), ("host-b", 1)]
        );
        for sequence in 1..=3 {
            db.advance_spoke_projection("host-a", "journal", sequence, &[]).unwrap();
        }
        db.advance_spoke_projection("host-b", "journal", 1, &[]).unwrap();
        assert!(db.spoke_hosts_behind().unwrap().is_empty());
        assert_eq!(db.spoke_projection_position("host-a").unwrap().unwrap().applied, 3);
    }

    #[test]
    fn the_cursor_moves_one_received_record_at_a_time() {
        let db = db();
        receive(&db, "host-a", 2);
        let out_of_order = db.advance_spoke_projection("host-a", "journal", 2, &[]).unwrap_err();
        assert_eq!(fault_of(&out_of_order), Some(SpokeProjectionFault::OutOfOrder));
        let unreceived = db.advance_spoke_projection("host-a", "journal", 3, &[]).unwrap_err();
        assert_eq!(fault_of(&unreceived), Some(SpokeProjectionFault::NotReceived));
        let foreign = db.advance_spoke_projection("host-a", "other", 1, &[]).unwrap_err();
        assert_eq!(fault_of(&foreign), Some(SpokeProjectionFault::JournalChanged));
        assert_eq!(
            db.advance_spoke_projection("host-a", "journal", 1, &[]).unwrap(),
            SpokeAdvance::Advanced {
                recorded: 0,
                duplicates: 0
            }
        );
        assert_eq!(
            db.advance_spoke_projection("host-a", "journal", 1, &[]).unwrap(),
            SpokeAdvance::AlreadyApplied
        );
        assert_eq!(db.spoke_projection_position("host-a").unwrap().unwrap().applied, 1);
    }

    #[test]
    fn effects_are_recorded_with_the_cursor_and_never_twice() {
        let db = db();
        receive(&db, "host-a", 3);
        let first = db
            .advance_spoke_projection("host-a", "journal", 1, &[effect("decision", "run:p1", b"first")])
            .unwrap();
        assert_eq!(
            first,
            SpokeAdvance::Advanced {
                recorded: 1,
                duplicates: 0
            }
        );
        // Re-advancing the same record is a no-op and keeps the first payload.
        assert_eq!(
            db.advance_spoke_projection("host-a", "journal", 1, &[effect("decision", "run:p1", b"again")])
                .unwrap(),
            SpokeAdvance::AlreadyApplied
        );
        // A different record describing the same thing yields no second effect.
        let second = db
            .advance_spoke_projection("host-a", "journal", 2, &[effect("decision", "run:p1", b"other")])
            .unwrap();
        assert_eq!(
            second,
            SpokeAdvance::Advanced {
                recorded: 0,
                duplicates: 1
            }
        );
        let stored = db.spoke_effect("host-a", "decision", "run:p1").unwrap().unwrap();
        assert_eq!((stored.sequence, stored.payload.as_slice()), (1, &b"first"[..]));
        assert!(db.spoke_effect_exists("host-a", "decision", "run:p1").unwrap());
        // The same key under another host or kind is a different effect.
        db.advance_spoke_projection(
            "host-a",
            "journal",
            3,
            &[effect("decision", "run:p2", b"x"), effect("quarantine", "run:p1", b"y")],
        )
        .unwrap();
        assert_eq!(db.pending_spoke_effects(10).unwrap().len(), 3);
    }

    #[test]
    fn a_pending_effect_payload_can_be_replaced_until_it_settles() {
        let db = db();
        receive(&db, "host-a", 1);
        db.advance_spoke_projection("host-a", "journal", 1, &[effect("reply", "run:1", b"ok")])
            .unwrap();
        db.replace_pending_spoke_effect_payload("host-a", "reply", "run:1", b"denied")
            .unwrap();
        assert_eq!(
            db.spoke_effect("host-a", "reply", "run:1").unwrap().unwrap().payload,
            b"denied"
        );
        db.settle_spoke_effect("host-a", "reply", "run:1", &SpokeEffectOutcome::Done)
            .unwrap();
        db.replace_pending_spoke_effect_payload("host-a", "reply", "run:1", b"again")
            .unwrap_err();
        assert_eq!(
            db.spoke_effect("host-a", "reply", "run:1").unwrap().unwrap().payload,
            b"denied"
        );
    }

    #[test]
    fn a_failed_effect_is_stored_and_never_retried() {
        let db = db();
        receive(&db, "host-a", 1);
        db.advance_spoke_projection(
            "host-a",
            "journal",
            1,
            &[NewSpokeEffect::failed("quarantine", "1", b"bytes", "undecodable frame")],
        )
        .unwrap();
        assert!(db.pending_spoke_effects(10).unwrap().is_empty());
        let stored = db.spoke_effect("host-a", "quarantine", "1").unwrap().unwrap();
        assert_eq!(
            (stored.state, stored.last_error.as_deref()),
            (SpokeEffectState::Failed, Some("undecodable frame"))
        );
        assert_eq!(db.spoke_projection_status().unwrap()[0].failed_effects, 1);
    }

    #[test]
    fn settlement_is_final_and_counts_definite_refusals_separately() {
        let db = db();
        receive(&db, "host-a", 2);
        db.advance_spoke_projection(
            "host-a",
            "journal",
            1,
            &[effect("decision", "a", b"a"), effect("decision", "b", b"b")],
        )
        .unwrap();
        db.settle_spoke_effect("host-a", "decision", "a", &SpokeEffectOutcome::Retry("offline".into()))
            .unwrap();
        db.settle_spoke_effect(
            "host-a",
            "decision",
            "a",
            &SpokeEffectOutcome::Rejected("expired".into()),
        )
        .unwrap();
        let pending = db.spoke_effect("host-a", "decision", "a").unwrap().unwrap();
        assert_eq!(
            (
                pending.state,
                pending.attempts,
                pending.rejections,
                pending.last_error.as_deref()
            ),
            (SpokeEffectState::Pending, 2, 1, Some("expired"))
        );
        db.settle_spoke_effect("host-a", "decision", "a", &SpokeEffectOutcome::Done)
            .unwrap();
        // A late failure report cannot reopen or fail a settled effect.
        db.settle_spoke_effect("host-a", "decision", "a", &SpokeEffectOutcome::Failed("late".into()))
            .unwrap();
        let done = db.spoke_effect("host-a", "decision", "a").unwrap().unwrap();
        assert_eq!((done.state, done.attempts), (SpokeEffectState::Done, 3));
        db.settle_spoke_effect("host-a", "decision", "b", &SpokeEffectOutcome::Failed("gone".into()))
            .unwrap();
        assert!(db.pending_spoke_effects(10).unwrap().is_empty());
        let status = db.spoke_projection_status().unwrap();
        assert_eq!(
            (
                status[0].pending_effects,
                status[0].failed_effects,
                status[0].position.lag()
            ),
            (0, 1, 1)
        );
    }

    #[test]
    fn the_cursor_and_effects_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.db");
        {
            let db = WorkDb::open(path.clone()).unwrap();
            receive(&db, "host-a", 2);
            db.advance_spoke_projection("host-a", "journal", 1, &[effect("decision", "k", b"payload")])
                .unwrap();
        }
        let db = WorkDb::open(path).unwrap();
        let position = db.spoke_projection_position("host-a").unwrap().unwrap();
        assert_eq!((position.received, position.applied), (2, 1));
        assert_eq!(db.pending_spoke_effects(10).unwrap()[0].payload, b"payload");
        assert_eq!(
            db.spoke_ingress_after("host-a", "journal", position.applied, 10)
                .unwrap()
                .iter()
                .map(|row| row.sequence)
                .collect::<Vec<_>>(),
            [2]
        );
    }

    #[test]
    fn a_newer_pane_observation_replaces_and_an_older_one_does_not() {
        let db = db();
        let observation = |sequence: u64, life: i32, exit_code: Option<i32>| {
            SpokePaneObservation::builder()
                .execution_id("exec_1")
                .assignment_generation(1)
                .host_id("host-a")
                .life(life)
                .session("worker")
                .pane_id("%1")
                .pid(42)
                .maybe_exit_code(exit_code)
                .boot_id("boot")
                .source_sequence(sequence)
                .observed_at_ms(1_000 + sequence as i64)
                .build()
        };
        assert!(db.record_spoke_pane_observation(&observation(5, 1, None)).unwrap());
        assert!(db.record_spoke_pane_observation(&observation(7, 2, Some(3))).unwrap());
        assert!(!db.record_spoke_pane_observation(&observation(6, 1, None)).unwrap());
        assert!(!db.record_spoke_pane_observation(&observation(7, 2, Some(3))).unwrap());
        let stored = db.spoke_pane_observation("exec_1").unwrap().unwrap();
        assert_eq!((stored.life, stored.exit_code, stored.source_sequence), (2, Some(3), 7));
        assert!(db.spoke_pane_observation("exec_2").unwrap().is_none());
    }

    #[test]
    fn pending_effects_are_fair_across_hosts() {
        let db = db();
        receive(&db, "host-a", 70);
        receive(&db, "host-b", 1);
        for sequence in 1..=70 {
            db.advance_spoke_projection(
                "host-a",
                "journal",
                sequence,
                &[effect("decision", &format!("a-{sequence}"), b"a")],
            )
            .unwrap();
        }
        db.advance_spoke_projection("host-b", "journal", 1, &[effect("decision", "b-1", b"b")])
            .unwrap();
        let pending = db.pending_spoke_effects(64).unwrap();
        assert!(
            pending.iter().any(|effect| effect.host_id == "host-b"),
            "a reachable later host must appear even when an earlier host has more than one batch pending: {pending:?}"
        );
        assert!(
            pending.iter().filter(|effect| effect.host_id == "host-a").count() <= 64,
            "one host must not fill the batch: {pending:?}"
        );
    }

    #[test]
    fn check_advance_rejects_a_journal_mismatch_before_any_write() {
        let db = db();
        receive(&db, "host-a", 1);
        db.advance_spoke_projection("host-a", "journal", 1, &[]).unwrap();
        let error = db.check_spoke_projection_advance("host-a", "other", 1).unwrap_err();
        assert_eq!(fault_of(&error), Some(SpokeProjectionFault::JournalChanged));
        assert_eq!(
            db.check_spoke_projection_advance("host-a", "journal", 1).unwrap(),
            SpokeAdvance::AlreadyApplied
        );
    }

    #[test]
    fn upgrade_fills_only_holes_above_the_prune_point_with_frameless_tombstones() {
        let db = db();
        receive(&db, "host-a", 4);
        {
            let conn = db.connect().unwrap();
            // An old-schema unretained capture at 3, and a pruned prefix.
            conn.execute("DELETE FROM spoke_ingress WHERE sequence = 3", [])
                .unwrap();
            conn.execute("DELETE FROM spoke_ingress WHERE sequence <= 1", [])
                .unwrap();
            conn.execute("UPDATE spoke_journals SET pruned_through = 1", [])
                .unwrap();
            migrate_applied_writes(&conn).unwrap();
        }
        assert!(db.spoke_ingress_frame("host-a", 1).unwrap().is_none());
        assert_eq!(
            db.spoke_ingress_frame("host-a", 2).unwrap().as_deref(),
            Some(&b"frame-2"[..])
        );
        assert_eq!(db.spoke_ingress_frame("host-a", 3).unwrap().as_deref(), Some(&b""[..]));
        assert_eq!(
            db.spoke_ingress_frame("host-a", 4).unwrap().as_deref(),
            Some(&b"frame-4"[..])
        );
    }

    #[test]
    fn advancing_the_cursor_deletes_passed_capture_tombstones() {
        let db = db();
        let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        db.commit_spoke_record(
            SpokeIngressCommit::builder()
                .host_id("host-a")
                .generation(admission.generation)
                .journal_id(&admission.journal_id)
                .sequence(1)
                .frame(b"kept")
                .epoch(1)
                .features("execution=1")
                .observed_at_ms(1)
                .runs(&[])
                .build(),
        )
        .unwrap();
        db.commit_spoke_record(
            SpokeIngressCommit::builder()
                .host_id("host-a")
                .generation(admission.generation)
                .journal_id(&admission.journal_id)
                .sequence(2)
                .frame(b"capture")
                .epoch(1)
                .features("execution=1")
                .observed_at_ms(1)
                .runs(&[])
                .retain(false)
                .build(),
        )
        .unwrap();
        assert_eq!(db.spoke_ingress_frame("host-a", 2).unwrap().as_deref(), Some(&b""[..]));
        db.advance_spoke_projection("host-a", "journal", 1, &[]).unwrap();
        assert_eq!(
            db.spoke_ingress_frame("host-a", 2).unwrap().as_deref(),
            Some(&b""[..]),
            "a tombstone ahead of the cursor is kept so projection can skip it"
        );
        db.advance_spoke_projection("host-a", "journal", 2, &[]).unwrap();
        assert!(
            db.spoke_ingress_frame("host-a", 2).unwrap().is_none(),
            "a passed tombstone is deleted"
        );
        assert_eq!(
            db.spoke_ingress_frame("host-a", 1).unwrap().as_deref(),
            Some(&b"kept"[..]),
            "retained frames stay until a later consumer prunes them"
        );
        // A retransmitted unretained capture at a deleted tombstone is already
        // receipted and does not recreate the row.
        db.commit_spoke_record(
            SpokeIngressCommit::builder()
                .host_id("host-a")
                .generation(admission.generation)
                .journal_id(&admission.journal_id)
                .sequence(2)
                .frame(b"capture")
                .epoch(1)
                .features("execution=1")
                .observed_at_ms(1)
                .runs(&[])
                .retain(false)
                .build(),
        )
        .unwrap();
        assert!(db.spoke_ingress_frame("host-a", 2).unwrap().is_none());
    }

    fn sqlite_failure(code: i32) -> anyhow::Error {
        anyhow::Error::new(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None))
            .context("writing the domain row")
    }

    #[test]
    fn transient_database_faults_are_retried_and_domain_failures_are_refused() {
        use crate::{SpokeWriteError, retryable_work_error};
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_LOCKED,
            rusqlite::ffi::SQLITE_IOERR,
            rusqlite::ffi::SQLITE_FULL,
        ] {
            let error = sqlite_failure(code);
            assert!(retryable_work_error(&error), "code {code} is transient");
            assert!(matches!(SpokeWriteError::from_work(error), SpokeWriteError::Retry(_)));
        }
        // A missing table is a defect, a constraint is a refusal, and an
        // `ensure!` is the handler saying no: none of them heals by waiting.
        for error in [
            sqlite_failure(rusqlite::ffi::SQLITE_ERROR),
            sqlite_failure(rusqlite::ffi::SQLITE_CONSTRAINT),
            anyhow::anyhow!("comment c1 is not in_revision"),
        ] {
            assert!(!retryable_work_error(&error), "{error:#}");
            assert!(matches!(SpokeWriteError::from_work(error), SpokeWriteError::Refused(_)));
        }
    }

    #[test]
    fn a_halt_reason_and_turn_state_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.db");
        {
            let db = WorkDb::open(path.clone()).unwrap();
            receive(&db, "host-a", 1);
            db.set_spoke_projection_halt("host-a", "record 1 is received but not stored")
                .unwrap();
            db.upsert_spoke_turn_state(&SpokeTurnState {
                execution_id: "exec_1".into(),
                assignment_generation: 2,
                parked: true,
            })
            .unwrap();
        }
        let db = WorkDb::open(path).unwrap();
        let status = db.spoke_projection_status().unwrap();
        assert_eq!(
            status[0].halt_reason.as_deref(),
            Some("record 1 is received but not stored")
        );
        db.clear_spoke_projection_halt("host-a").unwrap();
        assert!(db.spoke_projection_status().unwrap()[0].halt_reason.is_none());
        let turns = db.spoke_turn_states().unwrap();
        assert_eq!(
            turns,
            [SpokeTurnState {
                execution_id: "exec_1".into(),
                assignment_generation: 2,
                parked: true,
            }]
        );
        db.forget_spoke_turn_state("exec_1").unwrap();
        assert!(db.spoke_turn_states().unwrap().is_empty());
    }
}
