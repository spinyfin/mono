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
//! refuses to overwrite a token whose socket is still answering, so
//! even a misconfigured/future caller that *does* land on the
//! production path can't clobber it; and [`ControlTokenGuard`] compares
//! the full token (not just pid) before deleting, so it only ever
//! removes the exact secret it minted.
//!
//! [`write_token_file`]'s refusal originally gated on the recorded
//! pid's liveness alone. Pids recycle — very likely across a reboot —
//! so an engine killed uncleanly left a token file that, once its pid
//! got reassigned to some unrelated process, made every subsequent
//! engine start read a "live" pid and fail hard with no recovery path.
//! The token file already records `socket_path`, which nothing else can
//! wear: a socket is either bound by a live listener or it isn't. The
//! check now reconciles on that instead, treating the recorded pid as
//! a diagnostic hint rather than the gate.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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
    boss_log_files::default_control_token_path()
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

/// Is `existing`'s recorded engine still actually serving?
///
/// Reconciles on socket reachability rather than pid liveness: a pid is
/// recyclable (very likely across a reboot), but a socket is either bound
/// by a live listener or it isn't — nothing else can wear its address. The
/// recorded pid is logged as a diagnostic hint only; it never gates the
/// decision.
fn existing_token_is_live(path: &Path, existing: &ControlTokenFile) -> bool {
    let socket_live = crate::events_socket::path_has_a_live_listener(Path::new(&existing.socket_path));
    if !socket_live && process_is_alive(existing.pid as libc::pid_t) {
        tracing::warn!(
            token_path = %path.display(),
            recorded_pid = existing.pid,
            socket_path = %existing.socket_path,
            "reclaiming stale engine-control token: recorded pid is alive but its socket is dead \
             (pid was very likely recycled since the previous engine exited)",
        );
    }
    socket_live
}

/// Write the token file with mode 0600, creating parent directories
/// as needed. The write itself is atomic (write-temp-then-rename): a
/// crash or error between the reconciliation check and the write can
/// never leave a truncated or partial token file behind.
///
/// Refuses to clobber a token that is still owned by a live engine: if a
/// file already exists at `path`, parses, and its recorded `socket_path`
/// still has a live listener, this returns an actionable error instead of
/// overwriting it — the operator must stop that engine first, there is no
/// path-file-only recovery when the other engine is genuinely still
/// running. This is the fix for the 2026-07 incident where a
/// worker-launched fixture engine overwrote — and, on its own shutdown,
/// then deleted — the production control token.
///
/// A prior file that fails to parse, or whose socket is no longer
/// answering, is stale (e.g. left behind by a previous engine that
/// crashed without cleanup, or one whose pid was later recycled by an
/// unrelated process) and is safely reclaimed — this is the recovery
/// path: the common case of an uncleanly-killed engine no longer requires
/// any operator intervention to start a fresh one.
pub fn write_token_file(path: &Path, contents: &ControlTokenFile) -> Result<()> {
    if let Ok(existing_raw) = std::fs::read_to_string(path)
        && let Ok(existing) = serde_json::from_str::<ControlTokenFile>(&existing_raw)
        && existing_token_is_live(path, &existing)
    {
        anyhow::bail!(
            "refusing to overwrite engine-control token at {}: its socket {} still has a live listener \
             (recorded pid {}). Another engine appears to be running at this path — stop it before \
             starting a new one here.",
            path.display(),
            existing.socket_path,
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

    let tmp_path = sibling_tmp_path(path);
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(TOKEN_FILE_MODE)
            .open(&tmp_path)
            .with_context(|| format!("failed to open control-token temp file {}", tmp_path.display()))?;
        file.write_all(serialized.as_bytes())
            .with_context(|| format!("failed to write control-token temp file {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync control-token temp file {}", tmp_path.display()))?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to atomically install control-token file {} (from temp {})",
            path.display(),
            tmp_path.display()
        )
    })?;
    Ok(())
}

/// Sibling temp path used to stage an atomic write of `path`: same
/// directory (so the final `rename` is same-filesystem and atomic),
/// suffixed with this process's pid so concurrent writers never collide
/// on the temp file itself.
fn sibling_tmp_path(path: &Path) -> PathBuf {
    let mut tmp_name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(tmp_name)
}

/// RAII guard that removes the token file when dropped — both on
/// graceful return and on panic-unwind.
///
/// Refuses to delete a file that some other writer replaced at this path
/// after we wrote ours. Pid and token matching alone do *not* establish
/// that: `write_token_file` always installs via `rename`, but nothing
/// stops some other engine from later reclaiming the same path and, by
/// coincidence or because it copied our content, ending up with a file
/// that carries the same pid and token bytes we minted. What actually
/// distinguishes "the file I created" from "a file that now sits at the
/// path I wrote to" is device+inode identity, captured at construction
/// time and compared again at drop — a `rename` always produces a fresh
/// inode, so a later replacement is caught even if pid and token happen
/// to match. The pid/token comparison is kept as a second, independent
/// check: it survives even if inode capture failed (e.g. the file was
/// briefly missing right after we wrote it).
pub struct ControlTokenGuard {
    path: PathBuf,
    pid: u32,
    token: String,
    /// Device+inode of `path` at construction time, i.e. right after
    /// `write_token_file` renamed our temp file into place. `None` if the
    /// stat failed (in which case `Drop` falls back to the pid/token
    /// check alone).
    identity: Option<(u64, u64)>,
}

impl ControlTokenGuard {
    /// Captures `path`'s current device+inode (the file this guard is
    /// responsible for) alongside the pid/token this engine minted. Must
    /// be called immediately after `write_token_file` installs the file,
    /// so the captured identity is that of the file we just wrote, not
    /// some earlier occupant of the path.
    pub fn new(path: PathBuf, pid: u32, token: String) -> Self {
        let identity = std::fs::metadata(&path).ok().map(|m| (m.dev(), m.ino()));
        Self {
            path,
            pid,
            token,
            identity,
        }
    }
}

impl Drop for ControlTokenGuard {
    fn drop(&mut self) {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return;
        };
        if let Some(identity) = self.identity
            && (metadata.dev(), metadata.ino()) != identity
        {
            // Some other writer replaced the file at this path since we
            // wrote it — never touch a file we didn't create.
            return;
        }
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

    /// Exercises the pid/token comparison in isolation from the inode
    /// check: a guard constructed with a token that does not match what's
    /// on disk must not delete the file, purely on the strength of that
    /// mismatch. Note this state is a hypothetical, not one `serve()` can
    /// produce — in practice a guard is always constructed with the exact
    /// token `write_token_file` just wrote (see `app/server.rs`), so this
    /// pins the pid/token fallback's own behavior rather than modeling a
    /// reachable production regression. `guard_leaves_file_replaced_at_the_same_path_even_with_matching_pid_and_token`
    /// below covers the regression that IS reachable: a second writer
    /// replacing the file at this path.
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

    /// The regression that IS reachable: some other writer replaces the
    /// file at this path after we constructed our guard, and the
    /// replacement happens to carry byte-for-byte the same pid and token
    /// we minted (e.g. a reclamation race that copies forward the prior
    /// content). Pid+token comparison alone would wrongly treat this as
    /// "our file" and delete it; device+inode identity — captured at
    /// guard-construction time — catches it because `write_token_file`
    /// always installs via `rename`, which mints a fresh inode.
    #[test]
    fn guard_leaves_file_replaced_at_the_same_path_even_with_matching_pid_and_token() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "shared-token".into(),
            socket_path: "/x".into(),
            pid: 99,
        };
        write_token_file(&path, &contents).unwrap();
        let guard = ControlTokenGuard::new(path.clone(), 99, "shared-token".into());

        // Some other writer replaces the file at this path with
        // byte-for-byte the same pid/token our guard recorded. `socket_path`
        // here is dead, so `write_token_file` treats the existing file as
        // stale and reclaims it — installing a fresh inode via `rename`.
        write_token_file(&path, &contents).unwrap();

        drop(guard);
        assert!(
            path.exists(),
            "guard must not remove a file that was replaced at this path, even with matching pid/token"
        );
    }

    /// Regression for the escalation half of the incident: a live engine's
    /// token must never be silently overwritten by another process that
    /// resolves the same path (e.g. a worker-launched fixture engine that,
    /// due to an isolation bug, computed the production token path). The
    /// gate is the recorded socket actually having a live listener — a real
    /// `UnixListener` bound for the duration of the test — not the pid.
    #[test]
    fn write_token_file_refuses_to_overwrite_live_socket() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let socket_path = dir.path().join("prod.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();

        let production = ControlTokenFile {
            token: "production-secret".into(),
            socket_path: socket_path.display().to_string(),
            pid: std::process::id(),
        };
        write_token_file(&path, &production).unwrap();

        let fixture = ControlTokenFile {
            token: "fixture-secret".into(),
            socket_path: dir.path().join("fixture.sock").display().to_string(),
            pid: std::process::id(),
        };
        let err = write_token_file(&path, &fixture).expect_err("must refuse to clobber a live engine's token");
        assert!(err.to_string().contains("still has a live listener"), "{err}");

        // Production's token must be byte-for-byte unchanged.
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ControlTokenFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.token, "production-secret");
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
            socket_path: "/nonexistent/stale.sock".into(),
            pid: i32::MAX as u32,
        };
        write_token_file(&path, &stale).unwrap();

        let fresh = ControlTokenFile {
            token: "fresh-secret".into(),
            socket_path: "/nonexistent/fresh.sock".into(),
            pid: std::process::id(),
        };
        write_token_file(&path, &fresh).expect("a dead socket's token must be reclaimable");

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ControlTokenFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.token, "fresh-secret");
    }

    /// The pid-recycle regression this fix targets: the recorded pid
    /// happens to be alive (recycled by an unrelated process across a
    /// reboot, or simply this very test process) but its socket is dead —
    /// the write must still succeed instead of hard-failing, because
    /// liveness is decided by the socket, not the pid.
    #[test]
    fn write_token_file_reclaims_when_socket_is_dead_even_though_recorded_pid_is_alive() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let stale = ControlTokenFile {
            token: "stale-secret".into(),
            socket_path: dir.path().join("dead.sock").display().to_string(),
            // This test process's own pid: guaranteed alive, standing in for
            // a recycled pid that now belongs to some unrelated live process.
            pid: std::process::id(),
        };
        write_token_file(&path, &stale).unwrap();

        let fresh = ControlTokenFile {
            token: "fresh-secret".into(),
            socket_path: dir.path().join("fresh.sock").display().to_string(),
            pid: std::process::id(),
        };
        write_token_file(&path, &fresh)
            .expect("a dead socket's token must be reclaimable even when the recorded pid is alive");

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ControlTokenFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.token, "fresh-secret");
    }

    /// Atomicity: if the temp-file write step fails partway (simulated here
    /// by pre-occupying the exact temp path this pid would use with a
    /// directory, so `OpenOptions::open` fails before any bytes are
    /// written), the original file at `path` must be left completely
    /// untouched — the old non-atomic read-check-then-truncate
    /// implementation would have already truncated it by this point.
    #[test]
    fn write_token_file_leaves_original_intact_when_temp_write_fails() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let original = ControlTokenFile {
            token: "original-secret".into(),
            socket_path: dir.path().join("dead.sock").display().to_string(),
            pid: i32::MAX as u32,
        };
        write_token_file(&path, &original).unwrap();

        let tmp_path = sibling_tmp_path(&path);
        std::fs::create_dir(&tmp_path).unwrap();

        let fresh = ControlTokenFile {
            token: "fresh-secret".into(),
            socket_path: dir.path().join("fresh.sock").display().to_string(),
            pid: std::process::id(),
        };
        write_token_file(&path, &fresh).expect_err("temp-file creation must fail while the temp path is a directory");

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ControlTokenFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed.token, "original-secret",
            "a failed write must not touch the original file"
        );

        std::fs::remove_dir(&tmp_path).unwrap();
    }

    /// No leftover temp file after a successful write.
    #[test]
    fn write_token_file_cleans_up_temp_file_on_success() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine-control.token");
        let contents = ControlTokenFile {
            token: "x".into(),
            socket_path: "/nonexistent.sock".into(),
            pid: 1,
        };
        write_token_file(&path, &contents).unwrap();

        assert!(!sibling_tmp_path(&path).exists());
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "only the final token file should remain: {entries:?}");
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
