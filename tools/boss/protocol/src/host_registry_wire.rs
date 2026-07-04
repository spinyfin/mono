use serde::{Deserialize, Serialize};

/// One registered host plus all its current capabilities.
/// Wire type for [`FrontendEvent::HostsList`], [`FrontendEvent::HostResult`],
/// and [`FrontendEvent::HostUpdated`].
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[builder(on(String, into))]
pub struct HostSnapshot {
    /// Short identifier. `"local"` is the built-in host; remote hosts
    /// use whatever name was given to `bossctl hosts add` / `AddHost`.
    pub id: String,
    /// SSH target string (e.g. `user@hostname` or an SSH alias).
    /// `None` for the `local` host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_target: Option<String>,
    /// Maximum concurrent worker slots on this host.
    pub pool_size: i64,
    /// Whether the host will accept new work dispatches.
    pub enabled: bool,
    /// Epoch-seconds timestamp of the most recent contact *attempt*
    /// with this host — success or failure, registration push or
    /// dispatch-time cube invocation. `None` when the host has never
    /// been contacted (newly registered).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    /// Human-readable description of the last error, when the host is
    /// in a degraded state (e.g. wrapper push failed at registration,
    /// or a dispatch-time cube invocation failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_text: Option<String>,
    /// Consecutive dispatch-time cube invocation failures on this host.
    /// Resets to 0 on any success; auto-disables the host at
    /// `HOST_HEALTH_FAILURE_THRESHOLD`.
    #[serde(default)]
    pub consecutive_failures: i64,
    /// ISO-8601 timestamp of host registration.
    pub created_at: String,
    /// All capabilities on this host (both auto-discovered and
    /// user-tagged), ordered source-then-name.
    pub capabilities: Vec<HostCapabilitySnapshot>,
}

/// One capability on a host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostCapabilitySnapshot {
    pub capability: String,
    /// `"auto"` (engine-discovered) or `"user"` (manually tagged).
    pub source: String,
}
