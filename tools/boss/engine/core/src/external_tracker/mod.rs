//! External issue tracker: reconciliation policy, plus a re-export of the
//! transport layer.
//!
//! The tracker vocabulary (`ExternalTracker`, `UpstreamItem`, `TrackerError`,
//! …) and the GitHub transport (REST client, device-flow OAuth, credential
//! resolution) live in the lower-level `boss_github_tracker` crate, so that
//! touching them no longer rebuilds and retests the whole engine. They are
//! re-exported here under the long-standing `crate::external_tracker::*`
//! paths the engine already uses.
//!
//! What stays here is [`reconcile`] — reconciliation *policy*, which is
//! coupled to `WorkDb`, `TaskStatus`, and content checksums and is a poor
//! fit for a transport crate. The dependency edge is one-directional:
//! `boss_engine` -> `boss_github_tracker`, never the reverse.

pub mod reconcile;

mod org_state_sink;

#[cfg(test)]
mod github_oauth_org_probe_tests;

pub use boss_github_tracker::{
    CloseReason, ClosedReason, EchoTracker, ExternalTracker, RegistryError, Result, TrackerConfigError, TrackerContext,
    TrackerCredential, TrackerError, TrackerRegistry, UpstreamItem, UpstreamPrAssociation, UpstreamRef, UpstreamStatus,
    credentials, github, github_oauth,
};
pub use org_state_sink::WorkDbOrgStateSink;
