//! External issue tracker integration.
//!
//! The `ExternalTracker` trait, its vocabulary types, the GitHub Projects
//! backend, and credential resolution live in the `boss-external-tracker`
//! crate and are re-exported here, so `crate::external_tracker::…` paths
//! resolve the same as before the split.
//!
//! What remains in this crate is the half that needs the engine's DB and
//! metrics layers, and so cannot move down:
//!
//! - [`reconcile`] — drives the trait against [`crate::work::WorkDb`].
//! - [`github_oauth`] — the OAuth device flow, which records org-auth state
//!   in the work DB.
//!
//! The edge is one-directional: `boss-engine` -> `boss-external-tracker`.

pub mod github_oauth;
pub mod reconcile;

pub use boss_external_tracker::*;
