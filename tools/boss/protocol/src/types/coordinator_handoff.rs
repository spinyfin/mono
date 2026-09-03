//! Coordinator session handoff: the small, operator-facing summary the
//! outgoing coordinator session writes so the incoming session does not
//! start from zero after a restart (Claude Code update, app restart,
//! crash, restart ceiling).

use serde::{Deserialize, Serialize};

/// The stored coordinator handoff as the engine will hand it to a
/// `boss handoff show` caller. Coordinator-private state: it lives in the
/// engine's own database, never in a repo.
///
/// `written_by_current_session` is the staleness signal a reader needs
/// most: `true` means the live coordinator session (the one whose spawn
/// token the engine currently records) wrote this handoff itself, so it
/// reflects that session's own knowledge; `false` means it was written by
/// an earlier session and everything in it predates the current one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CoordinatorHandoffView {
    /// Seconds between `written_at` and the moment the engine answered.
    pub age_secs: i64,
    /// The handoff text, verbatim as written.
    pub body: String,
    /// Spawn token of the coordinator session that wrote this handoff.
    pub writer_spawn_token: String,
    /// Unix epoch seconds at which the handoff was written.
    pub written_at: i64,
    /// `written_at` rendered as an ISO-8601 UTC timestamp, for humans.
    pub written_at_iso8601: String,
    /// Whether the session whose spawn token the engine currently records
    /// is the writer. See the type-level doc.
    pub written_by_current_session: bool,
}
