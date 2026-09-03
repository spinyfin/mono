//! Shared, deterministic primitives for repository release workflows.
//!
//! The deterministic resolver is kept separate from command orchestration so
//! release decisions can be tested with captured command output rather than a
//! network connection or a checkout with remote credentials.

pub mod commands;

mod config;
mod decision;
mod release_state;
mod runner;
mod version;

pub use config::{NotesConfig, NotesSource, ReleaseConfig, VersionConfig, VersionScheme};
pub use decision::{SkipDecision, SkipInput, SkipReason, Trigger, assert_allowed_trigger, should_skip};
pub use release_state::{GitHubRelease, LastReleases, ReleaseState, query_release_state, resolve_last_release};
pub use runner::{Command, CommandOutput, CommandRunner, ProcessCommandRunner, RunnerError};
pub use version::{NextVersion, compute_next_version};

use thiserror::Error;

/// Errors returned by config parsing, version resolution, and release queries.
#[derive(Debug, Error)]
pub enum ReleaseError {
    #[error("invalid release configuration: {0}")]
    InvalidConfig(String),

    #[error("invalid TOML: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("invalid GitHub releases response: {0}")]
    Json(#[from] serde_json::Error),

    #[error("failed to list releases after {attempts} attempts: {last_error}")]
    ReleaseListUnavailable { attempts: usize, last_error: String },

    #[error("failed to query remote tags: {0}")]
    RemoteTagsUnavailable(String),

    #[error(
        "release API snapshot is incomplete: it reports {api_tag:?} as the highest matching release, but git ls-remote reports {remote_tag}"
    )]
    ReleaseListUnderreported {
        api_tag: Option<String>,
        remote_tag: String,
    },

    #[error("remote tag {tag} has no matching GitHub release")]
    LeakedRemoteTag { tag: String },

    #[error("could not determine whether remote tag {tag} has a GitHub release: {detail}")]
    ReleaseTagLookupUnavailable { tag: String, detail: String },

    #[error("unsupported trigger source {trigger_source:?}; expected schedule, ui, or api")]
    UnsupportedTrigger { trigger_source: String },
}

/// Convenient result alias for this crate.
pub type Result<T> = std::result::Result<T, ReleaseError>;
