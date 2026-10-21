//! Validation of the provenance a client sends with a cancel RPC.
//!
//! `CancelExecution` and `StopRun` carry `source`, `reason` and an optional
//! `actor` on the wire. They are `Option` there only so that a request
//! missing one reaches this module and gets a typed, specific refusal rather
//! than an opaque parse failure. There is no default for any of them: a
//! cancel that cannot say who sent it and why does not happen.

use boss_protocol::{CancelProvenance, CancelProvenanceError, CancelSource};

/// Why a cancel RPC was refused for missing or invalid provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CancelRpcRefusal {
    MissingSource,
    EngineSubsystemSource(CancelSource),
    MissingReason,
    MissingActor,
}

impl CancelRpcRefusal {
    /// The message returned to the client. Names the RPC and the exact field
    /// so a caller can fix the request without reading engine code.
    pub(super) fn message(&self, rpc: &str) -> String {
        match self {
            Self::MissingSource => format!(
                "{rpc} refused: no `source`. Every cancel must say which client surface sent it \
                 (one of app, boss_cli, bossctl); the engine does not infer or default it."
            ),
            Self::EngineSubsystemSource(source) => format!(
                "{rpc} refused: `source` `{source}` names an engine subsystem. Clients may only \
                 claim a client surface (app, boss_cli, bossctl)."
            ),
            Self::MissingReason => format!(
                "{rpc} refused: no `reason`. Every cancel must say why; the engine does not \
                 substitute a generic reason."
            ),
            Self::MissingActor => format!(
                "{rpc} refused: no `actor` was supplied and the engine could not observe the \
                 socket peer, so there is no identity to record."
            ),
        }
    }
}

/// Build the provenance for a cancel RPC, or say precisely what was missing.
///
/// `actor` is the client-supplied identity. When the client did not send one
/// the engine records the socket peer it observed (`peer pid N`), which is a
/// fact rather than a placeholder; with neither, the request is refused.
pub(super) fn provenance_from_request(
    source: Option<CancelSource>,
    reason: Option<&str>,
    actor: Option<&str>,
    peer_pid: Option<libc::pid_t>,
) -> Result<CancelProvenance, CancelRpcRefusal> {
    let source = source.ok_or(CancelRpcRefusal::MissingSource)?;
    if !source.is_client_surface() {
        return Err(CancelRpcRefusal::EngineSubsystemSource(source));
    }
    let actor = match actor.map(str::trim).filter(|actor| !actor.is_empty()) {
        Some(actor) => actor.to_owned(),
        None => match peer_pid {
            Some(pid) => format!("peer pid {pid} (engine-observed)"),
            None => return Err(CancelRpcRefusal::MissingActor),
        },
    };
    CancelProvenance::new(source, actor, reason.unwrap_or_default()).map_err(|err| match err {
        CancelProvenanceError::BlankReason => CancelRpcRefusal::MissingReason,
        CancelProvenanceError::BlankActor => CancelRpcRefusal::MissingActor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_request_yields_provenance() {
        let provenance = provenance_from_request(
            Some(CancelSource::Bossctl),
            Some("wrong repo"),
            Some("operator"),
            Some(7),
        )
        .unwrap();
        assert_eq!(provenance.source, CancelSource::Bossctl);
        assert_eq!(provenance.actor, "operator");
        assert_eq!(provenance.reason, "wrong repo");
    }

    #[test]
    fn missing_source_is_refused() {
        assert_eq!(
            provenance_from_request(None, Some("why"), Some("me"), Some(7)),
            Err(CancelRpcRefusal::MissingSource)
        );
    }

    #[test]
    fn engine_subsystem_source_is_refused() {
        assert_eq!(
            provenance_from_request(
                Some(CancelSource::MergePollerParentMerged),
                Some("why"),
                Some("me"),
                Some(7)
            ),
            Err(CancelRpcRefusal::EngineSubsystemSource(
                CancelSource::MergePollerParentMerged
            ))
        );
    }

    #[test]
    fn missing_or_blank_reason_is_refused() {
        for reason in [None, Some(""), Some("   ")] {
            assert_eq!(
                provenance_from_request(Some(CancelSource::App), reason, Some("me"), Some(7)),
                Err(CancelRpcRefusal::MissingReason)
            );
        }
    }

    #[test]
    fn absent_actor_falls_back_to_the_observed_peer_only() {
        let observed = provenance_from_request(Some(CancelSource::BossCli), Some("why"), None, Some(42)).unwrap();
        assert_eq!(observed.actor, "peer pid 42 (engine-observed)");
        assert_eq!(
            provenance_from_request(Some(CancelSource::BossCli), Some("why"), Some("  "), None),
            Err(CancelRpcRefusal::MissingActor)
        );
    }

    #[test]
    fn refusal_messages_name_the_rpc_and_field() {
        assert!(
            CancelRpcRefusal::MissingSource
                .message("cancel_execution")
                .contains("no `source`")
        );
        assert!(
            CancelRpcRefusal::MissingReason
                .message("stop_run")
                .contains("stop_run refused")
        );
    }
}
