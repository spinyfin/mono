//! Codex authentication isolation for concurrent Boss workers.
//!
//! # Policy (codex-cli 0.145.0, empirically verified)
//!
//! Codex stores credentials in `$CODEX_HOME/auth.json` (file mode). On access-
//! token expiry it **refreshes OAuth tokens and rewrites `auth.json` in place**.
//! A `chmod a-w` / read-only `auth.json` makes refresh fail with
//! `Permission denied (os error 13)` and the run fails.
//!
//! Therefore concurrent workers **must not** share one mutable `auth.json`
//! (including via symlink to the operator's interactive `~/.codex/auth.json`).
//! The supported policy is:
//!
//! 1. **Snapshot** — exclusive-lock the source, byte-copy `auth.json` into the
//!    per-run `CODEX_HOME` as a regular file (mode `0o600`), record a content
//!    fingerprint (SHA-256, never log raw bytes / tokens).
//! 2. **Run-local mutability** — leave the per-run file writable so Codex can
//!    persist a mid-run refresh into that isolated home only.
//! 3. **Refresh adoption** — on teardown, exclusive-lock the source again; if
//!    the per-run file's fingerprint changed and its `last_refresh` is newer
//!    than the source's, atomically replace the source with the run-local
//!    bytes so rotated tokens are not lost.
//!
//! Explicitly **rejected**:
//! - Symlinking the per-run `auth.json` at the operator interactive auth file
//!   (untested concurrent races; couples workers to interactive state).
//! - Read-only / immutable per-run `auth.json` (breaks Codex refresh).
//! - Logging credential material (tokens, API keys, full auth JSON).
//!
//! See `tools/boss/docs/investigations/codex-auth-isolation-2026-07-26.md`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Filename Codex reads/writes under `CODEX_HOME`.
pub const AUTH_JSON_NAME: &str = "auth.json";

/// Sidecar lock file next to the source `auth.json` (not inside a CODEX_HOME
/// that Codex may rewrite). Serialises snapshot + adoption against one source.
pub const AUTH_SOURCE_LOCK_NAME: &str = "auth.json.boss-lock";

/// Supported isolation policy. Documented and fixed; callers do not choose a
/// symlink or read-only alternative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthIsolationPolicy {
    /// Per-run byte snapshot of source `auth.json`, writable so Codex can
    /// refresh; adopt newer run-local rotations back to the source on
    /// teardown.
    SnapshotWithRefreshAdoption,
}

impl AuthIsolationPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SnapshotWithRefreshAdoption => "snapshot_with_refresh_adoption",
        }
    }
}

/// Errors from auth isolation operations. Display implementations never
/// include credential bytes — only paths, fingerprints, and structural notes.
#[derive(Debug, Error)]
pub enum CodexAuthError {
    #[error("codex auth source not found at {path}")]
    SourceMissing { path: PathBuf },

    #[error(
        "codex auth source path is a symlink ({path}); refusing — use a regular file source and SnapshotWithRefreshAdoption"
    )]
    SourceIsSymlink { path: PathBuf },

    #[error("codex auth source is not a regular file ({path})")]
    SourceNotRegularFile { path: PathBuf },

    #[error("codex auth JSON at {path} failed structural validation: {reason}")]
    InvalidAuthShape { path: PathBuf, reason: String },

    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to lock codex auth source ({path}): {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Opaque content fingerprint of an `auth.json` body (full SHA-256 hex).
/// Safe to log — it is not reversible to tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthFingerprint(String);

impl AuthFingerprint {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        Self(hex_encode(digest.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Short prefix for log lines (16 hex chars).
    pub fn short(&self) -> &str {
        let s = self.as_str();
        if s.len() >= 16 { &s[..16] } else { s }
    }
}

impl std::fmt::Display for AuthFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.short())
    }
}

/// Result of provisioning auth into a per-run `CODEX_HOME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSnapshot {
    /// Absolute path of the per-run `auth.json` written.
    pub auth_path: PathBuf,
    /// Fingerprint of the bytes installed at provision time.
    pub fingerprint: AuthFingerprint,
    /// Source path that was snapshotted (regular file).
    pub source_path: PathBuf,
    pub policy: AuthIsolationPolicy,
}

/// Outcome of attempting to adopt a run-local refresh back into the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptOutcome {
    /// Per-run file unchanged from the provision fingerprint.
    Unchanged,
    /// Per-run file missing (nothing to adopt).
    RunAuthMissing,
    /// Per-run file changed but its `last_refresh` is not newer than source.
    SourceAlreadyNewer {
        run_fingerprint: AuthFingerprint,
        source_fingerprint: AuthFingerprint,
    },
    /// Source replaced with run-local bytes (rotated tokens adopted).
    Adopted {
        previous_source_fingerprint: AuthFingerprint,
        new_fingerprint: AuthFingerprint,
        last_refresh: Option<String>,
    },
}

/// Resolve the default operator Codex auth path (`$CODEX_HOME/auth.json` or
/// `~/.codex/auth.json`). The returned path is a *discovery* hint only —
/// callers that want isolation must still
/// [`snapshot_auth_into_codex_home`]. Prefer a Boss-managed regular-file
/// source over pointing concurrent workers at the interactive home.
pub fn resolve_operator_auth_path() -> PathBuf {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        let home = home.trim();
        if !home.is_empty() {
            return PathBuf::from(home).join(AUTH_JSON_NAME);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".codex").join(AUTH_JSON_NAME);
    }
    PathBuf::from(".codex").join(AUTH_JSON_NAME)
}

/// Snapshot source `auth.json` into `codex_home/auth.json` under an exclusive
/// lock on the source. Refuses symlink sources. Never logs credential bytes.
pub fn snapshot_auth_into_codex_home(source_auth: &Path, codex_home: &Path) -> Result<AuthSnapshot, CodexAuthError> {
    let source_auth = canonicalize_existing(source_auth)?;
    ensure_regular_file_not_symlink(&source_auth)?;

    let lock_path = source_lock_path(&source_auth);
    let _lock = SourceLock::acquire(&lock_path)?;

    let bytes = read_file_bytes(&source_auth)?;
    validate_auth_json_shape(&bytes, &source_auth)?;
    let fingerprint = AuthFingerprint::from_bytes(&bytes);

    fs::create_dir_all(codex_home).map_err(|source| CodexAuthError::Io {
        path: codex_home.to_path_buf(),
        source,
    })?;
    let dest = codex_home.join(AUTH_JSON_NAME);
    // Refuse to create a symlink destination: always write a regular file.
    if dest
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        fs::remove_file(&dest).map_err(|source| CodexAuthError::Io {
            path: dest.clone(),
            source,
        })?;
    }
    write_private_file(&dest, &bytes)?;

    tracing::info!(
        policy = AuthIsolationPolicy::SnapshotWithRefreshAdoption.as_str(),
        source = %source_auth.display(),
        dest = %dest.display(),
        fingerprint = %fingerprint,
        "codex auth: provisioned per-run snapshot"
    );

    Ok(AuthSnapshot {
        auth_path: dest,
        fingerprint,
        source_path: source_auth,
        policy: AuthIsolationPolicy::SnapshotWithRefreshAdoption,
    })
}

/// If the per-run `auth.json` was rewritten (fingerprint differs from
/// `provisioned`) and carries a newer `last_refresh` than the source, adopt
/// those bytes into the source under an exclusive lock.
///
/// `provisioned` is the snapshot returned from
/// [`snapshot_auth_into_codex_home`]. Callers must pass the same source path.
pub fn adopt_refresh_if_newer(snapshot: &AuthSnapshot, codex_home: &Path) -> Result<AdoptOutcome, CodexAuthError> {
    let run_auth = codex_home.join(AUTH_JSON_NAME);
    if !run_auth.exists() {
        tracing::info!(
            codex_home = %codex_home.display(),
            "codex auth: adopt skipped — per-run auth.json missing"
        );
        return Ok(AdoptOutcome::RunAuthMissing);
    }

    let run_bytes = read_file_bytes(&run_auth)?;
    validate_auth_json_shape(&run_bytes, &run_auth)?;
    let run_fp = AuthFingerprint::from_bytes(&run_bytes);

    if run_fp == snapshot.fingerprint {
        tracing::debug!(
            fingerprint = %run_fp,
            "codex auth: adopt skipped — per-run auth unchanged"
        );
        return Ok(AdoptOutcome::Unchanged);
    }

    let source_auth = &snapshot.source_path;
    ensure_regular_file_not_symlink(source_auth)?;
    let lock_path = source_lock_path(source_auth);
    let _lock = SourceLock::acquire(&lock_path)?;

    // Re-read source under the lock.
    let source_bytes = read_file_bytes(source_auth)?;
    validate_auth_json_shape(&source_bytes, source_auth)?;
    let source_fp = AuthFingerprint::from_bytes(&source_bytes);

    let run_meta = auth_refresh_meta(&run_bytes);
    let source_meta = auth_refresh_meta(&source_bytes);

    let run_newer = match (&run_meta.last_refresh, &source_meta.last_refresh) {
        (Some(r), Some(s)) => r.as_str() > s.as_str(),
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => {
            // No timestamps: adopt only when content differs and source still
            // matches the provision-time fingerprint (no concurrent adoption).
            source_fp == snapshot.fingerprint
        }
    };

    if !run_newer {
        tracing::info!(
            run_fingerprint = %run_fp,
            source_fingerprint = %source_fp,
            run_last_refresh = run_meta.last_refresh.as_deref().unwrap_or("-"),
            source_last_refresh = source_meta.last_refresh.as_deref().unwrap_or("-"),
            "codex auth: adopt skipped — source already newer or equal"
        );
        return Ok(AdoptOutcome::SourceAlreadyNewer {
            run_fingerprint: run_fp,
            source_fingerprint: source_fp,
        });
    }

    write_private_file(source_auth, &run_bytes)?;

    tracing::info!(
        previous_source_fingerprint = %source_fp,
        new_fingerprint = %run_fp,
        last_refresh = run_meta.last_refresh.as_deref().unwrap_or("-"),
        source = %source_auth.display(),
        "codex auth: adopted per-run token refresh into source"
    );

    Ok(AdoptOutcome::Adopted {
        previous_source_fingerprint: source_fp,
        new_fingerprint: run_fp,
        last_refresh: run_meta.last_refresh,
    })
}

// ── internals ──────────────────────────────────────────────────────────────

struct SourceLock {
    _file: File,
}

impl SourceLock {
    fn acquire(lock_path: &Path) -> Result<Self, CodexAuthError> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|source| CodexAuthError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|source| CodexAuthError::Lock {
                path: lock_path.to_path_buf(),
                source,
            })?;
        FileExt::lock_exclusive(&file).map_err(|source| CodexAuthError::Lock {
            path: lock_path.to_path_buf(),
            source,
        })?;
        Ok(Self { _file: file })
    }
}

fn source_lock_path(source_auth: &Path) -> PathBuf {
    match source_auth.parent() {
        Some(parent) => parent.join(AUTH_SOURCE_LOCK_NAME),
        None => PathBuf::from(AUTH_SOURCE_LOCK_NAME),
    }
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf, CodexAuthError> {
    if !path.exists() {
        return Err(CodexAuthError::SourceMissing {
            path: path.to_path_buf(),
        });
    }
    // Do not use fs::canonicalize here for the symlink check — that resolves
    // the link. We want the caller's path identity for the symlink refusal,
    // then work on that path's openable target only after the check.
    Ok(path.to_path_buf())
}

fn ensure_regular_file_not_symlink(path: &Path) -> Result<(), CodexAuthError> {
    let meta = fs::symlink_metadata(path).map_err(|source| CodexAuthError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.file_type().is_symlink() {
        return Err(CodexAuthError::SourceIsSymlink {
            path: path.to_path_buf(),
        });
    }
    if !meta.is_file() {
        return Err(CodexAuthError::SourceNotRegularFile {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn read_file_bytes(path: &Path) -> Result<Vec<u8>, CodexAuthError> {
    let mut f = File::open(path).map_err(|source| CodexAuthError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).map_err(|source| CodexAuthError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(buf)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), CodexAuthError> {
    // Atomic-ish: write to sibling temp then rename.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.boss-tmp-{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("auth"),
        std::process::id()
    ));
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|source| CodexAuthError::Io {
                path: tmp.clone(),
                source,
            })?;
        f.write_all(bytes).map_err(|source| CodexAuthError::Io {
            path: tmp.clone(),
            source,
        })?;
        f.sync_all().map_err(|source| CodexAuthError::Io {
            path: tmp.clone(),
            source,
        })?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&tmp, perms).map_err(|source| CodexAuthError::Io {
            path: tmp.clone(),
            source,
        })?;
    }
    fs::rename(&tmp, path).map_err(|source| CodexAuthError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Structural validation only — never inspects token *values*.
#[derive(Debug, Deserialize)]
struct AuthJsonShape {
    /// Present for ChatGPT OAuth file mode (may be null).
    #[serde(default, rename = "OPENAI_API_KEY")]
    openai_api_key: Option<serde_json::Value>,
    #[serde(default)]
    tokens: Option<AuthTokensShape>,
    #[serde(default)]
    last_refresh: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthTokensShape {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Default)]
struct AuthRefreshMeta {
    last_refresh: Option<String>,
}

fn auth_refresh_meta(bytes: &[u8]) -> AuthRefreshMeta {
    match serde_json::from_slice::<AuthJsonShape>(bytes) {
        Ok(shape) => AuthRefreshMeta {
            last_refresh: shape.last_refresh,
        },
        Err(_) => AuthRefreshMeta::default(),
    }
}

fn validate_auth_json_shape(bytes: &[u8], path: &Path) -> Result<(), CodexAuthError> {
    let shape: AuthJsonShape = serde_json::from_slice(bytes).map_err(|e| CodexAuthError::InvalidAuthShape {
        path: path.to_path_buf(),
        reason: format!("json parse: {e}"),
    })?;

    // Accept either ChatGPT tokens or an API key. Never log which tokens
    // are present beyond boolean-ish structural reasons.
    let has_api_key = match &shape.openai_api_key {
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(serde_json::Value::Null) | None => false,
        Some(_) => true,
    };
    let has_tokens = shape
        .tokens
        .as_ref()
        .map(|t| {
            t.access_token.as_ref().is_some_and(|s| !s.is_empty())
                || t.refresh_token.as_ref().is_some_and(|s| !s.is_empty())
        })
        .unwrap_or(false);

    if !has_api_key && !has_tokens {
        return Err(CodexAuthError::InvalidAuthShape {
            path: path.to_path_buf(),
            reason: "neither OPENAI_API_KEY nor tokens.access_token/refresh_token present".into(),
        });
    }

    // Silence unused field warnings in release while keeping the shape for
    // future structural checks (id_token / account_id presence is optional).
    let _ = shape.tokens.as_ref().map(|t| (&t.id_token, &t.account_id));

    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    fn sample_auth(last_refresh: &str, access: &str, refresh: &str) -> String {
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "id-test",
                "access_token": access,
                "refresh_token": refresh,
                "account_id": "acct-test"
            },
            "last_refresh": last_refresh
        })
        .to_string()
    }

    #[test]
    fn snapshot_copies_bytes_as_regular_private_file() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source-auth.json");
        let body = sample_auth("2026-01-01T00:00:00Z", "access-aaa", "refresh-aaa");
        fs::write(&source, &body).unwrap();

        let home = dir.path().join("run-home");
        let snap = snapshot_auth_into_codex_home(&source, &home).unwrap();

        assert_eq!(snap.policy, AuthIsolationPolicy::SnapshotWithRefreshAdoption);
        assert_eq!(snap.auth_path, home.join(AUTH_JSON_NAME));
        assert!(snap.auth_path.is_file());
        assert!(!snap.auth_path.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&snap.auth_path).unwrap(), body);
        assert_eq!(snap.fingerprint, AuthFingerprint::from_bytes(body.as_bytes()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&snap.auth_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn snapshot_refuses_symlink_source() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real.json");
        fs::write(&real, sample_auth("2026-01-01T00:00:00Z", "a", "r")).unwrap();
        let link = dir.path().join("link.json");
        symlink(&real, &link).unwrap();

        let err = snapshot_auth_into_codex_home(&link, &dir.path().join("h")).unwrap_err();
        assert!(matches!(err, CodexAuthError::SourceIsSymlink { .. }));
        // Error display must not contain token-looking material from the file.
        let msg = err.to_string();
        assert!(!msg.contains("access-"));
        assert!(!msg.contains("refresh-"));
    }

    #[test]
    fn snapshot_replaces_existing_symlink_destination_with_regular_file() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.json");
        let body = sample_auth("2026-01-01T00:00:00Z", "access-bbb", "refresh-bbb");
        fs::write(&source, &body).unwrap();

        let home = dir.path().join("run-home");
        fs::create_dir_all(&home).unwrap();
        let elsewhere = dir.path().join("elsewhere.json");
        fs::write(&elsewhere, b"should-not-be-used").unwrap();
        symlink(&elsewhere, home.join(AUTH_JSON_NAME)).unwrap();

        let snap = snapshot_auth_into_codex_home(&source, &home).unwrap();
        assert!(!snap.auth_path.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&snap.auth_path).unwrap(), body);
        // Operator / "elsewhere" target must not have been mutated via the old link.
        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "should-not-be-used");
    }

    #[test]
    fn adopt_noop_when_unchanged() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.json");
        let body = sample_auth("2026-01-01T00:00:00Z", "access-c", "refresh-c");
        fs::write(&source, &body).unwrap();
        let home = dir.path().join("run");
        let snap = snapshot_auth_into_codex_home(&source, &home).unwrap();
        let outcome = adopt_refresh_if_newer(&snap, &home).unwrap();
        assert_eq!(outcome, AdoptOutcome::Unchanged);
    }

    #[test]
    fn adopt_writes_back_when_run_last_refresh_newer() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.json");
        let original = sample_auth("2026-01-01T00:00:00Z", "access-old", "refresh-old");
        fs::write(&source, &original).unwrap();
        let home = dir.path().join("run");
        let snap = snapshot_auth_into_codex_home(&source, &home).unwrap();

        let rotated = sample_auth("2026-07-26T12:00:00Z", "access-new", "refresh-new");
        fs::write(home.join(AUTH_JSON_NAME), &rotated).unwrap();

        let outcome = adopt_refresh_if_newer(&snap, &home).unwrap();
        match outcome {
            AdoptOutcome::Adopted {
                new_fingerprint,
                last_refresh,
                ..
            } => {
                assert_eq!(new_fingerprint, AuthFingerprint::from_bytes(rotated.as_bytes()));
                assert_eq!(last_refresh.as_deref(), Some("2026-07-26T12:00:00Z"));
            }
            other => panic!("expected Adopted, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&source).unwrap(), rotated);
    }

    #[test]
    fn adopt_skips_when_source_already_newer() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.json");
        let original = sample_auth("2026-01-01T00:00:00Z", "access-old", "refresh-old");
        fs::write(&source, &original).unwrap();
        let home = dir.path().join("run");
        let snap = snapshot_auth_into_codex_home(&source, &home).unwrap();

        // Another worker adopted a newer rotation into the source first.
        let newer_source = sample_auth("2026-07-26T13:00:00Z", "access-src", "refresh-src");
        fs::write(&source, &newer_source).unwrap();

        // This run also refreshed, but with an older last_refresh.
        let run_rotated = sample_auth("2026-07-26T12:00:00Z", "access-run", "refresh-run");
        fs::write(home.join(AUTH_JSON_NAME), &run_rotated).unwrap();

        let outcome = adopt_refresh_if_newer(&snap, &home).unwrap();
        assert!(matches!(outcome, AdoptOutcome::SourceAlreadyNewer { .. }));
        assert_eq!(fs::read_to_string(&source).unwrap(), newer_source);
    }

    #[test]
    fn rejects_empty_credentials_shape() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.json");
        fs::write(&source, r#"{"OPENAI_API_KEY":null,"tokens":{}}"#).unwrap();
        let err = snapshot_auth_into_codex_home(&source, &dir.path().join("h")).unwrap_err();
        assert!(matches!(err, CodexAuthError::InvalidAuthShape { .. }));
    }

    #[test]
    fn error_display_never_includes_token_values() {
        let err = CodexAuthError::InvalidAuthShape {
            path: PathBuf::from("/tmp/auth.json"),
            reason: "neither OPENAI_API_KEY nor tokens.access_token/refresh_token present".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/auth.json"));
        assert!(!msg.contains("sk-"));
        assert!(!msg.contains("eyJ"));
    }

    #[test]
    fn fingerprint_is_stable_and_not_raw_bytes() {
        let body = sample_auth("t", "access-secret-value", "refresh-secret-value");
        let fp = AuthFingerprint::from_bytes(body.as_bytes());
        assert_eq!(fp.as_str().len(), 64);
        assert!(!fp.as_str().contains("secret"));
        assert_eq!(fp.short().len(), 16);
    }

    #[test]
    fn resolve_operator_auth_path_prefers_codex_home_env() {
        // SAFETY: test process; we restore after.
        let prev = std::env::var_os("CODEX_HOME");
        // use a unique value
        // (env mutation is process-global; keep the critical section short)
        // For hermeticity we only check the pure path join logic via a direct call
        // when env is set — if another test races, still a PathBuf shape check.
        unsafe {
            std::env::set_var("CODEX_HOME", "/tmp/boss-codex-auth-test-home");
        }
        let p = resolve_operator_auth_path();
        match prev {
            Some(v) => unsafe { std::env::set_var("CODEX_HOME", v) },
            None => unsafe { std::env::remove_var("CODEX_HOME") },
        }
        assert_eq!(p, PathBuf::from("/tmp/boss-codex-auth-test-home/auth.json"));
    }
}
