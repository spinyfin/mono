//! Token-verified pane probe shared by the hub engine and the spoke daemon.

use crate::{DisplayField, Tmux};

/// What one token-verified look at a worker session found.
#[derive(Debug)]
pub enum PaneProbe {
    /// No session, or no spawn token in it.
    Absent,
    /// The session exists but carries a different spawn token.
    ForeignToken,
    /// The token matches and the pane is running.
    Alive,
    /// The token matches and the pane has exited. `status` is `None` when
    /// tmux reported no exit status (the process was ended by a signal).
    Dead { status: Option<String> },
    /// A tmux query failed, so identity or liveness could not be established.
    Unreadable { stage: &'static str, error: anyhow::Error },
}

/// Read the session's spawn token, then `#{pane_dead}`, then
/// `#{pane_dead_status}` when dead. Identity is established by the token
/// alone; any failed query is [`PaneProbe::Unreadable`], never a guess.
pub async fn probe_pane(tmux: &Tmux, session: &str, expected_token: &str) -> PaneProbe {
    match tmux.show_environment(session, crate::TMUX_SPAWN_TOKEN_ENV).await {
        Ok(Some(token)) if token == expected_token => {}
        Ok(Some(_)) => return PaneProbe::ForeignToken,
        Ok(None) => return PaneProbe::Absent,
        Err(error) => {
            return PaneProbe::Unreadable {
                stage: "spawn token",
                error,
            };
        }
    }
    match tmux.display_message(session, DisplayField::PaneDead).await {
        Ok(value) if value == "0" => return PaneProbe::Alive,
        Ok(value) if value == "1" => {}
        Ok(value) => {
            return PaneProbe::Unreadable {
                stage: "pane_dead",
                error: anyhow::anyhow!("unexpected #{{pane_dead}} value {value:?}"),
            };
        }
        Err(error) => {
            return PaneProbe::Unreadable {
                stage: "pane_dead",
                error,
            };
        }
    }
    match tmux.display_message(session, DisplayField::PaneDeadStatus).await {
        Ok(status) => PaneProbe::Dead {
            status: (!status.is_empty()).then_some(status),
        },
        Err(error) => PaneProbe::Unreadable {
            stage: "pane_dead_status",
            error,
        },
    }
}
