//! Durable metadata for the singleton coordinator tmux session.
//!
//! Unlike worker tmux sessions, the coordinator has no execution/run row to
//! carry its durable identity. These four metadata keys are therefore updated
//! together in one SQLite transaction before tmux receives `new-session`.

use anyhow::Result;
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::WorkDb;

const SESSION_NAME_KEY: &str = "coordinator.tmux_session_name";
const SPAWN_TOKEN_KEY: &str = "coordinator.tmux_spawn_token";
const SPAWN_STATE_KEY: &str = "coordinator.tmux_spawn_state";
const MODEL_KEY: &str = "coordinator.tmux_model";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoordinatorTmuxRecord {
    pub(crate) session_name: String,
    pub(crate) spawn_token: String,
    pub(crate) spawn_state: String,
    pub(crate) model: String,
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
        Ok(Some(CoordinatorTmuxRecord {
            session_name,
            spawn_token,
            spawn_state,
            model,
        }))
    }

    /// Persist the coordinator's complete `intended` record atomically before
    /// creating its tmux session. The order mirrors worker spawn exactly.
    pub(crate) fn record_coordinator_tmux_spawn_intent(
        &self,
        session_name: &str,
        spawn_token: &str,
        model: &str,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (key, value) in [
            (SESSION_NAME_KEY, session_name),
            (SPAWN_TOKEN_KEY, spawn_token),
            (SPAWN_STATE_KEY, "intended"),
            (MODEL_KEY, model),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn coordinator_intent_is_atomic_and_confirmation_is_token_bound() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        db.record_coordinator_tmux_spawn_intent("boss-coordinator", "token-a", "opus")
            .unwrap();
        assert_eq!(
            db.coordinator_tmux_record().unwrap(),
            Some(CoordinatorTmuxRecord {
                session_name: "boss-coordinator".to_owned(),
                spawn_token: "token-a".to_owned(),
                spawn_state: "intended".to_owned(),
                model: "opus".to_owned(),
            })
        );
        assert!(!db.record_coordinator_tmux_session_created("token-b").unwrap());
        assert!(db.record_coordinator_tmux_session_created("token-a").unwrap());
        assert_eq!(db.coordinator_tmux_record().unwrap().unwrap().spawn_state, "created");
    }
}
