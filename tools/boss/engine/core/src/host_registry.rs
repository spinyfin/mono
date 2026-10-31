//! Host services for the registry: local capability discovery.
//!
//! The registry's storage (`hosts`, `host_capabilities`, the `local` host
//! seed) lives with `WorkDb` in [`crate::work`]; this module is the half that
//! observes the machine. Auto-discovers capabilities for the `local` host on
//! every engine startup via `uname` + `gh auth status` + a per-driver
//! `command -v` — from engine startup, never from schema init (see
//! [`refresh_local_host_auto_capabilities`]).
use std::process::Stdio;
use std::time::Instant;

use anyhow::Result;

use crate::work::WorkDb;
pub use crate::work::{HOST_HEALTH_FAILURE_THRESHOLD, Host, HostCapability, HostHealthOutcome};

/// What one [`refresh_local_host_auto_capabilities`] run found.
#[derive(Debug, Clone)]
pub struct LocalCapabilityRefresh {
    /// Every `auto` capability now stored for the `local` host.
    pub capabilities: Vec<String>,
    /// How long the probes themselves took (no database time included).
    pub probe_elapsed: std::time::Duration,
}

impl LocalCapabilityRefresh {
    /// Number of `driver=` tags discovered.
    pub fn driver_count(&self) -> usize {
        self.capabilities
            .iter()
            .filter(|cap| cap.starts_with("driver="))
            .count()
    }
}

/// Probe the local host and replace all its `auto`-sourced capabilities
/// with the result. User-tagged rows (`source = 'user'`) are left
/// untouched. Called once per engine startup so auth drift and OS/arch
/// changes surface immediately.
///
/// The probe spawns processes (`uname`, `gh auth status`, one login
/// shell per registered driver) and takes seconds, so it runs *before*
/// the shared connection is borrowed — never hold `WorkDb::conn` across
/// a subprocess. This is also why it is not part of schema init any
/// more: see the capability-probe note in `work/schema_init.rs`.
pub fn refresh_local_host_auto_capabilities(db: &WorkDb) -> Result<LocalCapabilityRefresh> {
    let started = Instant::now();
    let capabilities = discover_local_capabilities();
    let probe_elapsed = started.elapsed();
    db.replace_auto_host_capabilities("local", &capabilities)?;
    let refresh = LocalCapabilityRefresh {
        capabilities,
        probe_elapsed,
    };
    if refresh.driver_count() == 0 {
        tracing::warn!(
            "host_registry: local host refresh found no installed drivers; driver-constrained dispatch will hold",
        );
    }
    tracing::debug!(
        count = refresh.capabilities.len(),
        driver_count = refresh.driver_count(),
        "host_registry: refreshed local host auto capabilities",
    );
    Ok(refresh)
}

// ── Local capability discovery ────────────────────────────────────────────────

/// Probe the local host and return its capability tags. Every returned tag
/// is stored with `source = "auto"`. Failures for individual probes are
/// logged and skipped; the remainder still land.
///
/// Not cached: the one caller is the once-per-boot startup refresh, and a
/// cache here would turn a later re-probe into a no-op.
///
/// The tag *spelling* comes from [`crate::host_capability_probe`], shared
/// with the remote probe: capability matching is exact string equality, so
/// a local and a remote macOS host must describe themselves identically or
/// a requirement written against one silently excludes the other.
///
/// Each sub-probe is timed and the breakdown logged at `info`, so a slow
/// cold start can be attributed to `gh auth status` (network + keychain)
/// versus the driver login shells versus `uname` without guessing.
pub(crate) fn discover_local_capabilities() -> Vec<String> {
    use crate::host_capability_probe::{
        arch_capability, discover_local_driver_capabilities, gh_authed_capability, os_capability,
    };

    let started = Instant::now();
    let mut caps: Vec<String> = Vec::new();

    // OS family
    match run_one(&["uname", "-s"]) {
        Some(raw) => caps.push(os_capability(&raw)),
        None => tracing::warn!("host_registry: uname -s failed; os= capability not set"),
    }

    // CPU architecture
    match run_one(&["uname", "-m"]) {
        Some(raw) => caps.push(arch_capability(&raw)),
        None => tracing::warn!("host_registry: uname -m failed; arch= capability not set"),
    }
    let uname_elapsed = started.elapsed();

    // gh auth state (per design open-question: catches credential drift
    // hours earlier than waiting for a `gh pr create` failure in a worker)
    // `gh auth status` validates the token against the API, so it spends
    // from the same shared budget as everything else and goes through the
    // instrumented spawn rather than a bare `Command`.
    let gh_started = Instant::now();
    // Bound to match the per-driver PATH probe (`host_capability_probe` uses
    // 10s). `gh auth status` talks to the network and the keychain; without
    // a timeout a black-holed API or a never-returning prompt holds the
    // startup dispatch gate for the life of the process.
    let gh_authed = boss_gh_telemetry::scope_blocking(boss_gh_telemetry::callers::HOST_REGISTRY, || {
        boss_github::gh_runner::gh_output_blocking_timeout(&["auth", "status"], std::time::Duration::from_secs(10))
    })
    .map(|out| out.status.success())
    .unwrap_or(false);
    caps.push(gh_authed_capability(gh_authed));
    let gh_elapsed = gh_started.elapsed();

    // Installed agent drivers (same vocabulary as the remote probe). A
    // host that has none still gets `drivers-probed=true` so selection
    // can tell "checked, missing" from "never checked".
    let drivers_started = Instant::now();
    caps.extend(discover_local_driver_capabilities());
    let drivers_elapsed = drivers_started.elapsed();

    tracing::info!(
        uname_ms = uname_elapsed.as_millis() as u64,
        gh_auth_status_ms = gh_elapsed.as_millis() as u64,
        driver_probes_ms = drivers_elapsed.as_millis() as u64,
        total_ms = started.elapsed().as_millis() as u64,
        capabilities = ?caps,
        "host_registry: local capability discovery complete",
    );

    caps
}

/// Run a command, capture stdout, trim whitespace. Returns `None` on any error.
fn run_one(argv: &[&str]) -> Option<String> {
    let (cmd, args) = argv.split_first()?;
    let out = std::process::Command::new(cmd)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::test_support::insert_host_capability;

    fn open_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
    }

    #[test]
    fn refresh_local_auto_caps_replaces_auto_leaves_user() {
        let db = open_db();
        // A user cap and a stale auto cap on the local host.
        db.add_user_host_capability("local", "team=infra").unwrap();
        insert_host_capability(&db, "local", "stale=auto", "auto");

        // Runs the real probe (uname, gh, one login shell per driver); it
        // must not hold the shared connection while it does — a guard held
        // here would deadlock the write inside.
        let refresh = refresh_local_host_auto_capabilities(&db).unwrap();

        let caps = db.list_host_capabilities("local").unwrap();
        // The user-sourced row survives the refresh.
        assert!(caps.iter().any(|c| c.capability == "team=infra" && c.source == "user"));
        // The stale auto row is replaced by the fresh probe set.
        assert!(!caps.iter().any(|c| c.capability == "stale=auto"));
        // Whatever the host has installed, discovery always records that it
        // ran, and the stored rows are exactly what the probe returned.
        assert!(refresh.capabilities.iter().any(|c| c == "drivers-probed=true"));
        let stored: Vec<&str> = caps
            .iter()
            .filter(|c| c.source == "auto")
            .map(|c| c.capability.as_str())
            .collect();
        let mut probed: Vec<&str> = refresh.capabilities.iter().map(String::as_str).collect();
        probed.sort_unstable();
        assert_eq!(stored, probed);
    }
}
