//! Durable metadata for the singleton coordinator tmux session.
//!
//! Unlike worker tmux sessions, the coordinator has no execution/run row to
//! carry its durable identity. These metadata keys are therefore updated
//! together in one SQLite transaction before tmux receives `new-session`.

use anyhow::Result;
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::WorkDb;

const SESSION_NAME_KEY: &str = "coordinator.tmux_session_name";
const SPAWN_TOKEN_KEY: &str = "coordinator.tmux_spawn_token";
const SPAWN_STATE_KEY: &str = "coordinator.tmux_spawn_state";
const MODEL_KEY: &str = "coordinator.tmux_model";
const CLAUDE_VERSION_KEY: &str = "coordinator.tmux_claude_version";
/// Unix epoch seconds at which the spawn intent for the current record was
/// committed — i.e. when this coordinator session was started. Read back by
/// the session-handoff brief (`crate::coordinator_handoff`) so an incoming
/// session can be told when the session it replaces began, and whether that
/// session ever wrote a handoff after starting.
const SPAWNED_AT_KEY: &str = "coordinator.tmux_spawned_at";
/// Stable tmux pane id assigned when the current session was created. Unlike
/// a pane index, this remains useful after the session has disappeared.
const PANE_ID_KEY: &str = "coordinator.tmux_pane_id";
/// Epoch seconds of the last completed healthy liveness observation for the
/// current spawn token. Bound to that token so a new session cannot inherit
/// stale evidence about the one it replaced.
const LIVENESS_PASSED_AT_KEY: &str = "coordinator.tmux_liveness_passed_at";

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct CoordinatorTmuxRecord {
    pub(crate) session_name: String,
    pub(crate) spawn_token: String,
    pub(crate) spawn_state: String,
    pub(crate) model: String,
    /// The `claude --version` output this session actually launched with,
    /// probed once at creation time. `None` for a record predating this
    /// field, or when the probe failed — never a guess.
    pub(crate) launched_claude_version: Option<String>,
    /// When this session's spawn intent was committed (epoch seconds).
    /// `None` for a record predating this field — never a guess.
    pub(crate) spawned_at: Option<i64>,
    /// The last pane id observed for this spawn token. Missing only for a
    /// session created before pane identity became durable.
    pub(crate) pane_id: Option<String>,
    /// The last time the liveness checks passed for this spawn token.
    pub(crate) liveness_passed_at: Option<i64>,
}

impl WorkDb {
    pub(crate) fn coordinator_tmux_record(&self) -> Result<Option<CoordinatorTmuxRecord>> {
        let conn = self.connect()?;
        let value_for = |key: &str| {
            conn.query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()
        };
        let (Some(session_name), Some(spawn_token), Some(spawn_state)) = (
            value_for(SESSION_NAME_KEY)?,
            value_for(SPAWN_TOKEN_KEY)?,
            value_for(SPAWN_STATE_KEY)?,
        ) else {
            return Ok(None);
        };
        // A pre-release engine never wrote this key. Treat that record as the
        // currently configured model rather than destructively recreating a
        // still-live coordinator merely because the representation evolved.
        let model = value_for(MODEL_KEY)?.unwrap_or_default();
        // Same compat posture as `model`: a record predating this field (or
        // written when the version probe failed) has no opinion, not an
        // empty string — the update-available check must treat that as
        // "can't tell", never "no update".
        let launched_claude_version = value_for(CLAUDE_VERSION_KEY)?.filter(|v| !v.is_empty());
        // Same posture again: a record from before this key was written has
        // no start time, and an unparseable value is treated the same way
        // rather than surfacing a bogus epoch.
        let spawned_at = value_for(SPAWNED_AT_KEY)?.and_then(|v| v.trim().parse::<i64>().ok());
        let pane_id = value_for(PANE_ID_KEY)?.filter(|v| !v.is_empty());
        let liveness_passed_at = value_for(LIVENESS_PASSED_AT_KEY)?.and_then(|v| v.trim().parse::<i64>().ok());
        Ok(Some(CoordinatorTmuxRecord {
            session_name,
            spawn_token,
            spawn_state,
            model,
            launched_claude_version,
            spawned_at,
            pane_id,
            liveness_passed_at,
        }))
    }

    /// Persist the coordinator's complete `intended` record atomically before
    /// creating its tmux session. The order mirrors worker spawn exactly.
    /// `claude_version` is best-effort (the probe that produced it never
    /// fails session creation) and stored as an empty string when absent;
    /// [`Self::coordinator_tmux_record`] maps that back to `None`.
    pub(crate) fn record_coordinator_tmux_spawn_intent(
        &self,
        session_name: &str,
        spawn_token: &str,
        model: &str,
        claude_version: Option<&str>,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let spawned_at = boss_engine_utils::epoch_time::now_epoch_secs().to_string();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (key, value) in [
            (SESSION_NAME_KEY, session_name),
            (SPAWN_TOKEN_KEY, spawn_token),
            (SPAWN_STATE_KEY, "intended"),
            (MODEL_KEY, model),
            (CLAUDE_VERSION_KEY, claude_version.unwrap_or_default()),
            (SPAWNED_AT_KEY, spawned_at.as_str()),
            (PANE_ID_KEY, ""),
            (LIVENESS_PASSED_AT_KEY, ""),
        ] {
            tx.execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Advance an `intended` coordinator record only when its token still
    /// matches the session this engine created. A stale confirmation must not
    /// overwrite a newer recreate's identity.
    pub(crate) fn record_coordinator_tmux_session_created(&self, spawn_token: &str) -> Result<bool> {
        let conn = self.connect()?;
        let changed = conn.execute(
            "UPDATE metadata SET value = 'created'
             WHERE key = ?1
               AND EXISTS (
                   SELECT 1 FROM metadata
                   WHERE key = ?2 AND value = ?3
               )",
            params![SPAWN_STATE_KEY, SPAWN_TOKEN_KEY, spawn_token],
        )?;
        Ok(changed == 1)
    }

    /// Complete a newly-created coordinator record with its stable pane
    /// identity and the creation-time liveness observation. Keeping these in
    /// the same update means a later missing-session recreate can identify
    /// the vanished pane and say when it was last known healthy.
    pub(crate) fn record_coordinator_tmux_session_created_with_pane(
        &self,
        spawn_token: &str,
        pane_id: &str,
    ) -> Result<bool> {
        self.write_pane_observation(spawn_token, pane_id, true)
    }

    /// Persist a successful liveness observation only if it still belongs to
    /// the current coordinator. The caller supplies the pane id it just
    /// observed, so a later session-loss record retains the real old pane.
    pub(crate) fn record_coordinator_tmux_liveness_pass(&self, spawn_token: &str, pane_id: &str) -> Result<bool> {
        self.write_pane_observation(spawn_token, pane_id, false)
    }

    fn write_pane_observation(&self, spawn_token: &str, pane_id: &str, mark_created: bool) -> Result<bool> {
        let mut conn = self.connect()?;
        let now = boss_engine_utils::epoch_time::now_epoch_secs().to_string();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let matches_token = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key = ?1 AND value = ?2)",
            params![SPAWN_TOKEN_KEY, spawn_token],
            |row| row.get::<_, bool>(0),
        )?;
        if !matches_token {
            return Ok(false);
        }
        if mark_created {
            tx.execute(
                "UPDATE metadata SET value = 'created' WHERE key = ?1",
                params![SPAWN_STATE_KEY],
            )?;
        }
        for (key, value) in [(PANE_ID_KEY, pane_id), (LIVENESS_PASSED_AT_KEY, now.as_str())] {
            tx.execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Whether a live coordinator record predates the durable Claude-version
    /// baseline. An empty value is intentionally *not* missing: it records a
    /// previous probe failure and must retain the update check's fail-closed
    /// semantics.
    pub(crate) fn coordinator_tmux_claude_version_is_missing(&self) -> Result<bool> {
        let conn = self.connect()?;
        let exists = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key = ?1)",
            params![CLAUDE_VERSION_KEY],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(!exists)
    }

    /// Record an adopted session's current Claude version only if no prior
    /// engine ever persisted a baseline. The launch version is unknowable for
    /// an adopted pre-feature session, so callers seed from a current probe
    /// rather than fabricating historical evidence.
    pub(crate) fn seed_coordinator_tmux_claude_version_if_absent(&self, claude_version: Option<&str>) -> Result<bool> {
        let conn = self.connect()?;
        let inserted = conn.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO NOTHING",
            params![CLAUDE_VERSION_KEY, claude_version.unwrap_or_default()],
        )?;
        Ok(inserted == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn coordinator_intent_is_atomic_and_confirmation_is_token_bound() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        db.record_coordinator_tmux_spawn_intent("boss-coordinator", "token-a", "opus", Some("2.1.0"))
            .unwrap();
        let mut record = db.coordinator_tmux_record().unwrap().unwrap();
        let spawned_at = record.spawned_at.take().expect("spawn intent must stamp a start time");
        assert!(spawned_at > 0, "start time must be a real epoch, got {spawned_at}");
        assert_eq!(
            record,
            CoordinatorTmuxRecord {
                session_name: "boss-coordinator".to_owned(),
                spawn_token: "token-a".to_owned(),
                spawn_state: "intended".to_owned(),
                model: "opus".to_owned(),
                launched_claude_version: Some("2.1.0".to_owned()),
                spawned_at: None,
                pane_id: None,
                liveness_passed_at: None,
            }
        );
        assert!(!db.record_coordinator_tmux_session_created("token-b").unwrap());
        assert!(db.record_coordinator_tmux_session_created("token-a").unwrap());
        assert_eq!(db.coordinator_tmux_record().unwrap().unwrap().spawn_state, "created");
    }

    #[test]
    fn absent_claude_version_round_trips_as_none_not_empty_string() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        db.record_coordinator_tmux_spawn_intent("boss-coordinator", "token-a", "opus", None)
            .unwrap();
        assert_eq!(
            db.coordinator_tmux_record().unwrap().unwrap().launched_claude_version,
            None,
            "a failed/skipped version probe must read back as None, never Some(\"\")"
        );
    }

    #[test]
    fn pane_observations_are_token_bound_and_round_trip() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        db.record_coordinator_tmux_spawn_intent("boss-coordinator", "token-a", "opus", None)
            .unwrap();

        assert!(!db.record_coordinator_tmux_liveness_pass("token-b", "%stale").unwrap());
        assert!(
            !db.record_coordinator_tmux_session_created_with_pane("token-b", "%stale")
                .unwrap()
        );
        assert!(db.record_coordinator_tmux_liveness_pass("token-a", "%live").unwrap());
        let record = db.coordinator_tmux_record().unwrap().unwrap();
        assert_eq!(record.pane_id.as_deref(), Some("%live"));
        assert!(record.liveness_passed_at.is_some());
        assert_eq!(record.spawn_state, "intended");

        assert!(
            db.record_coordinator_tmux_session_created_with_pane("token-a", "%created")
                .unwrap()
        );
        let record = db.coordinator_tmux_record().unwrap().unwrap();
        assert_eq!(record.pane_id.as_deref(), Some("%created"));
        assert!(record.liveness_passed_at.is_some());
        assert_eq!(record.spawn_state, "created");
    }
}
