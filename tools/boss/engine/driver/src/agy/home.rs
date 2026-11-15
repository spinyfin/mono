//! Boss-owned per-run `HOME` layout and pre-spawn posture checks for `agy`.
//!
//! The invariant this module enforces is not "each run has a directory":
//! no two concurrent `agy` runs share a mutable `agy` state artifact, and
//! the host login home's `~/.gemini` is never one of those artifacts. The login
//! keychain file is the deliberate credential exception, bridged through
//! [`crate::scoped_home`].

use crate::scoped_home::trust_path_variants;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use super::environment::{AgyProcessEnvironment, resolve_agy_binary, skip_binary_assert};
#[cfg(test)]
use crate::scoped_home::LOGIN_KEYCHAIN_LEAF;

/// Process-global lock for tests that mutate [`AGY_HOMES_ROOT_ENV`]. Shared
/// with Grok via [`crate::scoped_home::SCOPED_HOME_ENV_TEST_LOCK`] so the
/// two drivers cannot race their homes-root overrides.
pub use crate::scoped_home::SCOPED_HOME_ENV_TEST_LOCK as AGY_HOMES_ENV_TEST_LOCK;

/// Env override for the root under which per-run `agy` homes live.
pub const AGY_HOMES_ROOT_ENV: &str = "BOSS_AGY_HOMES_DIR";

/// When set to `1`/`true`, skip the live `agy` posture probe.
pub const AGY_SKIP_POSTURE_ASSERT_ENV: &str = "BOSS_AGY_SKIP_POSTURE_ASSERT";

/// Default leaf under the system temp when [`AGY_HOMES_ROOT_ENV`] is unset.
pub(crate) const AGY_HOMES_DIR_NAME: &str = "boss-agy-homes";

const PROCESS_HOME_LEAF: &str = "process-home";
/// Global-root hook directory, relative to the per-run `HOME`.
const HOOKS_DIR_RELATIVE: &str = ".gemini/config";
const CONTROL_LEAF: &str = "control";
const WORKSPACE_PATH_FILENAME: &str = "boss-workspace-path";

const ONBOARDING_JSON: &str = r#"{"consumerOnboardingComplete": true, "enterpriseOnboardingComplete": false, "onboardingComplete": true}
"#;

/// Check existing paths before creating or overwriting any run state.
fn validate_private_layout(process_home: &Path) -> anyhow::Result<()> {
    let root = agy_homes_root();
    let relative = process_home
        .strip_prefix(&root)
        .context("agy HOME outside homes root")?;
    if relative.components().count() != 2
        || relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("invalid agy process HOME {}", process_home.display());
    }
    refuse_host_gemini_as_home(process_home)?;
    if let Some(home) = std::env::var_os("HOME") {
        let host_gemini = PathBuf::from(home).join(".gemini");
        match fs::canonicalize(&host_gemini) {
            Ok(host) => {
                for ancestor in process_home.ancestors().filter(|p| p.exists()) {
                    if fs::canonicalize(ancestor)?.starts_with(&host) {
                        bail!("agy private HOME reaches host ~/.gemini");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("resolving host ~/.gemini"),
        }
    }
    let mut path = root.clone();
    for ancestor in root.ancestors() {
        // macOS exposes these system aliases; run-specific aliases are refused.
        if ancestor != Path::new("/tmp") && ancestor != Path::new("/var") {
            check_private_entry(ancestor)?;
        }
    }
    check_private_entry(&path)?;
    for component in relative.components() {
        path.push(component);
        check_private_entry(&path)?;
    }
    // Reject symlinks and shared hard links under the run’s .gemini, control and rules trees.
    check_private_tree(&process_home.join(".gemini"))?;
    check_private_tree(&process_home.parent().context("run container")?.join(CONTROL_LEAF))?;
    check_private_tree(&process_home.parent().context("run container")?.join("rules-root"))?;
    Ok(())
}

pub(super) fn check_private_entry(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || (meta.is_file() && meta.nlink() != 1) => {
            bail!("agy private path {} is a symlink or shared hard link", path.display());
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("stat private path {}", path.display())),
    }
}

fn check_private_tree(path: &Path) -> anyhow::Result<()> {
    check_private_entry(path)?;
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            check_private_tree(&entry?.path())?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

pub fn agy_homes_root() -> PathBuf {
    match std::env::var_os(AGY_HOMES_ROOT_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir().join(AGY_HOMES_DIR_NAME),
    }
}

pub fn sanitize_run_id_for_home(run_id: &str) -> anyhow::Result<String> {
    if run_id.is_empty() {
        bail!("empty run_id refused for Boss-owned agy HOME");
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
        bail!("run_id {run_id:?} sanitized to empty; refused for Boss-owned agy HOME");
    }
    Ok(safe)
}

pub fn agy_run_container_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    let safe = sanitize_run_id_for_home(run_id)?;
    let root = agy_homes_root();
    let container = root.join(safe);
    if !container.starts_with(&root) || container == root {
        bail!(
            "resolved agy run container {} is not a strict child of homes root {}",
            container.display(),
            root.display()
        );
    }
    Ok(container)
}

pub fn process_home_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    Ok(agy_run_container_for_run(run_id)?.join(PROCESS_HOME_LEAF))
}

pub fn hooks_dir_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    Ok(process_home_for_run(run_id)?.join(HOOKS_DIR_RELATIVE))
}

pub fn control_dir_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    Ok(agy_run_container_for_run(run_id)?.join(CONTROL_LEAF))
}

pub fn session_settings_path_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    Ok(control_dir_for_run(run_id)?.join("session-settings.json"))
}

pub fn log_file_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    Ok(control_dir_for_run(run_id)?.join("agy.log"))
}

pub fn agent_rules_path_for_run(run_id: &str) -> anyhow::Result<PathBuf> {
    Ok(agy_run_container_for_run(run_id)?.join("rules-root/.agents/rules/boss.md"))
}

pub fn workspace_path_stamp(control_dir: &Path) -> PathBuf {
    control_dir.join(WORKSPACE_PATH_FILENAME)
}

pub fn read_workspace_path_stamp(control_dir: &Path) -> anyhow::Result<PathBuf> {
    let path = workspace_path_stamp(control_dir);
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let s = raw.trim();
    if s.is_empty() {
        bail!("empty workspace path stamp at {}", path.display());
    }
    Ok(PathBuf::from(s))
}

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct AgyRuntimeState {
    pub process_home: PathBuf,
    pub control_dir: PathBuf,
    pub session_settings_path: PathBuf,
    pub log_file: PathBuf,
    pub workspace_path: PathBuf,
    pub agy_binary: PathBuf,
}

impl AgyRuntimeState {
    pub fn to_driver_runtime_state(&self) -> crate::DriverRuntimeState {
        crate::DriverRuntimeState::new(serde_json::to_value(self).expect("AgyRuntimeState is serializable"))
    }

    pub fn from_driver_runtime_state(state: &crate::DriverRuntimeState) -> anyhow::Result<Self> {
        serde_json::from_value(state.as_value().clone()).context("decoding AgyRuntimeState from DriverRuntimeState")
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_onboarding_json() -> &'static str {
    ONBOARDING_JSON
}

pub fn render_trusted_workspaces_json(workspace: &Path) -> String {
    let paths = trust_path_variants(workspace);
    serde_json::to_string_pretty(&serde_json::json!({ "trustedWorkspaces": paths }))
        .expect("trustedWorkspaces JSON is serializable")
        + "\n"
}

pub fn render_session_settings_json(workspace: &Path) -> String {
    // Trust only. Permission settings are supplied by the permission renderer.
    // Skipping onboarding does not answer data-collection consent;
    // enableTelemetry is omitted.
    render_trusted_workspaces_json(workspace)
}

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
// Provision
// ---------------------------------------------------------------------------

pub fn provision_agy_home(workspace: &Path, prompt_text: &str, run_id: &str) -> anyhow::Result<AgyRuntimeState> {
    let container = agy_run_container_for_run(run_id)?;
    let process_home = container.join(PROCESS_HOME_LEAF);
    let control_dir = container.join(CONTROL_LEAF);
    let session_settings_path = control_dir.join("session-settings.json");
    let log_file = control_dir.join("agy.log");

    validate_private_layout(&process_home)?;
    fs::create_dir_all(&process_home)
        .with_context(|| format!("creating agy process HOME {}", process_home.display()))?;
    fs::create_dir_all(&control_dir).with_context(|| format!("creating agy control dir {}", control_dir.display()))?;
    fs::create_dir_all(agent_rules_path_for_run(run_id)?.parent().context("rules directory")?)?;

    refuse_host_gemini_as_home(&process_home)?;

    let gemini = process_home.join(".gemini");
    let antigravity = gemini.join("antigravity-cli");
    fs::create_dir_all(antigravity.join("cache"))
        .with_context(|| format!("creating antigravity-cli cache under {}", antigravity.display()))?;
    let hooks_dir = process_home.join(HOOKS_DIR_RELATIVE);
    fs::create_dir_all(&hooks_dir).with_context(|| format!("creating hooks directory {}", hooks_dir.display()))?;

    // Onboarding screen skip. This is not a data-collection consent answer:
    // enableTelemetry is not written, and the Terms checkbox is not ticked.
    fs::write(antigravity.join("cache/onboarding.json"), render_onboarding_json())
        .with_context(|| format!("writing onboarding.json under {}", antigravity.display()))?;
    let trust_json = render_trusted_workspaces_json(workspace);
    fs::write(antigravity.join("settings.json"), &trust_json)
        .with_context(|| format!("writing private CLI settings under {}", antigravity.display()))?;
    fs::write(&session_settings_path, render_session_settings_json(workspace))
        .with_context(|| format!("writing session settings {}", session_settings_path.display()))?;
    fs::write(hooks_dir.join("hooks.json"), render_provision_hooks_json())
        .with_context(|| format!("writing provision hooks under {}", hooks_dir.display()))?;
    // Do not write `.gemini/settings.json` or `trustedFolders.json`: neither
    // is read by agy.

    let workspace_abs = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    fs::write(
        workspace_path_stamp(&control_dir),
        format!("{}\n", workspace_abs.display()),
    )
    .with_context(|| format!("writing {}", workspace_path_stamp(&control_dir).display()))?;

    crate::initial_prompt::provision(&control_dir, &control_dir, prompt_text, &super::AgyDriver::default())
        .context("provisioning agy initial-prompt launcher under the Boss-owned control directory")?;

    let agy_binary = resolve_agy_binary(&agy_homes_root())?;
    let environment =
        AgyProcessEnvironment::resolve(&process_home, &log_file, &session_settings_path, agy_binary.clone())
            .context("resolving scoped agy worker environment")?;
    environment
        .provision_home_delegates()
        .context("delegating host tool state into scoped agy HOME")?;

    assert_agy_posture(&process_home, workspace)?;

    if !skip_posture_assert() && !skip_binary_assert() {
        probe_agy_models(&environment).context("live agy models posture probe")?;
    }

    Ok(AgyRuntimeState::builder()
        .process_home(process_home)
        .control_dir(control_dir)
        .session_settings_path(session_settings_path)
        .log_file(log_file)
        .workspace_path(workspace_abs)
        .agy_binary(agy_binary)
        .build())
}

fn refuse_host_gemini_as_home(process_home: &Path) -> anyhow::Result<()> {
    if let Some(home) = std::env::var_os("HOME") {
        let interactive = PathBuf::from(&home);
        if process_home == interactive {
            bail!("refusing to use the host HOME as the Boss-owned agy process HOME");
        }
        let interactive_gemini = PathBuf::from(home).join(".gemini");
        if process_home.join(".gemini") == interactive_gemini {
            bail!("refusing to use host ~/.gemini as the Boss-owned agy state tree");
        }
    }
    Ok(())
}

pub(super) fn skip_posture_assert() -> bool {
    crate::scoped_home::env_flag(AGY_SKIP_POSTURE_ASSERT_ENV)
}

/// Filesystem invariant: private `agy` state is not shared, and the
/// host login home's `~/.gemini` is not the run's `.gemini`.
pub fn assert_agy_posture(process_home: &Path, workspace: &Path) -> anyhow::Result<()> {
    validate_private_layout(process_home)?;

    let onboarding = process_home.join(".gemini/antigravity-cli/cache/onboarding.json");
    let body = fs::read_to_string(&onboarding)
        .with_context(|| format!("reading onboarding.json at {}", onboarding.display()))?;
    let parsed: serde_json::Value = serde_json::from_str(&body).context("parsing provisioned onboarding.json")?;
    if parsed.get("onboardingComplete") != Some(&serde_json::Value::Bool(true)) {
        bail!("provisioned onboarding.json does not skip the onboarding screen");
    }

    let settings = fs::read_to_string(process_home.join(".gemini/antigravity-cli/settings.json"))
        .context("reading private CLI settings.json")?;
    let settings_json: serde_json::Value =
        serde_json::from_str(&settings).context("parsing private CLI settings.json")?;
    if settings_json.get("enableTelemetry").is_some() {
        bail!("private CLI settings must not answer data-collection consent (enableTelemetry)");
    }
    let trusted = settings_json
        .get("trustedWorkspaces")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let variants = trust_path_variants(workspace);
    if !variants.iter().any(|p| trusted.iter().any(|v| v.as_str() == Some(p))) {
        bail!(
            "trustedWorkspaces does not include the workspace path {}; got {trusted:?}",
            workspace.display()
        );
    }

    // `.gemini/settings.json` is not read by agy and must not be created.
    if process_home.join(".gemini/settings.json").exists() {
        bail!("must not write .gemini/settings.json; agy does not read it");
    }
    if process_home.join(".gemini/trustedFolders.json").exists() {
        bail!("must not write trustedFolders.json; agy does not read it");
    }

    Ok(())
}

fn probe_agy_models(environment: &AgyProcessEnvironment) -> anyhow::Result<()> {
    let mut command = Command::new(environment.agy_binary());
    command.args(environment.launch_args()).arg("models");
    environment.apply_to_command(&mut command);
    let output = super::environment::run_agy_bounded(&mut command, super::environment::AGY_MODELS_TIMEOUT)?;
    if !output.status.success() {
        bail!(
            "agy models failed under scoped HOME {} (status {}): stdout={} stderr={}",
            environment.process_home().display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Remove run state while keeping conversation and transcript paths readable
/// after teardown. `conversations/` is the conversation store; `brain/` holds
/// `<cid>/.system_generated/logs/transcript_full.jsonl`, the path the Stop
/// payload's transcriptPath stamps.
pub fn reclaim_agy_run(process_home: &Path) -> anyhow::Result<()> {
    let container = process_home.parent().context("agy run container")?;
    assert_agy_home_safe_to_delete(container)?;
    validate_private_layout(process_home)?;
    let base = ["process-home", ".gemini", "antigravity-cli"];
    let keep: Vec<Vec<&str>> = ["conversations", "brain"]
        .iter()
        .map(|leaf| base.iter().copied().chain([*leaf]).collect())
        .collect();
    retain_paths(container, &keep)
}

/// Delete everything under `path` except the given relative component paths
/// (and their contents).
fn retain_paths(path: &Path, keep: &[Vec<&str>]) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let matching: Vec<&[&str]> = keep
            .iter()
            .filter(|k| k.first().is_some_and(|first| name == *first))
            .map(|k| &k[1..])
            .collect();
        if matching.iter().any(|rest| rest.is_empty()) {
            continue;
        }
        if matching.is_empty() {
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        } else {
            let rest: Vec<Vec<&str>> = matching.iter().map(|r| r.to_vec()).collect();
            retain_paths(&entry.path(), &rest)?;
        }
    }
    Ok(())
}

pub fn assert_agy_home_safe_to_delete(path: &Path) -> anyhow::Result<()> {
    if path.as_os_str().is_empty() {
        bail!("refusing teardown with empty agy home path");
    }
    let root = agy_homes_root();
    if root.as_os_str().is_empty() {
        bail!("refusing teardown: Boss agy homes root is empty");
    }
    let root_canon = match fs::canonicalize(&root) {
        Ok(p) => p,
        Err(_) => root.clone(),
    };
    if !path.exists() {
        if path == root || path == root_canon {
            bail!(
                "refusing teardown: agy home {} equals homes root {}",
                path.display(),
                root_canon.display()
            );
        }
        let parent = path.parent().unwrap_or(path);
        if !(path.starts_with(&root)
            || path.starts_with(&root_canon)
            || parent.starts_with(&root)
            || parent.starts_with(&root_canon))
        {
            bail!(
                "refusing teardown: agy home {} is outside homes root {}",
                path.display(),
                root_canon.display()
            );
        }
        return Ok(());
    }
    let home_canon = fs::canonicalize(path).with_context(|| format!("canonicalize agy home {}", path.display()))?;
    if home_canon == root_canon {
        bail!(
            "refusing to delete agy home {} — equals Boss homes root {}",
            home_canon.display(),
            root_canon.display()
        );
    }
    if !home_canon.starts_with(&root_canon) {
        bail!(
            "refusing to delete agy home {} — outside Boss homes root {}",
            home_canon.display(),
            root_canon.display()
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "home_tests.rs"]
mod tests;
