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
