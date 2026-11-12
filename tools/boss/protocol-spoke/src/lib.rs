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
//! - [`identity`]: the `urn:boss:host:<id>` certificate identity.
//!
//! The crate depends on `boss-protocol` for the shared event types and on
//! nothing in the engine; transport, journal and policy live in later crates.

pub mod identity;
pub mod limits;
pub mod negotiate;
pub mod validate;
pub mod worker_event;

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
