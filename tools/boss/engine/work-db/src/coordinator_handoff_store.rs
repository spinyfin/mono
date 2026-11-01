//! Coordinator handoff metadata stored on WorkDb.
use anyhow::{Context, Result};
use boss_engine_storage_values::{CoordinatorHandoff, HandoffState};

use super::*;

/// Metadata key holding the JSON-encoded [`CoordinatorHandoff`].
pub const HANDOFF_METADATA_KEY: &str = "coordinator.handoff";

#[work_db_boundary::instrument]
impl WorkDb {
    /// Read the stored handoff. Never errors: every failure mode is a
    /// [`HandoffState`] variant so callers cannot accidentally treat a
    /// read failure as "nothing stored".
    pub fn coordinator_handoff_state(&self) -> HandoffState {
        match self.get_metadata(HANDOFF_METADATA_KEY) {
            Ok(None) => HandoffState::Missing,
            Ok(Some(raw)) => match serde_json::from_str::<CoordinatorHandoff>(&raw) {
                Ok(handoff) => HandoffState::Present(handoff),
                Err(error) => {
                    HandoffState::Unreadable(format!("stored coordinator handoff is not valid JSON: {error}"))
                }
            },
            Err(error) => HandoffState::Unreadable(format!("could not read stored coordinator handoff: {error:#}")),
        }
    }

    /// Replace the stored handoff. `body` must already have passed
    /// validation in the caller.
    pub fn set_coordinator_handoff(
        &self,
        body: &str,
        writer_spawn_token: &str,
        now_epoch_secs: i64,
    ) -> Result<CoordinatorHandoff> {
        let handoff = CoordinatorHandoff {
            write_id: (self.token_generator)(),
            body: body.to_owned(),
            written_at: now_epoch_secs,
            writer_spawn_token: writer_spawn_token.to_owned(),
        };
        let raw = serde_json::to_string(&handoff).context("encoding coordinator handoff")?;
        self.set_metadata(HANDOFF_METADATA_KEY, &raw)
            .context("persisting coordinator handoff")?;
        Ok(handoff)
    }
}
