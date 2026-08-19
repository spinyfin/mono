//! Per-driver provider quota probes and their cache.
//!
//! Boss drives three coding-agent CLIs, and each one's provider meters the
//! maintainer's subscription independently. Getting all three figures used to
//! mean starting each driver interactively and typing its slash command. This
//! crate obtains the same figures out of band and caches them, so the engine
//! can serve one comparable snapshot over RPC.
//!
//! # Mechanism per driver — three CLIs, three different answers
//!
//! | driver | mechanism | why |
//! |--------|-----------|-----|
//! | `claude` | `claude -p "/usage" --output-format json` | `/usage` is a **local** slash command: in print mode Claude Code executes it without a model turn (`num_turns: 0`, `total_cost_usd: 0`) and prints the provider's report in the JSON envelope's `result`. No credential ever passes through Boss — the CLI reads its own keychain entry. |
//! | `codex` | `codex app-server` stdio JSON-RPC → `account/rateLimits/read` | Codex exposes no `usage`/`status` subcommand, but its app-server protocol has a first-class, machine-readable method for exactly this. Two JSON-RPC lines, no thread started, no model turn. Again no credential passes through Boss: the child reads `$CODEX_HOME/auth.json` itself. |
//! | `grok` | HTTPS GET of the CLI's own billing/credits endpoint | Grok's `/usage` is an in-TUI extension with no CLI equivalent, and headless `grok -p "/usage"` treats the text as a *prompt to the model* — it would burn tokens and answer with prose. The endpoint the extension itself calls is the only non-interactive route, so this is the one driver where Boss handles a bearer token. It is read from the same `auth.json` the driver reads, held in memory for one request, and never logged, echoed, or persisted. |
//!
//! # What this must never become
//!
//! Nothing here estimates. If a probe fails, the driver's entry is a typed
//! [`boss_protocol::DriverQuotaOutcome::Unavailable`] — never a blank, never a
//! zero, never last week's figure passed off as current. And no figure here
//! is ever derived from Boss's own token accounting: that measures a
//! different thing (only work Boss dispatched) and presenting it as the
//! provider's view would mislead precisely when headroom matters.
//!
//! # Out-of-band by construction
//!
//! A probe is a short-lived child process (or one HTTPS request) owned by
//! this crate. It never allocates a worker slot, never creates an execution
//! row, never touches the pane/tmux layer, and never asks the dispatcher for
//! anything — see [`cache::QuotaCache`] and the crate's tests. The cache is
//! lazy: no probe runs until something asks for a snapshot, so engine startup
//! is untouched.

pub mod cache;
pub mod parse;
pub mod probes;

pub use cache::{DEFAULT_MIN_REFRESH_INTERVAL, DEFAULT_TTL, QuotaCache, QuotaProbeSet};

// ---------------------------------------------------------------------------
// The probe seam
// ---------------------------------------------------------------------------

use std::time::Duration;

use async_trait::async_trait;
use boss_protocol::DriverQuotaOutcome;

/// Per-driver deadline. Generous enough for a cold CLI start on a loaded
/// laptop, short enough that three probes in parallel cannot make the
/// Preferences pane feel hung. A probe that exceeds it is reported as
/// [`boss_protocol::DriverQuotaFailureKind::Timeout`], never left pending.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(25);

/// Reads one driver's provider-reported quota.
///
/// Implementations must return a [`DriverQuotaOutcome`] for every input,
/// including failure — a probe never returns an error type, because "we
/// could not tell" is itself a value the UI has to render.
#[async_trait]
pub trait DriverQuotaProbe: Send + Sync {
    /// Driver slug this probe answers for.
    fn driver(&self) -> &'static str;

    /// Run the probe. Must not log, return, or otherwise expose any
    /// credential, token, API key, or session identifier it encounters.
    async fn probe(&self) -> DriverQuotaOutcome;
}

/// Current wall-clock time in epoch seconds. Saturates rather than panics on
/// a clock set before 1970.
pub(crate) fn now_epoch_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build the production probe set: one probe per implemented driver.
///
/// `grok_auth_path` must come from the driver's own resolution
/// (`boss_engine_driver::grok::resolve_grok_auth_source`) so the probe reads
/// the identical `auth.json` the Grok driver reads — this crate deliberately
/// does not re-derive that path, because two independent derivations of a
/// credential location is exactly how they drift apart.
pub fn default_probes(grok_auth_path: std::path::PathBuf) -> QuotaProbeSet {
    vec![
        std::sync::Arc::new(probes::claude::ClaudeQuotaProbe::default()),
        std::sync::Arc::new(probes::codex::CodexQuotaProbe::default()),
        std::sync::Arc::new(probes::grok::GrokQuotaProbe::new(grok_auth_path)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::DRIVER_QUOTA_ORDER;

    #[test]
    fn production_probe_set_covers_every_driver_the_snapshot_promises() {
        let probes = default_probes(std::path::PathBuf::from("/tmp/auth.json"));
        let slugs: Vec<&str> = probes.iter().map(|p| p.driver()).collect();
        assert_eq!(slugs, DRIVER_QUOTA_ORDER.to_vec());
    }
}
