//! Wire types and contract helpers for the `boss.spoke.v1` hub/spoke protocol.
//!
//! The schema lives in `proto/boss/spoke/v1/spoke.proto`; Bazel generates the
//! prost message types and tonic `SpokeHub` / `Enrollment` services from it
//! (see `README.md` for the toolchain and the evolution rules). This crate
//! re-exports the generated items as [`v1`] and adds the small, dependency-light
//! logic that both ends of the stream must agree on:
//!
//! - [`limits`]: default frame and chunk bounds, negotiated downward only.
//! - [`validate`]: structural validation of decoded messages. A decoded message
//!   is not trusted until it validates: scalar zero values are not usable
//!   identities and an unset oneof is never success.
//! - [`negotiate`]: wire-epoch and feature-revision selection for `Hello`, and
//!   the typed `Reject` a hub answers with when there is no common contract.
//! - [`worker_event`]: lossless mapping between the wire `WorkerEvent` and
//!   [`boss_protocol::WorkerEvent`].
//! - [`proposal`]: the `proposal-json/1` envelope, run-write contracts, and
//!   the wire-kind mapping both ends of a queued proposal must share.
//! - [`enrollment_token`]: the enrollment token text form and the manual job id.
//! - [`identity`]: the `urn:boss:host:<id>` certificate identity.
//!
//! The crate depends on `boss-protocol` for the shared event types and on
//! nothing in the engine; transport, journal and policy live in later crates.

pub mod enrollment_token;
pub mod identity;
pub mod limits;
pub mod negotiate;
pub mod validate;
pub mod worker_event;

/// Shared proposal wire mapping used by both the spoke gateway and the hub
/// projector.
///
/// The spoke queues a proposal as a `v1::Proposal` whose `canonical_json` is
/// either a [`proposal::ProposalJsonEnvelope`] under [`proposal::PROPOSAL_JSON`],
/// or the run-scoped write body under one of the run-write contracts. Both ends
/// must agree on the contract names and on [`proposal::wire_kind`]: a drift
/// refuses every queued proposal of the affected kind with a final decision.
pub mod proposal {
    use boss_protocol::ProposalKind;
    use serde::{Deserialize, Serialize};

    use crate::v1;

    /// Payload contract for a validated `propose` call: kind, payload and
    /// idempotency key in one JSON object.
    pub const PROPOSAL_JSON: &str = "proposal-json/1";
    /// `boss comment guide-outcome`.
    pub const GUIDE_OUTCOME: &str = "guide-outcome/1";
    /// `boss comment reply`.
    pub const ANSWER_REPLY: &str = "answer-reply/1";
    /// `boss engine conflicts record-producer`.
    pub const CONFLICT_PRODUCER: &str = "conflict-producer/1";
    /// `boss engine conflicts mark-failed`.
    pub const CONFLICT_MARK_FAILED: &str = "conflict-mark-failed/1";
    /// `boss engine ci mark-failed`.
    pub const CI_MARK_FAILED: &str = "ci-mark-failed/1";

    /// The envelope the spoke gateway queues under [`PROPOSAL_JSON`].
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ProposalJsonEnvelope {
        pub kind: ProposalKind,
        pub payload: serde_json::Value,
        pub idempotency_key: String,
    }

    /// The wire kind the spoke gateway stamps for a payload kind. Both ends must
    /// agree, so a record whose two disagree is refused.
    pub fn wire_kind(kind: ProposalKind) -> v1::ProposalKind {
        match kind {
            ProposalKind::PrCreated => v1::ProposalKind::PrCreated,
            ProposalKind::RunDone => v1::ProposalKind::RunDone,
            ProposalKind::Wait => v1::ProposalKind::Wait,
            ProposalKind::ReviewReport => v1::ProposalKind::ReviewReport,
            ProposalKind::ReviewVerdict => v1::ProposalKind::ReviewVerdict,
            _ => v1::ProposalKind::OtherValidated,
        }
    }

    /// Contracts the gateway queues as run-scoped writes rather than as
    /// [`PROPOSAL_JSON`].
    pub fn is_run_write_contract(contract: &str) -> bool {
        matches!(
            contract,
            GUIDE_OUTCOME | ANSWER_REPLY | CONFLICT_PRODUCER | CONFLICT_MARK_FAILED | CI_MARK_FAILED
        )
    }

    /// Canonical JSON the spoke gateway queues for `boss comment reply`.
    pub fn answer_reply_body(run_id: impl Into<String>, body: impl Into<String>) -> serde_json::Value {
        serde_json::json!({"run_id": run_id.into(), "body": body.into()})
    }

    /// Canonical JSON the spoke gateway queues for `boss comment guide-outcome`.
    pub fn guide_outcome_body(
        run_id: impl Into<String>,
        comment_id: impl Into<String>,
        disposition: &boss_protocol::GuideCommentDisposition,
        body: impl Into<String>,
        request_regeneration: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "run_id": run_id.into(),
            "comment_id": comment_id.into(),
            "disposition": disposition,
            "body": body.into(),
            "request_regeneration": request_regeneration,
        })
    }

    /// Canonical JSON the spoke gateway queues for `boss engine ci mark-failed`;
    /// `extra` carries `queue_ci_evidence`.
    pub fn ci_mark_failed_body(
        run_id: impl Into<String>,
        attempt_id: impl Into<String>,
        reason: impl Into<String>,
        extra: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "run_id": run_id.into(),
            "attempt_id": attempt_id.into(),
            "reason": reason.into(),
            "extra": extra,
        })
    }

    /// Canonical JSON the spoke gateway queues for `boss engine conflicts mark-failed`;
    /// `extra` carries `evidence`.
    pub fn conflict_mark_failed_body(
        run_id: impl Into<String>,
        attempt_id: impl Into<String>,
        reason: impl Into<String>,
        extra: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "run_id": run_id.into(),
            "attempt_id": attempt_id.into(),
            "reason": reason.into(),
            "extra": extra,
        })
    }

    /// Canonical JSON the spoke gateway queues for producer-side conflict telemetry.
    pub fn conflict_producer_body(
        execution_id: impl Into<String>,
        head_branch: impl Into<String>,
        base_branch: impl Into<String>,
        conflicted_files: Vec<String>,
    ) -> serde_json::Value {
        serde_json::json!({
            "execution_id": execution_id.into(),
            "head_branch": head_branch.into(),
            "base_branch": base_branch.into(),
            "conflicted_files": conflicted_files,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn proposal_json_envelope_round_trips_and_wire_kind_matches() {
            let envelope = ProposalJsonEnvelope {
                kind: ProposalKind::RunDone,
                payload: json!({"outcome": "delivered", "summary": "done"}),
                idempotency_key: "auto:run_done:abc".into(),
            };
            let bytes = serde_json::to_vec(&envelope).unwrap();
            let back: ProposalJsonEnvelope = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(back, envelope);
            assert_eq!(wire_kind(ProposalKind::RunDone), v1::ProposalKind::RunDone);
            assert_eq!(wire_kind(ProposalKind::Blocked), v1::ProposalKind::OtherValidated);
            assert!(is_run_write_contract(GUIDE_OUTCOME));
            assert!(!is_run_write_contract(PROPOSAL_JSON));
        }
    }
}

/// Generated `boss.spoke.v1` messages, enums and tonic services.
pub mod v1 {
    pub use spoke_v1_proto::boss::spoke::v1::*;
}

pub use validate::{Validate, ValidationError};

/// Whether a spawn reply requires a fresh observation before finalizing launch.
pub fn spawn_result_is_uncertain(result: &v1::CommandResult) -> bool {
    use v1::{PaneLife, ResultCode, command_result};
    match ResultCode::try_from(result.code) {
        Ok(ResultCode::Rejected | ResultCode::Absent) => false,
        Ok(ResultCode::Ok) => !matches!(&result.value,
            Some(command_result::Value::Pane(pane)) if matches!(
                PaneLife::try_from(pane.life),
                Ok(PaneLife::Alive | PaneLife::Exited | PaneLife::BriefNotDelivered))),
        _ => true,
    }
}

#[cfg(test)]
mod spawn_result_tests {
    use super::*;
    use v1::{CommandResult, PaneLife, PaneState, ResultCode, command_result};
    #[test]
    fn uncertain_spawn_classification_covers_codes_and_panes() {
        for code in [ResultCode::Unknown as i32, ResultCode::OutcomeUncertain as i32, 999] {
            assert!(spawn_result_is_uncertain(&CommandResult {
                code,
                ..Default::default()
            }));
        }
        for code in [ResultCode::Rejected, ResultCode::Absent] {
            assert!(!spawn_result_is_uncertain(&CommandResult {
                code: code as i32,
                ..Default::default()
            }));
        }
        for life in [
            PaneLife::Alive,
            PaneLife::Exited,
            PaneLife::BriefNotDelivered,
            PaneLife::Missing,
            PaneLife::Indeterminate,
            PaneLife::PaneUnspecified,
        ] {
            let result = CommandResult {
                code: ResultCode::Ok as i32,
                value: Some(command_result::Value::Pane(PaneState {
                    life: life as i32,
                    ..Default::default()
                })),
                ..Default::default()
            };
            assert_eq!(
                spawn_result_is_uncertain(&result),
                !matches!(life, PaneLife::Alive | PaneLife::Exited | PaneLife::BriefNotDelivered)
            );
        }
        assert!(spawn_result_is_uncertain(&CommandResult {
            code: ResultCode::Ok as i32,
            ..Default::default()
        }));
    }
}
