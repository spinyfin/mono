//! Engine-control authentication token.
//!
//! The frontend socket sits on a well-known path that any process on
//! the same user/machine can dial. SIGTERM has the same property — any
//! caller with the right pid can land it, and the engine can't tell a
//! deliberate "the macOS app is auto-restarting me" signal apart from
//! a worker accidentally targeting `/tmp/boss-engine.pid`. Two
//! `bazel test`-mediated incidents in May 2026 killed the live engine
//! exactly that way (issue #705).
//!
//! This module owns the secret half of the proposed defense: a random
//! 32-byte token, written to a 0600 file under
//! `~/Library/Application Support/Boss/`. The `shutdown` RPC on the
//! frontend socket accepts the token and only the token; SIGTERM
//! becomes the fallback for OS-shutdown / panic paths rather than the
//! everyday "restart engine" gesture.
//!
//! The token file is the boundary the bazel sandbox already enforces:
//! `darwin-sandbox` denies test actions any access under
//! `~/Library/Application Support/`, so a test that ends up calling
//! the canonical-shutdown path reads `ENOENT`, fails auth, and the
//! live engine survives.
//!
//! That boundary only holds as long as the token path resolution
//! itself stays inside isolation. It didn't: `default_token_path()`
//! was called unconditionally, outside `IsolationPaths` (`app.rs`),
//! so a worker-launched engine (e.g. a Swift XCTest fixture) that sets
//! only `--socket-path` still resolved and wrote the *production*
//! token path — reading back what it wrote hands that process
//! Boss-tier shutdown authority (escalation), and its own shutdown
//! guard then deleted the file out from under production (denial of
//! service; this fired for real in 2026-07). The fix has three parts:
//! the token path is now derived alongside `db_path` / `events_socket`
//! / `pid_path` in `IsolationPaths` so an isolated fixture never
//! computes the production path in the first place; [`write_token_file`]
//! refuses to overwrite a token whose recorded pid is still alive, so
//! even a misconfigured/future caller that *does* land on the
//! production path can't clobber it; and [`ControlTokenGuard`] compares
//! the full token (not just pid) before deleting, so it only ever
//! removes the exact secret it minted.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::process_is_alive;

/// File mode for the control-token file. The token is the auth
/// credential for the shutdown RPC, so the file must not be readable
/// by other users on multi-tenant macs.
const TOKEN_FILE_MODE: u32 = 0o600;

/// Optional override for the token path. Mirrors the pattern used
/// by `BOSS_ENGINE_PID_PATH` / `BOSS_ENGINE_AUDIT_PATH` so tests can
/// point this somewhere harmless.
pub const TOKEN_PATH_ENV: &str = "BOSS_ENGINE_CONTROL_TOKEN_PATH";

/// On-disk layout for the token file. Stored as JSON rather than raw
/// bytes so the file is self-describing — a future tool that needs to
/// reconcile "which engine does this token belong to?" has the socket
/// path right there.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlTokenFile {
    /// Hex-encoded random bytes. The engine compares against this
    /// value verbatim; rotation is per-engine-start.
    pub token: String,
    /// The frontend socket path the engine bound on this run. A client
    /// that resolves the production path via `BossEnginePaths` and the
    /// token via this file can confirm they're talking to the engine
    /// that minted the token before sending the shutdown RPC.
    pub socket_path: String,
    /// Engine pid that minted the token. Diagnostic only — the RPC
    /// itself only validates the token string.
    pub pid: u32,
}

/// Default token location: alongside the other Boss state files under
/// `~/Library/Application Support/Boss/`. Honours
/// [`TOKEN_PATH_ENV`] first so a test instance can point this
/// elsewhere without inheriting the production path.
pub fn default_token_path() -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os(TOKEN_PATH_ENV) {
        let trimmed = override_path.to_string_lossy().trim().to_owned();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Boss/engine-control.token"))
}

/// Generate a fresh 32-byte hex-encoded token. Backed by
/// `fastrand::Rng::with_seed`-style RNG seeded from `getrandom` —
/// `fastrand` is already in the workspace deps, and 256 bits of
/// entropy via its `Rng::u64()` rolled four times is overkill for an
/// auth credential whose threat model is "the wrong test ended up in
/// the production codepath", not "a remote adversary."
pub fn generate_token() -> String {
    // Seed from OS entropy by way of `fastrand`'s default seeder
    // (`fastrand::Rng::new()` already uses a thread-local CSPRNG-ish
    // seed). Four u64 draws → 32 bytes → 64 hex chars.
    let mut rng = fastrand::Rng::new();
    let mut bytes = [0u8; 32];
    for chunk in bytes.chunks_mut(8) {
        let word = rng.u64(..);
        let word_bytes = word.to_le_bytes();
        chunk.copy_from_slice(&word_bytes[..chunk.len()]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
}

/// Write the token file with mode 0600, creating parent directories
/// as needed.
///
/// Refuses to clobber a token that is still owned by a live engine:
/// if a file already exists at `path`, parses, and its recorded pid is
/// still alive, this returns an error instead of overwriting it. This
/// is the fix for the 2026-07 incident where a worker-launched fixture
/// engine overwrote — and, on its own shutdown, then deleted — the
/// production control token (issue: engine-control token writable and
/// deletable by any worker-launched engine). Pid liveness is the same
/// check `bind_events_socket`'s isolation guard should be doing for
/// the events socket.
///
/// A prior file that fails to parse, or whose pid is no longer alive,
/// is stale (e.g. left behind by a previous engine that crashed
/// without cleanup) and is safely overwritten.
pub fn write_token_file(path: &Path, contents: &ControlTokenFile) -> Result<()> {
    if let Ok(existing_raw) = std::fs::read_to_string(path)
        && let Ok(existing) = serde_json::from_str::<ControlTokenFile>(&existing_raw)
        && process_is_alive(existing.pid as libc::pid_t)
    {
        anyhow::bail!(
            "refusing to overwrite engine-control token at {}: still owned by live engine pid {}",
            path.display(),
            existing.pid
        );
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create control-token directory {}", parent.display()))?;
    }

    let serialized = serde_json::to_string(contents).context("failed to serialize control-token file")?;

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(TOKEN_FILE_MODE)
        .open(path)
        .with_context(|| format!("failed to open control-token file {}", path.display()))?;
    file.write_all(serialized.as_bytes())
        .with_context(|| format!("failed to write control-token file {}", path.display()))?;
    Ok(())
}

/// RAII guard that removes the token file when dropped — both on
/// graceful return and on panic-unwind.
///
/// Only removes the file if it still holds *exactly* what this engine
/// wrote: both the pid and the token string must match. Pid alone is
/// necessary but insufficient — a fixture that (due to a bug upstream)
/// resolved and wrote the *production* token path is, by pid, always
/// "the writer" of that path, so a pid-only check would still let it
/// delete production's token on shutdown. Comparing the full token
/// establishes ownership at the path level: this guard only ever
/// removes the exact secret it minted, never merely a file that
/// happens to share its pid.
pub struct ControlTokenGuard {
    path: PathBuf,
    pid: u32,
    token: String,
}

impl ControlTokenGuard {
    pub fn new(path: PathBuf, pid: u32, token: String) -> Self {
        Self { path, pid, token }
    }
}

impl Drop for ControlTokenGuard {
    fn drop(&mut self) {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<ControlTokenFile>(&raw) else {
            // Don't remove a file we can't parse — it might belong to
            // a future engine version.
            return;
        };
        if parsed.pid != self.pid || parsed.token != self.token {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generate_token_returns_64_hex_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_is_not_constant() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "two consecutive draws collided — RNG broken?");
    }

    #[test]
    fn write_token_file_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "deadbeef".into(),
            socket_path: "/tmp/boss-engine.sock".into(),
            pid: 12345,
        };
        write_token_file(&path, &contents).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ControlTokenFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.token, "deadbeef");
        assert_eq!(parsed.socket_path, "/tmp/boss-engine.sock");
        assert_eq!(parsed.pid, 12345);
    }

    #[test]
    fn write_token_file_creates_parent_dir() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/dir/engine-control.token");
        let contents = ControlTokenFile {
            token: "x".into(),
            socket_path: "/x".into(),
            pid: 1,
        };
        write_token_file(&path, &contents).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn write_token_file_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "x".into(),
            socket_path: "/x".into(),
            pid: 1,
        };
        write_token_file(&path, &contents).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn guard_removes_file_with_matching_pid_and_token() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "x".into(),
            socket_path: "/x".into(),
            pid: 99,
        };
        write_token_file(&path, &contents).unwrap();
        {
            let _guard = ControlTokenGuard::new(path.clone(), 99, "x".into());
        }
        assert!(!path.exists(), "guard should remove the file");
    }

    #[test]
    fn guard_leaves_file_with_mismatched_pid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "x".into(),
            socket_path: "/x".into(),
            pid: 12345,
        };
        write_token_file(&path, &contents).unwrap();
        {
            let _guard = ControlTokenGuard::new(path.clone(), 99, "x".into());
        }
        assert!(path.exists(), "guard with mismatched pid must not remove the file");
    }

    /// Path-level ownership regression: pid matching alone is not enough.
    /// A guard must only remove the file if the token it recorded at
    /// creation still matches what's on disk — otherwise a fixture that
    /// (due to an isolation bug) wrote at the *production* token path
    /// would delete production's token on shutdown simply because its
    /// own pid matches what it itself wrote there.
    #[test]
    fn guard_leaves_file_with_matching_pid_but_mismatched_token() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "the-real-production-token".into(),
            socket_path: "/x".into(),
            pid: 99,
        };
        write_token_file(&path, &contents).unwrap();
        {
            // Guard believes it minted a different token under the same pid
            // (e.g. it read a stale local copy before someone else's write
            // landed at this path).
            let _guard = ControlTokenGuard::new(path.clone(), 99, "a-different-token".into());
        }
        assert!(
            path.exists(),
            "guard must not remove a file whose token no longer matches what it minted, even if the pid matches"
        );
    }

    /// Regression for the escalation half of the incident: a live engine's
    /// token must never be silently overwritten by another process that
    /// resolves the same path (e.g. a worker-launched fixture engine that,
    /// due to an isolation bug, computed the production token path).
    #[test]
    fn write_token_file_refuses_to_overwrite_live_pid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let live_pid = std::process::id();
        let production = ControlTokenFile {
            token: "production-secret".into(),
            socket_path: "/prod.sock".into(),
            pid: live_pid,
        };
        write_token_file(&path, &production).unwrap();

        let fixture = ControlTokenFile {
            token: "fixture-secret".into(),
            socket_path: "/fixture.sock".into(),
            pid: live_pid + 1,
        };
        let err = write_token_file(&path, &fixture).expect_err("must refuse to clobber a live engine's token");
        assert!(err.to_string().contains("still owned by live engine"), "{err}");

        // Production's token must be byte-for-byte unchanged.
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ControlTokenFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.token, "production-secret");
        assert_eq!(parsed.pid, live_pid);
    }

    /// The DoS half: a token file left behind by a process that is no
    /// longer running (a crash, or a fixture whose process has since
    /// exited) is stale and must be safely reclaimable — otherwise a
    /// single crash would wedge every future engine start at that path.
    #[test]
    fn write_token_file_overwrites_stale_dead_pid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let stale = ControlTokenFile {
            token: "stale-secret".into(),
            socket_path: "/stale.sock".into(),
            pid: i32::MAX as u32,
        };
        write_token_file(&path, &stale).unwrap();

        let fresh = ControlTokenFile {
            token: "fresh-secret".into(),
            socket_path: "/fresh.sock".into(),
            pid: std::process::id(),
        };
        write_token_file(&path, &fresh).expect("a dead pid's token must be reclaimable");

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ControlTokenFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.token, "fresh-secret");
    }

    /// An unparsable existing file (corrupt, or from a future schema) must
    /// not permanently wedge the token path — it's treated as stale rather
    /// than refused, mirroring the guard's own "don't remove what we can't
    /// parse" stance applied to the write side instead.
    #[test]
    fn write_token_file_overwrites_unparsable_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        std::fs::write(&path, b"not json").unwrap();

        let fresh = ControlTokenFile {
            token: "fresh-secret".into(),
            socket_path: "/fresh.sock".into(),
            pid: std::process::id(),
        };
        write_token_file(&path, &fresh).expect("unparsable existing content must not block a fresh write");

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ControlTokenFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.token, "fresh-secret");
    }

    #[test]
    fn default_token_path_honours_env_override() {
        let dir = TempDir::new().unwrap();
        let override_path = dir.path().join("override.token");
        // SAFETY: single-threaded test scope.
        unsafe {
            std::env::set_var(TOKEN_PATH_ENV, &override_path);
        }
        let resolved = default_token_path().unwrap();
        assert_eq!(resolved, override_path);
        unsafe {
            std::env::remove_var(TOKEN_PATH_ENV);
        }
    }
}
