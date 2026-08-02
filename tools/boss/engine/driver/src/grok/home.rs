//! Boss-owned per-run `GROK_HOME` layout and pre-spawn posture checks.
//!
//! Parallel to Codex's `CODEX_HOME` isolation: never point a worker at the
//! operator's interactive `~/.grok`. Auth is a **symlink** (not a copy) of the
//! host credential. Permission isolation (T-01 findings) additionally scopes
//! the worker process `HOME` so `~/.claude/settings.local.json` cannot load.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// Env override for the root under which per-run `GROK_HOME` directories live.
/// Tests set this so homes land in a disposable temp tree.
pub const GROK_HOMES_ROOT_ENV: &str = "BOSS_GROK_HOMES_DIR";

/// Env override for the auth.json *source* path (symlink target). Tests point
/// this at a throwaway file so the interactive `~/.grok/auth.json` is never
/// required.
pub const GROK_AUTH_SOURCE_ENV: &str = "BOSS_GROK_AUTH_SOURCE";

/// When set to `1`/`true`, skip the live `grok inspect --json` assertion.
/// Production never sets this; unit tests that only check file layout may.
pub const GROK_SKIP_POSTURE_ASSERT_ENV: &str = "BOSS_GROK_SKIP_POSTURE_ASSERT";

/// Process-global lock for tests that mutate [`GROK_HOMES_ROOT_ENV`] /
/// [`GROK_AUTH_SOURCE_ENV`] / [`GROK_SKIP_POSTURE_ASSERT_ENV`].
pub static GROK_HOMES_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Default leaf under the system temp when [`GROK_HOMES_ROOT_ENV`] is unset.
const GROK_HOMES_DIR_NAME: &str = "boss-grok-homes";

/// Most recent Grok CLI version actually characterised by the design +
/// investigations. Documentation only — Grok auto-updates itself on its own
/// schedule, so `assert_inspect_json_posture` does not gate on this: a
/// mismatch only logs a `tracing::warn!` (see there). Update this value (and
/// re-run `--trust` / `grok models` / `grok inspect --json` posture
/// by hand) after actually re-characterising against a newer release —
/// bumping it without re-characterising just relabels the drift as
/// "known".
const LAST_CHARACTERISED_GROK_VERSION: &str = "0.2.114";

/// Filename of the Boss-assigned session UUID under `GROK_HOME`.
const SESSION_ID_FILENAME: &str = "boss-session-id";

/// Filename of the absolute workspace path stamped at provision so
/// `spawn_invocation` can pass `--cwd` without a workspace field on
/// [`crate::SpawnRequest`].
const WORKSPACE_PATH_FILENAME: &str = "boss-workspace-path";

/// Leaf name of the scoped process `HOME` under the run container.
const PROCESS_HOME_LEAF: &str = "process-home";

/// Leaf name of the actual `GROK_HOME` under the run container.
const GROK_HOME_LEAF: &str = "grok-home";

/// Filename under `$GROK_HOME/hooks/` for Boss's global hook wiring.
///
/// `provision_grok_home` writes a no-op canary here (a single `SessionStart`
/// hook that just runs `true`) so `grok inspect --json` reports a non-empty
/// hooks inventory before the real wiring exists. [`GrokDriver::write_permission_config`]
/// (`super::super::hooks::write_hooks`) overwrites the same file with the
/// real boss-event forwarder + adapter-wrapped `PreToolUse` guards once guard
/// scripts are materialised — never both a stale canary and the real file at
/// once.
const HOOKS_FILENAME: &str = "boss-provision.json";

/// Absolute path of the hooks-wiring file under `grok_home`. Shared between
/// the provisional canary write here and the real wiring
/// [`super::hooks::write_hooks`] writes once `write_permission_config` runs.
pub(super) fn hooks_file_path(grok_home: &Path) -> PathBuf {
    grok_home.join("hooks").join(HOOKS_FILENAME)
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Root directory that holds Boss-owned per-run Grok containers.
///
/// Prefer [`GROK_HOMES_ROOT_ENV`] when set (tests); otherwise
/// `$TMPDIR/boss-grok-homes`. Never the operator interactive `~/.grok`.
pub fn grok_homes_root() -> PathBuf {
    match std::env::var_os(GROK_HOMES_ROOT_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir().join(GROK_HOMES_DIR_NAME),
    }
}

/// Sanitize `run_id` to a single path segment under the homes root.
pub fn sanitize_run_id_for_home(run_id: &str) -> anyhow::Result<String> {
    if run_id.is_empty() {
        bail!("empty run_id refused for Boss-owned GROK_HOME");
    }
    let safe: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        bail!("run_id {run_id:?} sanitized to empty; refused for Boss-owned GROK_HOME");
    }
    Ok(safe)
}

/// Per-run container: holds `grok-home/` (`GROK_HOME`) and `process-home/` (`HOME`).
pub fn grok_run_container_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    let safe = sanitize_run_id_for_home(run_id)?;
    // Resolve the root exactly once, then check containment against that same
    // value — see the equivalent note in `codex::codex_home_for_run`. Two
    // separate reads let a concurrent `GROK_HOMES_ROOT_ENV` change (tests
    // only) fail the check for a valid run id.
    let root = grok_homes_root();
    let container = root.join(safe);
    if !container.starts_with(&root) || container == root {
        bail!(
            "resolved Grok run container {} is not a strict child of homes root {}",
            container.display(),
            root.display()
        );
    }
    Ok(container)
}

/// Absolute path of the Boss-owned per-run `GROK_HOME` for `run_id`.
///
/// Deterministic so [`super::GrokDriver::spawn_invocation`] and
/// [`super::GrokDriver::provision_workspace`] agree without threading the path
/// through [`crate::SpawnRequest`]. Never points at the interactive Grok home.
pub fn grok_home_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    Ok(grok_run_container_for_run(run_id)?.join(GROK_HOME_LEAF))
}

/// Scoped process `HOME` for the worker (empty `~/.claude` tree).
///
/// T-01: isolated `GROK_HOME` alone does not drop operator Claude permission
/// sources; scoping `HOME` does.
pub fn process_home_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    Ok(grok_run_container_for_run(run_id)?.join(PROCESS_HOME_LEAF))
}

/// Host credential path used as the `auth.json` symlink target.
pub fn resolve_grok_auth_source() -> PathBuf {
    if let Ok(path) = std::env::var(GROK_AUTH_SOURCE_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(".grok").join("auth.json"),
        _ => PathBuf::from(".grok").join("auth.json"),
    }
}

/// Default leaf (relative to a real `HOME`) of `gh`'s config directory, used
/// when neither `GH_CONFIG_DIR` nor `XDG_CONFIG_HOME` is set on the host —
/// mirrors `gh`'s own resolution order (`gh help environment`).
const GH_DEFAULT_CONFIG_LEAF: &str = ".config/gh";

/// macOS login keychain leaf, relative to a real `HOME`. `gh auth login`
/// stores the actual OAuth token here via the OS keyring, not in
/// `~/.config/gh/hosts.yml` (which holds only non-secret metadata — host,
/// username, git protocol). Keychain Services resolves this path from
/// `$HOME`, so it goes dark under a scoped `HOME` the same way `~/.claude`
/// does — confirmed empirically (`security find-generic-password` fails
/// under a scoped `HOME` and succeeds once this file is bridged in).
const LOGIN_KEYCHAIN_LEAF: &str = "Library/Keychains/login.keychain-db";

/// Resolve the real host's `gh` config directory, honoring the exact
/// precedence `gh` itself documents (`GH_CONFIG_DIR` > `XDG_CONFIG_HOME/gh` >
/// `$HOME/.config/gh`). The worker's spawn env sets `GH_CONFIG_DIR` to this
/// value directly — `gh` honors that var natively, so no filesystem bridge
/// is needed for the config directory itself (only for the keychain-backed
/// token; see [`resolve_login_keychain_source`]).
///
/// Must be called before `HOME` is overridden in the calling process's own
/// env (i.e. from the engine process, not the spawned worker) so it resolves
/// the operator's real config, not the scoped one.
pub fn resolve_gh_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GH_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir).join("gh"));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(GH_DEFAULT_CONFIG_LEAF))
}

/// Resolve the real host's login keychain file, or `None` when it does not
/// exist — e.g. a non-macOS host, or `gh` was never authenticated there.
/// Bridging is then skipped rather than treated as fatal: unlike Grok's own
/// `auth.json` (a hard requirement for the tool to run at all), this only
/// affects `cube pr create`'s ability to authenticate later, which already
/// fails loudly on its own when `gh` has no usable credential.
fn resolve_login_keychain_source() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    let path = PathBuf::from(home).join(LOGIN_KEYCHAIN_LEAF);
    path.exists().then_some(path)
}

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

/// Opaque payload persisted on the execution as [`crate::DriverRuntimeState`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct GrokRuntimeState {
    pub grok_home: PathBuf,
    pub process_home: PathBuf,
    pub auth_source_path: PathBuf,
    pub session_id: String,
    pub workspace_path: PathBuf,
    /// `grokVersion` observed from `grok inspect --json` at provision time
    /// (`None` when the live posture assert was skipped — test-only, via
    /// [`GROK_SKIP_POSTURE_ASSERT_ENV`]). Recorded so drift is visible after
    /// the fact even though it no longer gates provisioning; see
    /// [`LAST_CHARACTERISED_GROK_VERSION`].
    pub grok_version: Option<String>,
}

impl GrokRuntimeState {
    pub fn to_driver_runtime_state(&self) -> crate::DriverRuntimeState {
        crate::DriverRuntimeState::new(serde_json::to_value(self).expect("GrokRuntimeState is serializable"))
    }

    pub fn from_driver_runtime_state(state: &crate::DriverRuntimeState) -> anyhow::Result<Self> {
        serde_json::from_value(state.as_value().clone()).context("decoding GrokRuntimeState from DriverRuntimeState")
    }
}

// ---------------------------------------------------------------------------
// Config / trust / hooks rendering
// ---------------------------------------------------------------------------

/// Official externalCompat surfaces for Claude/Cursor (T-01 matrix).
/// There is **no** `plugins` cell — inspect reports `mcps` and `sessions`.
/// Writing `plugins = false` is a no-op that left mcps/sessions at
/// `enabled: true, source: default` under live `grok inspect`.
pub const COMPAT_VENDORS: &[&str] = &["claude", "cursor"];

/// Surfaces Boss must disable and fail-closed-assert for every vendor in
/// [`COMPAT_VENDORS`]. Order matches common inspect output; set membership
/// is what matters for posture checks.
pub const COMPAT_SURFACES: &[&str] = &["hooks", "agents", "skills", "mcps", "rules", "sessions"];

/// Full `[compat.claude]` + `[compat.cursor]` disable block (design posture +
/// T-01 findings). There is no effectual `permissions = false` cell; HOME
/// scoping is the permission-isolation lever. Official cells are
/// hooks/agents/skills/mcps/rules/sessions — not `plugins`. Also pins
/// `[ui] vim_mode = false` (design T-12/T-13) so the interrupt control verb
/// (Esc) is never silently broken by fullscreen vim-scrollback mode.
pub fn render_base_config_toml() -> String {
    // Keep this byte-stable for tests and for `grok inspect` assertions.
    r#"# Boss-owned Grok config. Written every provision (idempotent overwrite).
# Compat surfaces off so reused cube workspaces that still contain
# `.claude/CLAUDE.md` / `.claude/settings.json` do not load under Grok.
# Official externalCompat cells: hooks/agents/skills/mcps/rules/sessions
# (no plugins surface — writing plugins=false is a silent no-op).

[compat.claude]
hooks = false
agents = false
skills = false
mcps = false
rules = false
sessions = false

[compat.cursor]
hooks = false
agents = false
skills = false
mcps = false
rules = false
sessions = false

[ui]
# Esc-cancel is swallowed as a no-op in fullscreen vim-scrollback mode
# (design T-12/T-13; user-guide 03-keyboard-shortcuts.md / 05-configuration.md),
# which would silently break the ControlVerbs interrupt path. `vim_mode`
# already defaults to false upstream, but Boss owns this explicitly rather
# than depend on a default that could change.
vim_mode = false
"#
    .to_owned()
}

/// Pre-seed `$GROK_HOME/trusted_folders.toml` with both `/tmp` and
/// `/private/tmp` (and `/var`/`/private/var`) forms of the workspace path and
/// its canonical root. Belt for hidden `--trust` (D-3).
pub fn render_trusted_folders_toml(workspace: &Path) -> String {
    let decided_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out = String::new();
    for path in trust_path_variants(workspace) {
        // TOML bare keys with dots/slashes need quoted table headers.
        out.push_str(&format!(
            "[folders.{path:?}]\ntrusted = true\ndecided_at = {decided_at}\n\n"
        ));
    }
    out
}

/// Absolute path forms Grok's folder-trust store may key by on macOS.
pub fn trust_path_variants(workspace: &Path) -> Vec<String> {
    let mut paths = BTreeSet::new();
    push_path_and_symlink_forms(&mut paths, workspace);
    if let Ok(canon) = fs::canonicalize(workspace) {
        push_path_and_symlink_forms(&mut paths, &canon);
    }
    paths.into_iter().collect()
}

fn push_path_and_symlink_forms(paths: &mut BTreeSet<String>, path: &Path) {
    let s = path.display().to_string();
    if s.is_empty() {
        return;
    }
    paths.insert(s.clone());
    // macOS /tmp → /private/tmp
    if let Some(rest) = s.strip_prefix("/tmp") {
        paths.insert(format!("/private/tmp{rest}"));
    }
    if let Some(rest) = s.strip_prefix("/private/tmp") {
        paths.insert(format!("/tmp{rest}"));
    }
    // macOS /var → /private/var (cube workspaces often live under /var/folders…)
    if let Some(rest) = s.strip_prefix("/var/") {
        paths.insert(format!("/private/var/{rest}"));
    }
    if let Some(rest) = s.strip_prefix("/private/var/") {
        paths.insert(format!("/var/{rest}"));
    }
}

/// Provisional global hooks so `grok inspect` reports a registered inventory
/// at `provision_workspace` time, before `write_permission_config` (which
/// runs later in the spawn flow, once guard scripts are materialised)
/// overwrites this same file with the real boss-event + guard set. No-op
/// `true` command — progress is not yet observed by the engine at this point.
fn render_provision_hooks_json() -> String {
    r#"{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "*",
        "hooks": [{"type": "command", "command": "true"}]
      }
    ]
  }
}
"#
    .to_owned()
}

// ---------------------------------------------------------------------------
// Session id
// ---------------------------------------------------------------------------

/// Generate a UUID v4 string from `/dev/urandom` (Boss hosts are Unix).
pub fn new_session_uuid() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    let mut f = fs::File::open("/dev/urandom").context("opening /dev/urandom for session UUID")?;
    f.read_exact(&mut bytes)
        .context("reading 16 bytes from /dev/urandom for session UUID")?;
    // RFC 4122 version 4 + variant 1.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

pub fn session_id_path(grok_home: &Path) -> PathBuf {
    grok_home.join(SESSION_ID_FILENAME)
}

pub fn read_session_id(grok_home: &Path) -> anyhow::Result<String> {
    let path = session_id_path(grok_home);
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let id = raw.trim();
    if id.is_empty() {
        bail!("empty session id at {}", path.display());
    }
    Ok(id.to_owned())
}

pub fn workspace_path_stamp(grok_home: &Path) -> PathBuf {
    grok_home.join(WORKSPACE_PATH_FILENAME)
}

pub fn read_workspace_path_stamp(grok_home: &Path) -> anyhow::Result<PathBuf> {
    let path = workspace_path_stamp(grok_home);
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let s = raw.trim();
    if s.is_empty() {
        bail!("empty workspace path stamp at {}", path.display());
    }
    Ok(PathBuf::from(s))
}

// ---------------------------------------------------------------------------
// Provision body
// ---------------------------------------------------------------------------

/// Create / refresh the Boss-owned per-run Grok home for `run_id`.
///
/// Idempotent: rewrites config, trust, hooks canary, auth symlink, and prompt
/// on every call so reused cube workspaces always get a compat-suppressing
/// posture.
pub fn provision_grok_home(workspace: &Path, prompt_text: &str, run_id: &str) -> anyhow::Result<GrokRuntimeState> {
    let container = grok_run_container_for_run(run_id)?;
    let grok_home = container.join(GROK_HOME_LEAF);
    let process_home = container.join(PROCESS_HOME_LEAF);

    fs::create_dir_all(&grok_home).with_context(|| format!("creating GROK_HOME {}", grok_home.display()))?;
    fs::create_dir_all(grok_home.join("hooks"))
        .with_context(|| format!("creating hooks dir under {}", grok_home.display()))?;
    fs::create_dir_all(grok_home.join("sessions"))
        .with_context(|| format!("creating sessions dir under {}", grok_home.display()))?;
    // Scoped HOME with no `.claude` tree — quarantine operator Claude settings.
    fs::create_dir_all(&process_home).with_context(|| format!("creating process HOME {}", process_home.display()))?;
    // Explicitly ensure no leftover .claude from a prior mis-provision.
    let claude_under_process = process_home.join(".claude");
    if claude_under_process.exists() {
        fs::remove_dir_all(&claude_under_process).with_context(|| {
            format!(
                "removing stale {} so Claude permission sources cannot load",
                claude_under_process.display()
            )
        })?;
    }

    // Bridge the operator's real gh credential store into process-home so
    // `cube pr create`'s `gh` calls (the only sanctioned PR-push path) can
    // authenticate. `GH_CONFIG_DIR` (set on the worker's spawn env — see
    // `spawn_invocation`) covers `~/.config/gh`'s non-secret metadata, but
    // gh's actual OAuth token lives in the macOS login keychain, which
    // Keychain Services resolves from `$HOME` — so it needs its own bridge.
    // Symlink only the single keychain file (never copy, matching the
    // auth.json pattern below), and never the whole `~/Library/Keychains`
    // directory: that also holds the iCloud-synced "local items" keychain
    // and other apps' entries unrelated to gh. Best-effort: skipped when
    // the host has no login keychain (see `resolve_login_keychain_source`).
    if let Some(keychain_source) = resolve_login_keychain_source() {
        let keychain_dir = process_home.join("Library").join("Keychains");
        fs::create_dir_all(&keychain_dir).with_context(|| format!("creating {}", keychain_dir.display()))?;
        let keychain_dest = keychain_dir.join("login.keychain-db");
        match fs::symlink_metadata(&keychain_dest) {
            Ok(_) => fs::remove_file(&keychain_dest)
                .with_context(|| format!("removing prior keychain link {}", keychain_dest.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("stat {}", keychain_dest.display())),
        }
        std::os::unix::fs::symlink(&keychain_source, &keychain_dest).with_context(|| {
            format!(
                "symlinking login keychain from {} → {}",
                keychain_source.display(),
                keychain_dest.display()
            )
        })?;
    }

    // Auth: symlink only — never copy the credential into the run home.
    let auth_source = resolve_grok_auth_source();
    if !auth_source.exists() {
        bail!(
            "Grok auth source {} does not exist; log in once with interactive \
             `grok` or set {GROK_AUTH_SOURCE_ENV} for tests",
            auth_source.display()
        );
    }
    // Refuse to use the live interactive home as GROK_HOME itself.
    if let Some(home) = std::env::var_os("HOME") {
        let interactive = PathBuf::from(home).join(".grok");
        if grok_home == interactive {
            bail!("refusing to use interactive ~/.grok as Boss-owned GROK_HOME");
        }
    }
    let auth_dest = grok_home.join("auth.json");
    // Replace any prior file/symlink so re-provision is idempotent.
    match fs::symlink_metadata(&auth_dest) {
        Ok(_) => {
            fs::remove_file(&auth_dest).with_context(|| format!("removing prior auth link {}", auth_dest.display()))?
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("stat {}", auth_dest.display()));
        }
    }
    std::os::unix::fs::symlink(&auth_source, &auth_dest).with_context(|| {
        format!(
            "symlinking auth.json from {} → {}",
            auth_source.display(),
            auth_dest.display()
        )
    })?;

    // Config + trust + provisional hooks — overwrite every run.
    fs::write(grok_home.join("config.toml"), render_base_config_toml())
        .with_context(|| format!("writing {}/config.toml", grok_home.display()))?;
    fs::write(
        grok_home.join("trusted_folders.toml"),
        render_trusted_folders_toml(workspace),
    )
    .with_context(|| format!("writing {}/trusted_folders.toml", grok_home.display()))?;
    fs::write(hooks_file_path(&grok_home), render_provision_hooks_json())
        .with_context(|| format!("writing provision hooks under {}", grok_home.display()))?;

    // Session id: stable for the run so spawn_invocation and later interrupt
    // observation share one Boss-assigned UUID. Refresh only when missing so
    // re-provision mid-run does not orphan an existing session tree.
    let session_path = session_id_path(&grok_home);
    let session_id = match fs::read_to_string(&session_path) {
        Ok(existing) if !existing.trim().is_empty() => existing.trim().to_owned(),
        _ => {
            let id = new_session_uuid()?;
            fs::write(&session_path, format!("{id}\n"))
                .with_context(|| format!("writing {}", session_path.display()))?;
            id
        }
    };

    // Absolute workspace path for spawn `--cwd` (SpawnRequest has no workspace).
    let workspace_abs = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    fs::write(
        workspace_path_stamp(&grok_home),
        format!("{}\n", workspace_abs.display()),
    )
    .with_context(|| format!("writing {}", workspace_path_stamp(&grok_home).display()))?;

    // Workspace-local `.grok/`: initial prompt + catch-all gitignore.
    // Agent-rules (`AGENTS.md`) are written by write_workspace_files into
    // `$GROK_HOME/AGENTS.md` via agent_rules_destination — not into the
    // workspace tree (Grok reads global rules from GROK_HOME and project
    // rules from the repo root AGENTS.md, never `.grok/AGENTS.md`).
    let config_dir = workspace.join(".grok");
    fs::create_dir_all(&config_dir).with_context(|| format!("creating {}", config_dir.display()))?;
    fs::write(config_dir.join("initial-prompt.txt"), prompt_text)
        .with_context(|| format!("writing initial prompt to {}/initial-prompt.txt", config_dir.display()))?;
    fs::write(config_dir.join(".gitignore"), "*\n")
        .with_context(|| format!("writing gitignore under {}", config_dir.display()))?;

    // Assert posture with live `grok inspect --json` (fail closed on checks
    // 2-5; the observed grokVersion is recorded below, never gated).
    let grok_version = assert_grok_posture(&grok_home, &process_home, workspace)?;

    Ok(GrokRuntimeState {
        grok_home,
        process_home,
        auth_source_path: auth_source,
        session_id,
        workspace_path: workspace_abs,
        grok_version,
    })
}

// ---------------------------------------------------------------------------
// Posture assertion
// ---------------------------------------------------------------------------

fn skip_posture_assert() -> bool {
    match std::env::var(GROK_SKIP_POSTURE_ASSERT_ENV) {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

/// Run `grok inspect --json` under the provisioned home and fail loudly on
/// bad posture (checks 2-5 of [`assert_inspect_json_posture`] — never
/// downgraded to a warning). Returns the observed `grokVersion` (`None` when
/// the assert was skipped, so no live inspect ran) so the caller can persist
/// it on the execution record — see [`GrokRuntimeState::grok_version`].
pub fn assert_grok_posture(grok_home: &Path, process_home: &Path, workspace: &Path) -> anyhow::Result<Option<String>> {
    if skip_posture_assert() {
        tracing::warn!(
            grok_home = %grok_home.display(),
            "skipping grok inspect posture assert ({GROK_SKIP_POSTURE_ASSERT_ENV} set; test-only)"
        );
        return Ok(None);
    }

    let output = Command::new("grok")
        .arg("inspect")
        .arg("--json")
        .current_dir(workspace)
        .env("GROK_HOME", grok_home)
        .env("HOME", process_home)
        // Never inherit operator GROK_FOLDER_TRUST=0 (ungates project hooks/MCP).
        .env_remove("GROK_FOLDER_TRUST")
        .output()
        .with_context(|| {
            format!(
                "running `grok inspect --json` with GROK_HOME={} HOME={} cwd={}",
                grok_home.display(),
                process_home.display(),
                workspace.display()
            )
        })?;

    if !output.status.success() {
        bail!(
            "grok inspect --json failed (status {}): stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let inspect: serde_json::Value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "parsing grok inspect --json stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })?;

    assert_inspect_json_posture(&inspect, grok_home, process_home, workspace)?;

    Ok(inspect.get("grokVersion").and_then(|v| v.as_str()).map(str::to_owned))
}

/// Validate a captured / live `grok inspect --json` document against the
/// Boss-required posture. Pure over the JSON so unit tests pin fail-closed
/// behaviour without a live `grok` binary.
///
/// Fail-closed rules:
/// - `projectTrusted` must be true
/// - every (`COMPAT_VENDORS` × `COMPAT_SURFACES`) cell must be present with
///   `enabled == false` (missing cell = failure; schema drift is not silent)
/// - hooks inventory must be non-empty (canary or full T-09 set)
/// - operator-home Claude permission sources must not appear under scoped HOME
///
/// `grokVersion` is observed but never gated: Grok auto-updates itself, and a
/// hard version pin here turned every automatic bump into a fail-closed
/// provisioning outage with no worker even attempted. A drift from
/// [`LAST_CHARACTERISED_GROK_VERSION`] only logs a `tracing::warn!`; the
/// caller (`assert_grok_posture`) is responsible for persisting the observed
/// version onto the execution record so the drift is not silently discarded.
pub fn assert_inspect_json_posture(
    inspect: &serde_json::Value,
    grok_home: &Path,
    process_home: &Path,
    workspace: &Path,
) -> anyhow::Result<()> {
    // Version: observed and logged, never gated (Grok auto-updates itself).
    let version = inspect.get("grokVersion").and_then(|v| v.as_str()).unwrap_or("");
    if !version.is_empty() && !version.starts_with(LAST_CHARACTERISED_GROK_VERSION) {
        tracing::warn!(
            grok_version = version,
            last_characterised_grok_version = LAST_CHARACTERISED_GROK_VERSION,
            "grok CLI version has drifted from the last characterised version; \
             re-run the `--trust` / `grok models` / `grok inspect --json` posture checks \
             and bump LAST_CHARACTERISED_GROK_VERSION once re-characterised"
        );
    }

    // Folder trust.
    let trusted = inspect.get("projectTrusted").and_then(|v| v.as_bool()).unwrap_or(false);
    if !trusted {
        bail!(
            "grok inspect reports projectTrusted=false for workspace {}; \
             trusted_folders.toml pre-seed failed (and --trust must not be the only belt)",
            workspace.display()
        );
    }

    // Compat: full known set for claude + cursor must be present and off.
    // Missing cell fails closed (inspect schema drift is not "pass").
    let cells = inspect
        .pointer("/externalCompat/cells")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for vendor in COMPAT_VENDORS {
        for surface in COMPAT_SURFACES {
            let enabled = cells.iter().find_map(|cell| {
                let v = cell.get("vendor")?.as_str()?;
                let s = cell.get("surface")?.as_str()?;
                if v == *vendor && s == *surface {
                    cell.get("enabled")?.as_bool()
                } else {
                    None
                }
            });
            match enabled {
                Some(false) => {}
                Some(true) => {
                    bail!(
                        "grok inspect reports externalCompat {vendor}/{surface} still enabled; \
                         Boss config.toml must disable the full compat block \
                         (hooks/agents/skills/mcps/rules/sessions)"
                    );
                }
                None => {
                    bail!(
                        "grok inspect missing externalCompat cell {vendor}/{surface}; \
                         expected enabled=false (fail-closed on schema drift). \
                         Official surfaces: hooks/agents/skills/mcps/rules/sessions"
                    );
                }
            }
        }
    }

    // Hooks must be registered (provisional canary or T-09 full set).
    // Progress observation remains follow-on; the canary only proves the
    // inventory path works so inspect does not report empty hooks.
    let hooks = inspect
        .get("hooks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if hooks.is_empty() {
        bail!(
            "grok inspect reports empty hooks inventory under GROK_HOME={}; \
             Boss global hooks were not registered",
            grok_home.display()
        );
    }

    // Operator Claude permission sources must not load under scoped HOME.
    let perm_sources = inspect
        .pointer("/permissions/sources")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for src in &perm_sources {
        let s = src.as_str().unwrap_or("");
        if s.contains(".claude/settings") && !s.contains(&process_home.display().to_string()) {
            // Project `.claude/settings.json` under the workspace can still
            // load when trusted (T-01 A7) — that is independent of HOME
            // scoping. Fail only on operator-home sources.
            let operator_home = std::env::var("HOME").unwrap_or_default();
            if !operator_home.is_empty() && s.contains(&operator_home) {
                bail!(
                    "grok inspect still loads operator Claude permission source {s:?}; \
                     process HOME scoping failed (T-01)"
                );
            }
        }
    }

    Ok(())
}

/// Refuse teardown paths outside the Boss-owned homes root.
pub fn assert_grok_home_safe_to_delete(grok_home: &Path) -> anyhow::Result<()> {
    if grok_home.as_os_str().is_empty() {
        bail!("refusing teardown with empty grok_home path");
    }
    let root = grok_homes_root();
    if root.as_os_str().is_empty() {
        bail!("refusing teardown: Boss grok homes root is empty");
    }
    let root_canon = match fs::canonicalize(&root) {
        Ok(p) => p,
        Err(_) => root.clone(),
    };
    if !grok_home.exists() {
        if grok_home == root || grok_home == root_canon {
            bail!(
                "refusing teardown: grok_home {} equals homes root {}",
                grok_home.display(),
                root_canon.display()
            );
        }
        // Container parent also counts as "under root".
        let parent = grok_home.parent().unwrap_or(grok_home);
        if !(grok_home.starts_with(&root)
            || grok_home.starts_with(&root_canon)
            || parent.starts_with(&root)
            || parent.starts_with(&root_canon))
        {
            bail!(
                "refusing teardown: grok_home {} is outside homes root {}",
                grok_home.display(),
                root_canon.display()
            );
        }
        return Ok(());
    }
    let home_canon =
        fs::canonicalize(grok_home).with_context(|| format!("canonicalize GROK_HOME {}", grok_home.display()))?;
    if home_canon == root_canon {
        bail!(
            "refusing to delete GROK_HOME {} — equals Boss homes root {}",
            home_canon.display(),
            root_canon.display()
        );
    }
    if !home_canon.starts_with(&root_canon) {
        bail!(
            "refusing to delete GROK_HOME {} — outside Boss homes root {}",
            home_canon.display(),
            root_canon.display()
        );
    }
    Ok(())
}

/// Reclaim a Boss-owned per-run Grok container (`grok-home/` +
/// `process-home/`) after retention policy says it is eligible. Refuses
/// anything outside [`grok_homes_root`]. Idempotent when the path is
/// already gone. Used by the engine retention sweep — **not** by
/// interactive `~/.grok` scanning and not by cwd heuristics.
///
/// `container` is the run container returned by
/// [`grok_run_container_for_run`] (parent of `GROK_HOME`), not `GROK_HOME`
/// alone, so `process-home/` is reclaimed along with it. Mirrors
/// [`crate::codex::reclaim_codex_home`] (`tools/boss/engine/driver/src/codex.rs:409`).
///
/// Deletion never follows symlinks: `std::fs::remove_dir_all` unlinks a
/// symlink entry it encounters instead of descending into (or deleting) its
/// target, so the `auth.json` credential symlink under `grok-home/` is
/// removed while the real credential file it points at (outside the homes
/// root) is left untouched.
pub fn reclaim_grok_home(container: &Path) -> anyhow::Result<()> {
    assert_grok_home_safe_to_delete(container)?;
    if !container.exists() {
        return Ok(());
    }
    // Re-check after exists: race with another reclaim is fine (NotFound).
    assert_grok_home_safe_to_delete(container)?;
    match fs::remove_dir_all(container) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing Boss-owned Grok run container {}", container.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn trust_path_variants_includes_tmp_and_private_tmp() {
        let variants = trust_path_variants(Path::new("/tmp/ws-a"));
        assert!(variants.iter().any(|p| p == "/tmp/ws-a"));
        assert!(variants.iter().any(|p| p == "/private/tmp/ws-a"));
    }

    #[test]
    fn trust_path_variants_includes_var_and_private_var() {
        let variants = trust_path_variants(Path::new("/var/folders/xx/ws"));
        assert!(variants.iter().any(|p| p == "/var/folders/xx/ws"));
        assert!(variants.iter().any(|p| p == "/private/var/folders/xx/ws"));
    }

    #[test]
    fn render_config_disables_claude_and_cursor_compat() {
        let cfg = render_base_config_toml();
        assert!(cfg.contains("[compat.claude]"));
        assert!(cfg.contains("[compat.cursor]"));
        for surface in COMPAT_SURFACES {
            // Each surface appears false under both vendors (count >= 2).
            let needle = format!("{surface} = false");
            assert!(
                cfg.matches(&needle).count() >= 2,
                "expected {needle} under both vendors: {cfg}"
            );
        }
        // Official matrix has no plugins cell — writing `plugins = false` is a
        // silent no-op that previously left mcps/sessions enabled under live
        // inspect. Comments may mention the word; the assignment must not.
        assert!(
            !cfg.lines().any(|l| {
                let t = l.trim();
                !t.starts_with('#') && t.contains("plugins")
            }),
            "must not write plugins= assignment (not an official cell): {cfg}"
        );
        // Forbidden: undocumented permissions cell that has no effect.
        assert!(!cfg.lines().any(|l| {
            let t = l.trim();
            !t.starts_with('#') && t.contains("permissions")
        }));
    }

    /// Minimal inspect JSON with every expected compat cell disabled and a
    /// canary hook entry — shape captured from grok 0.2.112.
    fn good_inspect_fixture() -> serde_json::Value {
        let mut cells = Vec::new();
        for vendor in COMPAT_VENDORS {
            for surface in COMPAT_SURFACES {
                cells.push(serde_json::json!({
                    "vendor": vendor,
                    "surface": surface,
                    "enabled": false,
                    "source": "config",
                }));
            }
        }
        // Codex sessions default cell is present in live inspect; we do not
        // assert on it (Boss only suppresses claude/cursor).
        cells.push(serde_json::json!({
            "vendor": "codex",
            "surface": "sessions",
            "enabled": true,
            "source": "default",
        }));
        serde_json::json!({
            "grokVersion": LAST_CHARACTERISED_GROK_VERSION,
            "projectTrusted": true,
            "hooks": [{
                "event": "session_start",
                "hookType": "command",
                "target": "true",
                "matcher": "*",
            }],
            "externalCompat": { "cells": cells },
            "permissions": { "sources": [], "loaded": 0 },
        })
    }

    #[test]
    fn assert_inspect_json_accepts_full_disabled_matrix() {
        let inspect = good_inspect_fixture();
        assert_inspect_json_posture(
            &inspect,
            Path::new("/tmp/boss-grok-homes/r/grok-home"),
            Path::new("/tmp/boss-grok-homes/r/process-home"),
            Path::new("/tmp/ws"),
        )
        .expect("good fixture must pass");
    }

    #[test]
    fn assert_inspect_json_fails_when_mcps_still_enabled() {
        let mut inspect = good_inspect_fixture();
        // Flip claude/mcps to enabled — the bug plugins=false left open.
        for cell in inspect["externalCompat"]["cells"].as_array_mut().unwrap() {
            if cell["vendor"] == "claude" && cell["surface"] == "mcps" {
                cell["enabled"] = serde_json::json!(true);
                cell["source"] = serde_json::json!("default");
            }
        }
        let err = assert_inspect_json_posture(
            &inspect,
            Path::new("/tmp/h"),
            Path::new("/tmp/ph"),
            Path::new("/tmp/ws"),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("claude/mcps") && msg.contains("enabled"),
            "expected enabled-cell failure, got {msg}"
        );
    }

    #[test]
    fn assert_inspect_json_fails_closed_when_sessions_cell_missing() {
        let mut inspect = good_inspect_fixture();
        let cells = inspect["externalCompat"]["cells"].as_array_mut().unwrap();
        cells.retain(|c| !(c["vendor"] == "claude" && c["surface"] == "sessions"));
        let err = assert_inspect_json_posture(
            &inspect,
            Path::new("/tmp/h"),
            Path::new("/tmp/ph"),
            Path::new("/tmp/ws"),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing") && msg.contains("claude/sessions"),
            "expected missing-cell fail-closed, got {msg}"
        );
    }

    #[test]
    fn assert_inspect_json_fails_on_empty_hooks() {
        let mut inspect = good_inspect_fixture();
        inspect["hooks"] = serde_json::json!([]);
        let err = assert_inspect_json_posture(
            &inspect,
            Path::new("/tmp/h"),
            Path::new("/tmp/ph"),
            Path::new("/tmp/ws"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("empty hooks"),
            "expected empty-hooks failure, got {err}"
        );
    }

    #[test]
    fn assert_inspect_json_posture_does_not_gate_on_version_mismatch() {
        let mut inspect = good_inspect_fixture();
        inspect["grokVersion"] = serde_json::json!("9.9.9-not-characterised");
        assert_inspect_json_posture(
            &inspect,
            Path::new("/tmp/h"),
            Path::new("/tmp/ph"),
            Path::new("/tmp/ws"),
        )
        .expect("a grokVersion drift must only warn, never fail closed");
    }

    #[test]
    fn assert_inspect_json_fails_when_untrusted() {
        let mut inspect = good_inspect_fixture();
        inspect["projectTrusted"] = serde_json::json!(false);
        let err = assert_inspect_json_posture(
            &inspect,
            Path::new("/tmp/h"),
            Path::new("/tmp/ph"),
            Path::new("/tmp/ws"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("projectTrusted"),
            "expected trust failure, got {err}"
        );
    }

    #[test]
    fn session_uuid_is_v4_shaped() {
        let id = new_session_uuid().unwrap();
        let parts: Vec<_> = id.split('-').collect();
        assert_eq!(parts.len(), 5, "{id}");
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].starts_with('4'), "version nibble: {id}");
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
    }

    #[test]
    fn grok_home_for_run_is_under_homes_root() {
        let tmp = TempDir::new().unwrap();
        let _lock = GROK_HOMES_ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os(GROK_HOMES_ROOT_ENV);
        // SAFETY: serialised by GROK_HOMES_ENV_TEST_LOCK.
        unsafe { std::env::set_var(GROK_HOMES_ROOT_ENV, tmp.path()) };
        let home = grok_home_for_run("run-abc").unwrap();
        assert!(home.starts_with(tmp.path()));
        assert!(home.ends_with("grok-home"));
        match prior {
            Some(v) => unsafe { std::env::set_var(GROK_HOMES_ROOT_ENV, v) },
            None => unsafe { std::env::remove_var(GROK_HOMES_ROOT_ENV) },
        }
    }

    #[test]
    fn reclaim_grok_home_removes_symlink_entry_but_never_its_target() {
        let tmp = TempDir::new().unwrap();
        let _lock = GROK_HOMES_ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os(GROK_HOMES_ROOT_ENV);
        let homes_root = tmp.path().join("boss-grok-homes");
        fs::create_dir_all(&homes_root).unwrap();
        // SAFETY: serialised by GROK_HOMES_ENV_TEST_LOCK.
        unsafe { std::env::set_var(GROK_HOMES_ROOT_ENV, &homes_root) };

        let container = homes_root.join("run-1");
        let grok_home = container.join(GROK_HOME_LEAF);
        fs::create_dir_all(&grok_home).unwrap();
        fs::create_dir_all(container.join(PROCESS_HOME_LEAF)).unwrap();

        // Real credential file living entirely outside the Boss-owned homes
        // root, exactly like the operator's real `~/.grok/auth.json`.
        let real_auth_dir = tmp.path().join("real-auth-target");
        fs::create_dir_all(&real_auth_dir).unwrap();
        let real_auth = real_auth_dir.join("auth.json");
        fs::write(&real_auth, "super-secret-token").unwrap();

        // Symlink inside GROK_HOME pointing at the real credential, exactly
        // as provisioning wires it (see `provision_grok_home`).
        std::os::unix::fs::symlink(&real_auth, grok_home.join("auth.json")).unwrap();

        reclaim_grok_home(&container).unwrap();

        assert!(
            !container.exists(),
            "run container (including the symlink) must be gone"
        );
        assert!(
            real_auth.exists(),
            "symlink target outside the home must survive a sweep"
        );
        assert_eq!(
            fs::read_to_string(&real_auth).unwrap(),
            "super-secret-token",
            "symlink target's contents must be untouched"
        );

        match prior {
            Some(v) => unsafe { std::env::set_var(GROK_HOMES_ROOT_ENV, v) },
            None => unsafe { std::env::remove_var(GROK_HOMES_ROOT_ENV) },
        }
    }

    /// Save/restore a set of env vars around a test body, serialised by
    /// [`GROK_HOMES_ENV_TEST_LOCK`] (shared with every other env-mutating
    /// test in this crate — env vars are process-global, so anything that
    /// flips `HOME`/`GH_CONFIG_DIR`/`XDG_CONFIG_HOME` must use the same lock
    /// the homes/auth-source tests already serialise on).
    struct MultiEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl MultiEnvGuard {
        fn set(pairs: &[(&'static str, &Path)]) -> Self {
            Self::set_and_clear(pairs, &[])
        }

        /// `set` each `(key, value)` pair, then remove every key in `clear`
        /// (e.g. to test fallback precedence by ensuring a higher-priority
        /// var is genuinely absent, not just shadowed).
        fn set_and_clear(pairs: &[(&'static str, &Path)], clear: &[&'static str]) -> Self {
            let lock = GROK_HOMES_ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let mut prior = Vec::new();
            for (key, value) in pairs {
                prior.push((*key, std::env::var_os(key)));
                // SAFETY: serialised by GROK_HOMES_ENV_TEST_LOCK.
                unsafe { std::env::set_var(key, value) };
            }
            for key in clear {
                prior.push((*key, std::env::var_os(key)));
                // SAFETY: serialised by GROK_HOMES_ENV_TEST_LOCK.
                unsafe { std::env::remove_var(key) };
            }
            Self { _lock: lock, prior }
        }
    }

    impl Drop for MultiEnvGuard {
        fn drop(&mut self) {
            for (key, prior) in &self.prior {
                match prior {
                    // SAFETY: serialised by GROK_HOMES_ENV_TEST_LOCK.
                    Some(v) => unsafe { std::env::set_var(key, v) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }
    }

    #[test]
    fn resolve_gh_config_dir_prefers_gh_config_dir_env() {
        let _guard = MultiEnvGuard::set(&[
            ("GH_CONFIG_DIR", Path::new("/explicit/gh-config")),
            ("XDG_CONFIG_HOME", Path::new("/should-be-ignored")),
            ("HOME", Path::new("/also-ignored")),
        ]);
        assert_eq!(resolve_gh_config_dir(), Some(PathBuf::from("/explicit/gh-config")));
    }

    #[test]
    fn resolve_gh_config_dir_falls_back_to_xdg_config_home() {
        let _guard = MultiEnvGuard::set_and_clear(
            &[
                ("XDG_CONFIG_HOME", Path::new("/xdg-home")),
                ("HOME", Path::new("/ignored-home")),
            ],
            &["GH_CONFIG_DIR"],
        );
        assert_eq!(resolve_gh_config_dir(), Some(PathBuf::from("/xdg-home/gh")));
    }

    #[test]
    fn resolve_gh_config_dir_falls_back_to_home_dot_config_gh() {
        let _guard = MultiEnvGuard::set_and_clear(
            &[("HOME", Path::new("/plain-home"))],
            &["GH_CONFIG_DIR", "XDG_CONFIG_HOME"],
        );
        assert_eq!(resolve_gh_config_dir(), Some(PathBuf::from("/plain-home/.config/gh")));
    }

    #[test]
    fn resolve_login_keychain_source_finds_existing_file() {
        let tmp = TempDir::new().unwrap();
        let keychain_dir = tmp.path().join("Library").join("Keychains");
        fs::create_dir_all(&keychain_dir).unwrap();
        let keychain_file = keychain_dir.join("login.keychain-db");
        fs::write(&keychain_file, b"fake").unwrap();
        let _guard = MultiEnvGuard::set(&[("HOME", tmp.path())]);
        assert_eq!(resolve_login_keychain_source(), Some(keychain_file));
    }

    #[test]
    fn resolve_login_keychain_source_none_when_missing() {
        let tmp = TempDir::new().unwrap();
        // No Library/Keychains under this HOME at all.
        let _guard = MultiEnvGuard::set(&[("HOME", tmp.path())]);
        assert_eq!(resolve_login_keychain_source(), None);
    }
}
