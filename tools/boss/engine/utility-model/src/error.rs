//! Failure modes of resolving a utility call.

use crate::task::UtilityTask;

/// Why a [`crate::UtilityModel`] could not produce a [`crate::UtilityCall`].
///
/// Deliberately typed rather than `anyhow`: every call site needs to tell
/// "the engine has no credentials, so this feature can never work" apart from
/// "the API returned 429", and the live-status debug surface reports the two
/// differently.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UtilityModelError {
    /// No credential could be resolved for this task. `tried` names the env
    /// vars consulted, in precedence order, so the log line tells an operator
    /// exactly what to set.
    #[error("utility model {provider}: no credential for {task} (tried {})", tried.join(", "))]
    NoCredentials {
        task: UtilityTask,
        provider: String,
        tried: Vec<String>,
    },
}

impl UtilityModelError {
    /// The task whose resolution failed.
    pub fn task(&self) -> UtilityTask {
        match self {
            Self::NoCredentials { task, .. } => *task,
        }
    }

    /// Short stable tag for metrics and debug surfaces.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::NoCredentials { .. } => "no_credentials",
        }
    }
}
