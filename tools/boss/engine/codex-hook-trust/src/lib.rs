//! Codex hook-trust provisioning and the per-run guardrail arming gate.
//!
//! # Why this exists
//!
//! Codex carries Boss's command guardrails on Claude-wire-compatible
//! `PreToolUse` hooks (design decision: operator chose hook-based
//! `ToolUseInterception` over a `PATH`-shim rewrite for the Codex driver).
//! Those hooks fail **open and silently** when untrusted: an untrusted or
//! misconfigured hook is skipped with no stream event, no log line, and no
//! error. Boss regenerates per-run `CODEX_HOME` config every dispatch, so a
//! stale or missing `[hooks.state.<key>].trusted_hash` would disarm every
//! guardrail with nothing to observe.
//!
//! `--dangerously-bypass-hook-trust` is **not** an acceptable answer: it also
//! trusts project-local `.codex/` hooks from the repository under work, which
//! in Boss's threat model is attacker-controllable content.
//!
//! This crate is the gate that makes the chosen mechanism honest:
//!
//! 1. Compute Codex's `trusted_hash` over the exact hook identity Codex uses
//!    (event + matcher + normalized command handler).
//! 2. Hash the guard **executable contents** Boss is about to arm (Codex's
//!    own hash does not cover file bytes — only the command string).
//! 3. Stamp `[hooks.state]` into the worker's `config.toml`.
//! 4. **Observe** arming via `codex app-server` `hooks/list` — every required
//!    hook must report `trustStatus = trusted`. Silence, missing hooks, or
//!    `untrusted`/`modified` refuse the worker.
//!
//! Missing, stale, or unobservable attestation → refuse. Silence is not
//! success.
//!
//! # Trusted-hash inputs (codex-cli 0.145.0, verified live)
//!
//! Codex computes `current_hash` in `command_hook_hash` as:
//!
//! ```text
//! NormalizedHookIdentity {
//!   event_name: <snake_case event label>,  // e.g. "pre_tool_use"
//!   matcher: Option<String>,               // omitted when None
//!   hooks: [ HookHandlerConfig::Command {
//!     type: "command",
//!     command: <string as configured>,
//!     timeout: <u64, default 600 for non-SessionEnd>,
//!     async: <bool>,
//!     // statusMessage / additionalContextLimit omitted when None
//!     // commandWindows forced to None before hashing
//!   } ]
//! }
//! → serialize to TOML value → JSON → canonical (sorted-key) JSON
//! → sha256 hex → "sha256:{hex}"
//! ```
//!
//! The state key is `{absolute_config_path}:{event_label}:{group_idx}:{handler_idx}`.
//! Paths must be the same absolute form Codex resolves (on macOS, the
//! realpath under `/private/...`).
//!
//! Evidence: `tools/boss/docs/investigations/codex-hook-trust-provisioning-2026-07-26.md`.
//!
//! # What this crate does *not* do
//!
//! - Write the hook *definitions* themselves (that is `CodexDriver` spawn
//!   provisioning). Callers write `[[hooks.*]]` first, then call the gate.
//! - Use `--dangerously-bypass-hook-trust`. Ever.
//! - Claim a hook *ran* during the worker turn — that remains a runtime
//!   concern (SessionStart marker / deny path). The gate proves the trust
//!   record is armed so Codex *will* invoke the configured handlers.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use sha2::{Digest, Sha256};

#[cfg(test)]
mod tests;

// ── Hook event labels (match codex_hooks::hook_event_key_label) ─────────────

/// Codex hook event names Boss may wire. Labels match the snake_case keys
/// Codex uses in `trusted_hash` identity and state keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PreCompact,
    PostCompact,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    SubagentStart,
    SubagentStop,
    Stop,
}

impl HookEvent {
    /// Snake-case label used in hash identity and state keys.
    pub fn key_label(self) -> &'static str {
        match self {
            Self::PreToolUse => "pre_tool_use",
            Self::PermissionRequest => "permission_request",
            Self::PostToolUse => "post_tool_use",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::SubagentStart => "subagent_start",
            Self::SubagentStop => "subagent_stop",
            Self::Stop => "stop",
        }
    }

    /// Default command-hook timeout Codex applies during discovery.
    ///
    /// SessionEnd defaults to 1s (capped at 3s); every other event defaults
    /// to 600s. The normalized identity always includes an explicit timeout,
    /// so stamping must use the same default Codex will when the config
    /// omits `timeout`.
    pub fn default_timeout_sec(self) -> u64 {
        match self {
            Self::SessionEnd => 1,
            _ => 600,
        }
    }
}

// ── Spec / request ──────────────────────────────────────────────────────────

/// One command hook Boss intends to arm for a Codex worker run.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct CommandHookSpec {
    pub event: HookEvent,
    /// Tool matcher (e.g. `".*"` for PreToolUse). `None` omits the field
    /// from the hash identity, matching Codex when the config has no matcher.
    pub matcher: Option<String>,
    /// Absolute path of the command Codex will execute. Must resolve to the
    /// same realpath that ends up in the worker `config.toml`.
    pub command: PathBuf,
    /// Explicit timeout. `None` uses [`HookEvent::default_timeout_sec`].
    pub timeout_sec: Option<u64>,
    #[builder(default)]
    pub async_hook: bool,
    /// Indices in the TOML `[[hooks.<Event>]]` / `hooks` arrays — part of the
    /// state key Codex looks up.
    #[builder(default)]
    pub group_index: usize,
    #[builder(default)]
    pub handler_index: usize,
    /// When true (guardrail hooks), the command path must exist, be a regular
    /// file, and be executable; its content SHA-256 is bound into the
    /// attestation. SessionStart arming probes may set this false if they
    /// only need trust status, not content binding.
    #[builder(default = true)]
    pub require_guard_executable: bool,
}

/// Inputs for one per-run arming attempt.
#[derive(Debug, Clone)]
pub struct ArmRequest {
    /// Per-worker `CODEX_HOME`. Must already contain `config.toml` with the
    /// hook *definitions* written; this gate only stamps `[hooks.state]`.
    pub codex_home: PathBuf,
    /// Absolute path of the user-layer config file that declares the hooks
    /// (almost always `{codex_home}/config.toml`). Used as the state-key
    /// prefix — must match the path Codex resolves for that layer.
    pub config_path: PathBuf,
    /// Working directory for `hooks/list` observation (a git repo; Codex
    /// discovers project layers relative to cwd).
    pub cwd: PathBuf,
    pub hooks: Vec<CommandHookSpec>,
    /// `codex` binary used for live observation. Never invoked with
    /// `--dangerously-bypass-hook-trust`.
    pub codex_bin: PathBuf,
}

// ── Attestation ─────────────────────────────────────────────────────────────

/// Per-hook row inside a successful attestation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct HookAttestationEntry {
    /// Codex state key: `{config_path}:{event}:{group}:{handler}`.
    pub key: String,
    pub event: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// Value stamped into `[hooks.state."<key>"].trusted_hash`.
    pub trusted_hash: String,
    /// SHA-256 of the guard executable bytes (`sha256:{hex}`), when
    /// `require_guard_executable` was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard_content_sha256: Option<String>,
    /// Live `trustStatus` observed after stamping (must be `trusted`).
    pub observed_trust_status: String,
}

/// How arming was proven — not inferred from silence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationProof {
    /// `hooks/list` returned an entry for every required hook with
    /// `trustStatus = "trusted"` and `currentHash` equal to the stamped hash.
    HooksList {
        /// codex-cli version string when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_version: Option<String>,
    },
}

/// Proof that Codex hook guardrails are armed for this run.
///
/// Serialize to disk next to the run for audit; re-check with
/// [`verify_attestation`] before treating a prior arming as still valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookTrustAttestation {
    pub codex_home: String,
    pub config_path: String,
    pub generated_at_unix: u64,
    pub hooks: Vec<HookAttestationEntry>,
    pub observation: ObservationProof,
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// Why the per-run gate refused to arm (and therefore refused the worker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustGateError {
    /// `config.toml` is missing or unreadable.
    ConfigMissing { path: PathBuf, detail: String },
    /// No hooks were supplied — arming nothing is not success.
    NoHooksConfigured,
    /// Guard executable path does not exist or is not a file.
    GuardExecutableMissing { path: PathBuf },
    /// Guard path exists but is not executable by the current user.
    GuardExecutableNotExecutable { path: PathBuf },
    /// Failed to read or hash guard bytes.
    GuardExecutableUnreadable { path: PathBuf, detail: String },
    /// Failed to write `[hooks.state]` back to config.
    ConfigWriteFailed { path: PathBuf, detail: String },
    /// Live observation could not be obtained (spawn failure, RPC error,
    /// empty response). Silence is not success.
    ObservationFailed { detail: String },
    /// A required hook was absent from `hooks/list` after stamping.
    HookNotListed { key: String },
    /// `hooks/list` reported a trust status other than `trusted`.
    HookNotTrusted {
        key: String,
        status: String,
        current_hash: String,
        stamped_hash: String,
    },
    /// Observed `currentHash` disagrees with the hash we stamped.
    HashMismatch {
        key: String,
        stamped: String,
        observed: String,
    },
    /// Attestation re-check failed: content or config hash went stale.
    AttestationStale { detail: String },
    /// Attestation re-check failed: required entry missing.
    AttestationIncomplete { detail: String },
}

impl std::fmt::Display for TrustGateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigMissing { path, detail } => {
                write!(
                    f,
                    "Codex hook-trust gate: config missing or unreadable at {}: {detail}",
                    path.display()
                )
            }
            Self::NoHooksConfigured => {
                write!(
                    f,
                    "Codex hook-trust gate: no hooks configured — refusing to dispatch \
                     (arming nothing is not success)"
                )
            }
            Self::GuardExecutableMissing { path } => {
                write!(
                    f,
                    "Codex hook-trust gate: guard executable missing at {} — refusing worker",
                    path.display()
                )
            }
            Self::GuardExecutableNotExecutable { path } => {
                write!(
                    f,
                    "Codex hook-trust gate: guard at {} is not executable — refusing worker",
                    path.display()
                )
            }
            Self::GuardExecutableUnreadable { path, detail } => {
                write!(
                    f,
                    "Codex hook-trust gate: cannot read guard at {}: {detail}",
                    path.display()
                )
            }
            Self::ConfigWriteFailed { path, detail } => {
                write!(
                    f,
                    "Codex hook-trust gate: failed to stamp trust into {}: {detail}",
                    path.display()
                )
            }
            Self::ObservationFailed { detail } => {
                write!(
                    f,
                    "Codex hook-trust gate: could not observe hook trust status ({detail}) — \
                     silence is not success; refusing worker"
                )
            }
            Self::HookNotListed { key } => {
                write!(
                    f,
                    "Codex hook-trust gate: required hook key `{key}` not listed by hooks/list \
                     after stamping — refusing worker"
                )
            }
            Self::HookNotTrusted {
                key,
                status,
                current_hash,
                stamped_hash,
            } => {
                write!(
                    f,
                    "Codex hook-trust gate: hook `{key}` trustStatus={status} \
                     (currentHash={current_hash}, stamped={stamped_hash}) — refusing worker"
                )
            }
            Self::HashMismatch { key, stamped, observed } => {
                write!(
                    f,
                    "Codex hook-trust gate: hash mismatch for `{key}`: stamped={stamped} \
                     observed={observed} — refusing worker"
                )
            }
            Self::AttestationStale { detail } => {
                write!(f, "Codex hook-trust gate: attestation stale: {detail}")
            }
            Self::AttestationIncomplete { detail } => {
                write!(f, "Codex hook-trust gate: attestation incomplete: {detail}")
            }
        }
    }
}

impl std::error::Error for TrustGateError {}

// ── Hashing (mirrors codex_config::version_for_toml + command_hook_hash) ────

/// SHA-256 of arbitrary bytes as `sha256:{lowercase_hex}`.
pub fn sha256_hex_prefixed(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!("sha256:{hex}")
}

/// Canonical-JSON SHA-256 used by Codex for config fingerprints and hook trust.
///
/// Object keys are sorted recursively; the serialized form uses compact
/// separators and no leading/trailing whitespace — matching
/// `serde_json::to_vec` over the sorted tree.
pub fn version_for_json(value: &JsonValue) -> String {
    let canonical = canonical_json(value);
    let serialized = serde_json::to_vec(&canonical).unwrap_or_default();
    sha256_hex_prefixed(&serialized)
}

fn canonical_json(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(map) => {
            let mut sorted = JsonMap::new();
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            for key in keys {
                if let Some(val) = map.get(&key) {
                    sorted.insert(key, canonical_json(val));
                }
            }
            JsonValue::Object(sorted)
        }
        JsonValue::Array(items) => JsonValue::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

/// Compute Codex's `current_hash` / `trusted_hash` for one command hook.
///
/// `command` must be the exact string that will appear in config (resolved
/// absolute path). `timeout_sec` must be the timeout Codex will normalize to
/// (use [`HookEvent::default_timeout_sec`] when the config omits it).
pub fn command_hook_trusted_hash(
    event: HookEvent,
    command: &str,
    matcher: Option<&str>,
    timeout_sec: u64,
    async_hook: bool,
) -> String {
    // Shape matches NormalizedHookIdentity + HookHandlerConfig::Command after
    // Codex forces command_windows = None and drops default additionalContextLimit.
    let mut handler = JsonMap::new();
    handler.insert("type".into(), JsonValue::String("command".into()));
    handler.insert("command".into(), JsonValue::String(command.into()));
    handler.insert("timeout".into(), JsonValue::Number(timeout_sec.into()));
    handler.insert("async".into(), JsonValue::Bool(async_hook));

    let mut identity = JsonMap::new();
    identity.insert("event_name".into(), JsonValue::String(event.key_label().into()));
    if let Some(m) = matcher {
        identity.insert("matcher".into(), JsonValue::String(m.into()));
    }
    identity.insert("hooks".into(), JsonValue::Array(vec![JsonValue::Object(handler)]));

    version_for_json(&JsonValue::Object(identity))
}

/// Build the `[hooks.state]` key Codex looks up for a handler.
///
/// `config_path` must be the absolute path string of the layer file (same
/// realpath Codex reports as `sourcePath` / key prefix).
pub fn hook_state_key(config_path: &Path, event: HookEvent, group_index: usize, handler_index: usize) -> String {
    format!(
        "{}:{}:{group_index}:{handler_index}",
        config_path.display(),
        event.key_label()
    )
}

// ── Path / guard helpers ────────────────────────────────────────────────────

/// Resolve `path` to the absolute form Codex uses in keys and command strings.
///
/// Prefer `canonicalize` when the path exists so macOS `/var` → `/private/var`
/// matches Codex's `AbsolutePathBuf` resolution.
pub fn resolve_absolute(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn content_sha256_of_file(path: &Path) -> Result<String, TrustGateError> {
    let bytes = fs::read(path).map_err(|err| TrustGateError::GuardExecutableUnreadable {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    Ok(sha256_hex_prefixed(&bytes))
}

fn ensure_guard_executable(path: &Path) -> Result<String, TrustGateError> {
    let meta = fs::metadata(path).map_err(|_| TrustGateError::GuardExecutableMissing {
        path: path.to_path_buf(),
    })?;
    if !meta.is_file() {
        return Err(TrustGateError::GuardExecutableMissing {
            path: path.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o111 == 0 {
            return Err(TrustGateError::GuardExecutableNotExecutable {
                path: path.to_path_buf(),
            });
        }
    }
    content_sha256_of_file(path)
}

// ── Config stamping ─────────────────────────────────────────────────────────

/// Stamp `trusted_hash` entries into `config_path`'s `[hooks.state]` tables.
///
/// Surgical text edit (not full re-serialize): Boss-owned per-run configs use
/// `[[hooks.*]]` array-of-tables grammar that must stay byte-stable for the
/// command strings already written; a round-trip through `toml::Value` is
/// unnecessary and fragile for that shape.
///
/// Does not observe arming and does not validate guard executables — use
/// [`arm_and_attest`] for the full gate. Returns the hashes that were written.
pub fn stamp_hook_trust(
    config_path: &Path,
    hooks: &[CommandHookSpec],
) -> Result<Vec<(String, String)>, TrustGateError> {
    if hooks.is_empty() {
        return Err(TrustGateError::NoHooksConfigured);
    }

    let config_path = resolve_absolute(config_path);
    let raw = fs::read_to_string(&config_path).map_err(|err| TrustGateError::ConfigMissing {
        path: config_path.clone(),
        detail: err.to_string(),
    })?;

    let mut stamped = Vec::with_capacity(hooks.len());
    for hook in hooks {
        let command = resolve_absolute(&hook.command);
        let command_str = command.to_string_lossy().into_owned();
        let timeout = hook.timeout_sec.unwrap_or_else(|| hook.event.default_timeout_sec());
        let hash = command_hook_trusted_hash(
            hook.event,
            &command_str,
            hook.matcher.as_deref(),
            timeout,
            hook.async_hook,
        );
        let key = hook_state_key(&config_path, hook.event, hook.group_index, hook.handler_index);
        stamped.push((key, hash));
    }

    let without_state = strip_hooks_state_tables(&raw);
    let mut out = without_state;
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out.push('\n');
    for (key, hash) in &stamped {
        // Quote the key — it always contains path separators and colons.
        let escaped = key.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!("[hooks.state.\"{escaped}\"]\n"));
        out.push_str(&format!("trusted_hash = \"{hash}\"\n\n"));
    }

    fs::write(&config_path, out).map_err(|err| TrustGateError::ConfigWriteFailed {
        path: config_path.clone(),
        detail: err.to_string(),
    })?;

    Ok(stamped)
}

/// Drop any existing `[hooks.state]` / `[hooks.state."…"]` tables so a re-stamp
/// replaces rather than duplicates.
fn strip_hooks_state_tables(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut skipping = false;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            // Enter skip mode for any hooks.state table header.
            let is_state = trimmed.starts_with("[hooks.state.")
                || trimmed.starts_with("[hooks.state]")
                || trimmed.starts_with("[hooks.state ");
            skipping = is_state;
            if skipping {
                continue;
            }
        }
        if skipping {
            // Skip body lines of the state table (assignments / blanks until
            // the next header). Blank lines inside the table are skipped too.
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.contains('=') {
                continue;
            }
            // Unknown content — stop skipping and keep the line.
            skipping = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ── Observation ─────────────────────────────────────────────────────────────

/// One hook row from a live `hooks/list` observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedHook {
    pub key: String,
    pub trust_status: String,
    pub current_hash: String,
    pub enabled: bool,
}

/// Pluggable observer so unit tests can prove refuse paths without spawning
/// Codex. Production uses [`CodexAppServerObserver`].
pub trait TrustObserver {
    fn observe_hooks(&self, codex_home: &Path, cwd: &Path) -> Result<Vec<ObservedHook>, TrustGateError>;
}

/// Live observer: `codex app-server` over stdio, `hooks/list` RPC.
///
/// Never passes `--dangerously-bypass-hook-trust`.
#[derive(Debug, Clone)]
pub struct CodexAppServerObserver {
    pub codex_bin: PathBuf,
}

impl TrustObserver for CodexAppServerObserver {
    fn observe_hooks(&self, codex_home: &Path, cwd: &Path) -> Result<Vec<ObservedHook>, TrustGateError> {
        let mut child = Command::new(&self.codex_bin)
            .arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .current_dir(cwd)
            .env("CODEX_HOME", codex_home)
            // Explicitly do NOT set any bypass-hook-trust flag/env.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| TrustGateError::ObservationFailed {
                detail: format!("failed to spawn `{} app-server`: {err}", self.codex_bin.display()),
            })?;

        let mut stdin = child.stdin.take().ok_or_else(|| TrustGateError::ObservationFailed {
            detail: "app-server stdin not piped".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| TrustGateError::ObservationFailed {
            detail: "app-server stdout not piped".into(),
        })?;
        let mut reader = BufReader::new(stdout);

        let write_msg = |stdin: &mut std::process::ChildStdin, msg: &JsonValue| {
            let line = serde_json::to_string(msg).map_err(|err| TrustGateError::ObservationFailed {
                detail: format!("serialize RPC: {err}"),
            })?;
            writeln!(stdin, "{line}").map_err(|err| TrustGateError::ObservationFailed {
                detail: format!("write RPC: {err}"),
            })?;
            stdin.flush().map_err(|err| TrustGateError::ObservationFailed {
                detail: format!("flush RPC: {err}"),
            })?;
            Ok::<(), TrustGateError>(())
        };

        let read_until_id =
            |reader: &mut BufReader<std::process::ChildStdout>, want_id: u64| -> Result<JsonValue, TrustGateError> {
                // Bound the read so a hung app-server cannot wedge dispatch forever.
                // app-server is local and typically answers in milliseconds.
                let mut line = String::new();
                for _ in 0..200 {
                    line.clear();
                    let n = reader
                        .read_line(&mut line)
                        .map_err(|err| TrustGateError::ObservationFailed {
                            detail: format!("read RPC: {err}"),
                        })?;
                    if n == 0 {
                        return Err(TrustGateError::ObservationFailed {
                            detail: "app-server closed stdout before responding".into(),
                        });
                    }
                    let Ok(val) = serde_json::from_str::<JsonValue>(line.trim()) else {
                        continue;
                    };
                    if val.get("id").and_then(|v| v.as_u64()) == Some(want_id) {
                        return Ok(val);
                    }
                }
                Err(TrustGateError::ObservationFailed {
                    detail: format!("no response for RPC id={want_id} within read bound"),
                })
            };

        write_msg(
            &mut stdin,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": { "name": "boss-codex-hook-trust", "version": "0" },
                    "capabilities": {}
                }
            }),
        )?;
        let _init = read_until_id(&mut reader, 1)?;

        write_msg(
            &mut stdin,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
        )?;

        write_msg(
            &mut stdin,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "hooks/list",
                "params": {}
            }),
        )?;
        let list_resp = read_until_id(&mut reader, 2)?;

        // Best-effort shutdown; ignore failures.
        let _ = child.kill();
        let _ = child.wait();

        parse_hooks_list_response(&list_resp)
    }
}

fn parse_hooks_list_response(resp: &JsonValue) -> Result<Vec<ObservedHook>, TrustGateError> {
    let result = resp.get("result").ok_or_else(|| TrustGateError::ObservationFailed {
        detail: format!("hooks/list response missing result: {resp}"),
    })?;
    // Shape: { "data": [ { "cwd": "...", "hooks": [ {...}, ... ] } ] }
    let data = result
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| TrustGateError::ObservationFailed {
            detail: "hooks/list result.data missing or not an array".into(),
        })?;
    if data.is_empty() {
        return Err(TrustGateError::ObservationFailed {
            detail: "hooks/list returned empty data — silence is not success".into(),
        });
    }
    let mut out = Vec::new();
    for group in data {
        let hooks = group
            .get("hooks")
            .and_then(|h| h.as_array())
            .ok_or_else(|| TrustGateError::ObservationFailed {
                detail: "hooks/list group missing hooks array".into(),
            })?;
        for h in hooks {
            let key = h
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| TrustGateError::ObservationFailed {
                    detail: "hook entry missing key".into(),
                })?
                .to_string();
            let trust_status = h
                .get("trustStatus")
                .or_else(|| h.get("trust_status"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let current_hash = h
                .get("currentHash")
                .or_else(|| h.get("current_hash"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let enabled = h.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            if trust_status.is_empty() || current_hash.is_empty() {
                return Err(TrustGateError::ObservationFailed {
                    detail: format!("hook `{key}` missing trustStatus/currentHash — unobservable arming"),
                });
            }
            out.push(ObservedHook {
                key,
                trust_status,
                current_hash,
                enabled,
            });
        }
    }
    if out.is_empty() {
        return Err(TrustGateError::ObservationFailed {
            detail: "hooks/list returned no hook entries — silence is not success".into(),
        });
    }
    Ok(out)
}

// ── Full gate ───────────────────────────────────────────────────────────────

/// Arm Codex hook trust for this run and produce an attestation.
///
/// Steps (fail-closed at each):
/// 1. Refuse if `hooks` is empty.
/// 2. For every `require_guard_executable` hook: require a regular executable
///    file; bind its content SHA-256 into the attestation.
/// 3. Stamp `[hooks.state.<key>].trusted_hash` for every hook.
/// 4. Observe via `observer` (`hooks/list`); every required key must report
///    `trustStatus = "trusted"` with `currentHash` equal to the stamped hash.
/// 5. Return a serializable [`HookTrustAttestation`].
///
/// Does **not** pass `--dangerously-bypass-hook-trust`.
pub fn arm_and_attest_with_observer<O: TrustObserver>(
    request: &ArmRequest,
    observer: &O,
) -> Result<HookTrustAttestation, TrustGateError> {
    if request.hooks.is_empty() {
        return Err(TrustGateError::NoHooksConfigured);
    }

    let config_path = resolve_absolute(&request.config_path);
    let codex_home = resolve_absolute(&request.codex_home);

    // 1–2. Validate guards and capture content hashes up front.
    let mut content_hashes: BTreeMap<String, Option<String>> = BTreeMap::new();
    for hook in &request.hooks {
        let command = resolve_absolute(&hook.command);
        let key = hook_state_key(&config_path, hook.event, hook.group_index, hook.handler_index);
        let content = if hook.require_guard_executable {
            Some(ensure_guard_executable(&command)?)
        } else {
            // Still require the command path to exist when it is a filesystem
            // path — a missing binary is the second silent fail-open mode.
            if !command.exists() {
                return Err(TrustGateError::GuardExecutableMissing { path: command });
            }
            None
        };
        content_hashes.insert(key, content);
    }

    // 3. Stamp trust.
    let stamped = stamp_hook_trust(&config_path, &request.hooks)?;
    let stamped_map: BTreeMap<String, String> = stamped.into_iter().collect();

    // 4. Observe — silence is not success.
    let observed = observer.observe_hooks(&codex_home, &request.cwd)?;
    let observed_map: BTreeMap<String, ObservedHook> = observed.into_iter().map(|h| (h.key.clone(), h)).collect();

    let mut entries = Vec::with_capacity(request.hooks.len());
    for hook in &request.hooks {
        let command = resolve_absolute(&hook.command);
        let command_str = command.to_string_lossy().into_owned();
        let key = hook_state_key(&config_path, hook.event, hook.group_index, hook.handler_index);
        let stamped_hash = stamped_map
            .get(&key)
            .ok_or_else(|| TrustGateError::AttestationIncomplete {
                detail: format!("stamped map missing key `{key}`"),
            })?;
        let obs = observed_map
            .get(&key)
            .ok_or_else(|| TrustGateError::HookNotListed { key: key.clone() })?;

        if !obs.trust_status.eq_ignore_ascii_case("trusted") {
            return Err(TrustGateError::HookNotTrusted {
                key: key.clone(),
                status: obs.trust_status.clone(),
                current_hash: obs.current_hash.clone(),
                stamped_hash: stamped_hash.clone(),
            });
        }
        if obs.current_hash != *stamped_hash {
            return Err(TrustGateError::HashMismatch {
                key: key.clone(),
                stamped: stamped_hash.clone(),
                observed: obs.current_hash.clone(),
            });
        }

        entries.push(HookAttestationEntry {
            key: key.clone(),
            event: hook.event.key_label().to_string(),
            command: command_str,
            matcher: hook.matcher.clone(),
            trusted_hash: stamped_hash.clone(),
            guard_content_sha256: content_hashes.get(&key).cloned().flatten(),
            observed_trust_status: obs.trust_status.clone(),
        });
    }

    let generated_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(HookTrustAttestation {
        codex_home: codex_home.to_string_lossy().into_owned(),
        config_path: config_path.to_string_lossy().into_owned(),
        generated_at_unix,
        hooks: entries,
        observation: ObservationProof::HooksList { codex_version: None },
    })
}

/// Production entry point: arm trust and attest via `codex app-server`.
pub fn arm_and_attest(request: &ArmRequest) -> Result<HookTrustAttestation, TrustGateError> {
    let observer = CodexAppServerObserver {
        codex_bin: request.codex_bin.clone(),
    };
    arm_and_attest_with_observer(request, &observer)
}

/// Re-validate an attestation against current disk state without re-observing
/// Codex. Detects stale guard content or a config whose stamped hashes no
/// longer match the hook identity.
///
/// Does not call Codex; pair with a fresh [`arm_and_attest`] when live
/// observation is required again.
pub fn verify_attestation(attestation: &HookTrustAttestation, hooks: &[CommandHookSpec]) -> Result<(), TrustGateError> {
    if hooks.is_empty() {
        return Err(TrustGateError::NoHooksConfigured);
    }
    if attestation.hooks.is_empty() {
        return Err(TrustGateError::AttestationIncomplete {
            detail: "attestation carries no hook entries".into(),
        });
    }

    let config_path = resolve_absolute(Path::new(&attestation.config_path));
    let config_raw = fs::read_to_string(&config_path).map_err(|err| TrustGateError::AttestationStale {
        detail: format!("config unreadable at {}: {err}", config_path.display()),
    })?;

    let att_by_key: BTreeMap<&str, &HookAttestationEntry> =
        attestation.hooks.iter().map(|e| (e.key.as_str(), e)).collect();

    for hook in hooks {
        let command = resolve_absolute(&hook.command);
        let command_str = command.to_string_lossy().into_owned();
        let timeout = hook.timeout_sec.unwrap_or_else(|| hook.event.default_timeout_sec());
        let expected_hash = command_hook_trusted_hash(
            hook.event,
            &command_str,
            hook.matcher.as_deref(),
            timeout,
            hook.async_hook,
        );
        let key = hook_state_key(&config_path, hook.event, hook.group_index, hook.handler_index);

        let entry = att_by_key
            .get(key.as_str())
            .ok_or_else(|| TrustGateError::AttestationIncomplete {
                detail: format!("attestation missing key `{key}`"),
            })?;

        if entry.trusted_hash != expected_hash {
            return Err(TrustGateError::AttestationStale {
                detail: format!(
                    "hook identity for `{key}` changed: attestation={} recomputed={}",
                    entry.trusted_hash, expected_hash
                ),
            });
        }

        // Config must still carry the stamped hash (text scan; config is
        // Boss-owned and may use array-of-tables grammar elsewhere).
        match trusted_hash_in_config(&config_raw, &key) {
            Some(h) if h == entry.trusted_hash => {}
            Some(h) => {
                return Err(TrustGateError::AttestationStale {
                    detail: format!(
                        "config trusted_hash for `{key}` is {h}, attestation has {}",
                        entry.trusted_hash
                    ),
                });
            }
            None => {
                return Err(TrustGateError::AttestationStale {
                    detail: format!("config missing hooks.state.\"{key}\".trusted_hash"),
                });
            }
        }

        if hook.require_guard_executable {
            let live = content_sha256_of_file(&command)?;
            match &entry.guard_content_sha256 {
                Some(bound) if bound == &live => {}
                Some(bound) => {
                    return Err(TrustGateError::AttestationStale {
                        detail: format!(
                            "guard content changed for `{}`: attestation={bound} live={live}",
                            command.display()
                        ),
                    });
                }
                None => {
                    return Err(TrustGateError::AttestationIncomplete {
                        detail: format!("attestation missing guard content hash for `{}`", command.display()),
                    });
                }
            }
        }
    }

    Ok(())
}

/// Locate `trusted_hash = "…"` under the `[hooks.state."<key>"]` table header.
fn trusted_hash_in_config(config_raw: &str, key: &str) -> Option<String> {
    let escaped = key.replace('\\', "\\\\").replace('"', "\\\"");
    let header = format!("[hooks.state.\"{escaped}\"]");
    let mut lines = config_raw.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() != header {
            continue;
        }
        for body in lines.by_ref() {
            let t = body.trim();
            if t.starts_with('[') {
                break;
            }
            if let Some(rest) = t.strip_prefix("trusted_hash") {
                let rest = rest.trim_start();
                let rest = rest.strip_prefix('=')?.trim_start();
                let rest = rest.strip_prefix('"')?.strip_suffix('"')?;
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// Write the attestation as pretty JSON next to the run (audit trail).
pub fn write_attestation_file(path: &Path, attestation: &HookTrustAttestation) -> Result<(), TrustGateError> {
    let json = serde_json::to_string_pretty(attestation).map_err(|err| TrustGateError::ConfigWriteFailed {
        path: path.to_path_buf(),
        detail: format!("serialize attestation: {err}"),
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| TrustGateError::ConfigWriteFailed {
            path: path.to_path_buf(),
            detail: format!("create parent: {err}"),
        })?;
    }
    fs::write(path, json).map_err(|err| TrustGateError::ConfigWriteFailed {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })
}
