//! Boss-owned per-run `HOME` layout and pre-spawn posture checks for `agy`.
//!
//! The invariant this module enforces is not "each run has a directory":
//! no two concurrent `agy` runs share a mutable `agy` state artifact, and
//! the operator's `~/.gemini` is never one of those artifacts. The login
//! keychain file is the deliberate credential exception, bridged through
//! [`crate::scoped_home`].

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use super::environment::{AgyProcessEnvironment, resolve_agy_binary, skip_binary_assert, verify_agy_binary};
#[cfg(test)]
use crate::scoped_home::LOGIN_KEYCHAIN_LEAF;

/// Env override for the root under which per-run `agy` homes live.
pub const AGY_HOMES_ROOT_ENV: &str = "BOSS_AGY_HOMES_DIR";

/// When set to `1`/`true`, skip the live `agy` posture probe.
pub const AGY_SKIP_POSTURE_ASSERT_ENV: &str = "BOSS_AGY_SKIP_POSTURE_ASSERT";

/// Default leaf under the system temp when [`AGY_HOMES_ROOT_ENV`] is unset.
pub(crate) const AGY_HOMES_DIR_NAME: &str = "boss-agy-homes";

const PROCESS_HOME_LEAF: &str = "process-home";
const CONTROL_LEAF: &str = "control";
const WORKSPACE_PATH_FILENAME: &str = "boss-workspace-path";

const ONBOARDING_JSON: &str = r#"{"consumerOnboardingComplete": true, "enterpriseOnboardingComplete": false, "onboardingComplete": true}
"#;

/// Mutable `agy` artifacts that must stay private to the run. The login
/// keychain is deliberately absent: it is the shared credential bridge.
const PRIVATE_STATE_LEAVES: &[&str] = &[
    ".gemini/antigravity-cli/conversation_summaries.db",
    ".gemini/antigravity-cli/settings.json",
    ".gemini/antigravity-cli/presence",
    ".gemini/antigravity-cli/brain",
    ".gemini/antigravity-cli/conversations",
    ".gemini/antigravity-cli/updater",
    ".gemini/antigravity-cli/log",
    ".gemini/antigravity-cli/cache",
    ".gemini/config",
];

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
    Ok(process_home_for_run(run_id)?.join(".gemini").join("AGENTS.md"))
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
    pub agy_version: Option<String>,
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
    // Trust only. Permission posture (agentMode, toolPermission, allow/deny)
    // is the later per-kind settings renderer; skipping onboarding is not a
    // data-collection consent answer, so enableTelemetry is omitted.
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
    if let Some(rest) = s.strip_prefix("/tmp") {
        paths.insert(format!("/private/tmp{rest}"));
    }
    if let Some(rest) = s.strip_prefix("/private/tmp") {
        paths.insert(format!("/tmp{rest}"));
    }
    if let Some(rest) = s.strip_prefix("/var/") {
        paths.insert(format!("/private/var/{rest}"));
    }
    if let Some(rest) = s.strip_prefix("/private/var/") {
        paths.insert(format!("/var/{rest}"));
    }
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

    fs::create_dir_all(&process_home)
        .with_context(|| format!("creating agy process HOME {}", process_home.display()))?;
    fs::create_dir_all(&control_dir).with_context(|| format!("creating agy control dir {}", control_dir.display()))?;

    refuse_operator_gemini_as_home(&process_home)?;

    let gemini = process_home.join(".gemini");
    let antigravity = gemini.join("antigravity-cli");
    fs::create_dir_all(antigravity.join("cache"))
        .with_context(|| format!("creating antigravity-cli cache under {}", antigravity.display()))?;
    fs::create_dir_all(gemini.join("config"))
        .with_context(|| format!("creating .gemini/config under {}", gemini.display()))?;

    // Onboarding screen skip. This is not a data-collection consent answer:
    // enableTelemetry is not written, and the Terms checkbox is not ticked.
    fs::write(antigravity.join("cache/onboarding.json"), render_onboarding_json())
        .with_context(|| format!("writing onboarding.json under {}", antigravity.display()))?;
    let trust_json = render_trusted_workspaces_json(workspace);
    fs::write(antigravity.join("settings.json"), &trust_json)
        .with_context(|| format!("writing private CLI settings under {}", antigravity.display()))?;
    fs::write(&session_settings_path, render_session_settings_json(workspace))
        .with_context(|| format!("writing session settings {}", session_settings_path.display()))?;
    fs::write(gemini.join("config/hooks.json"), render_provision_hooks_json())
        .with_context(|| format!("writing provision hooks under {}", gemini.display()))?;
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
    if !skip_binary_assert() {
        verify_agy_binary(&agy_binary).context("verifying Boss-owned agy binary identity at provision")?;
    }

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

fn refuse_operator_gemini_as_home(process_home: &Path) -> anyhow::Result<()> {
    if let Some(home) = std::env::var_os("HOME") {
        let interactive = PathBuf::from(&home);
        if process_home == interactive {
            bail!("refusing to use the operator HOME as the Boss-owned agy process HOME");
        }
        let interactive_gemini = PathBuf::from(home).join(".gemini");
        if process_home.join(".gemini") == interactive_gemini {
            bail!("refusing to use operator ~/.gemini as the Boss-owned agy state tree");
        }
    }
    Ok(())
}

pub(super) fn skip_posture_assert() -> bool {
    match std::env::var(AGY_SKIP_POSTURE_ASSERT_ENV) {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

/// Filesystem invariant: private `agy` state is not shared, and the
/// operator's `~/.gemini` is not the run's `.gemini`.
pub fn assert_agy_posture(process_home: &Path, workspace: &Path) -> anyhow::Result<()> {
    let root = agy_homes_root();
    if !process_home.starts_with(&root) {
        bail!(
            "agy process HOME {} is outside Boss homes root {}",
            process_home.display(),
            root.display()
        );
    }
    refuse_operator_gemini_as_home(process_home)?;

    let gemini = process_home.join(".gemini");
    if gemini.is_symlink() {
        bail!(
            "agy .gemini at {} is a symlink; private state must not be shared",
            gemini.display()
        );
    }
    if !gemini.is_dir() {
        bail!("agy .gemini {} is missing after provision", gemini.display());
    }
    if let Some(operator_home) = std::env::var_os("HOME") {
        let operator_gemini = PathBuf::from(&operator_home).join(".gemini");
        if let (Ok(run), Ok(operator)) = (fs::canonicalize(&gemini), fs::canonicalize(&operator_gemini))
            && run == operator
        {
            bail!(
                "agy process HOME .gemini canonicalizes to the operator ~/.gemini ({})",
                operator.display()
            );
        }
        // HOME itself is the operator home at provision time; the worker's
        // HOME will be process_home. Confirm they differ.
        if process_home == Path::new(&operator_home) {
            bail!("scoped agy HOME equals the operator HOME");
        }
    }

    for leaf in PRIVATE_STATE_LEAVES {
        let path = process_home.join(leaf);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&path).with_context(|| format!("read_link {}", path.display()))?;
                if !target.starts_with(process_home) {
                    bail!(
                        "agy private state {} is a symlink to {} outside this run; \
                         concurrent runs must not share mutable agy artifacts",
                        path.display(),
                        target.display()
                    );
                }
            }
            Ok(_) | Err(_) => {}
        }
    }

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
    command.arg("models");
    environment.apply_to_command(&mut command);
    let output = command
        .output()
        .with_context(|| format!("running {} models", environment.agy_binary().display()))?;
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
mod tests {
    use super::*;
    use crate::scoped_home::SCOPED_HOME_ENV_TEST_LOCK;
    use crate::test_support::home_override;
    use std::ffi::OsString;
    use tempfile::TempDir;

    struct EnvGuard {
        _lock: Option<std::sync::MutexGuard<'static, ()>>,
        prior_homes: Option<OsString>,
        prior_skip: Option<OsString>,
        prior_bin_skip: Option<OsString>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            restore(AGY_HOMES_ROOT_ENV, self.prior_homes.as_ref());
            restore(AGY_SKIP_POSTURE_ASSERT_ENV, self.prior_skip.as_ref());
            restore(
                super::super::environment::AGY_SKIP_BINARY_ASSERT_ENV,
                self.prior_bin_skip.as_ref(),
            );
        }
    }

    fn restore(key: &str, prior: Option<&OsString>) {
        match prior {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    fn env_for_provision(homes: &Path) -> EnvGuard {
        let lock = SCOPED_HOME_ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        env_for_provision_with_lock(homes, Some(lock))
    }

    fn env_for_provision_with_home_override(homes: &Path) -> EnvGuard {
        env_for_provision_with_lock(homes, None)
    }

    fn env_for_provision_with_lock(homes: &Path, lock: Option<std::sync::MutexGuard<'static, ()>>) -> EnvGuard {
        let prior_homes = std::env::var_os(AGY_HOMES_ROOT_ENV);
        let prior_skip = std::env::var_os(AGY_SKIP_POSTURE_ASSERT_ENV);
        let prior_bin_skip = std::env::var_os(super::super::environment::AGY_SKIP_BINARY_ASSERT_ENV);
        unsafe {
            std::env::set_var(AGY_HOMES_ROOT_ENV, homes);
            std::env::set_var(AGY_SKIP_POSTURE_ASSERT_ENV, "1");
            std::env::set_var(super::super::environment::AGY_SKIP_BINARY_ASSERT_ENV, "1");
        }
        EnvGuard {
            _lock: lock,
            prior_homes,
            prior_skip,
            prior_bin_skip,
        }
    }

    #[test]
    fn process_home_for_run_is_under_homes_root() {
        let tmp = TempDir::new().unwrap();
        let _guard = env_for_provision(tmp.path());
        let home = process_home_for_run("run-abc").unwrap();
        assert!(home.starts_with(tmp.path()));
        assert!(home.ends_with("process-home"));
    }

    #[test]
    fn onboarding_json_skips_the_screen_without_answering_consent() {
        let parsed: serde_json::Value = serde_json::from_str(render_onboarding_json()).unwrap();
        assert_eq!(parsed["onboardingComplete"], true);
        assert_eq!(parsed["consumerOnboardingComplete"], true);
        assert!(parsed.get("enableTelemetry").is_none());
    }

    #[test]
    fn provision_creates_private_tree_and_keychain_bridge() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let real_home = tmp.path().join("real-home");
        let keychain_dir = real_home.join("Library/Keychains");
        fs::create_dir_all(&keychain_dir).unwrap();
        let keychain_file = keychain_dir.join("login.keychain-db");
        fs::write(&keychain_file, b"dummy-keychain").unwrap();
        fs::create_dir_all(real_home.join(".gemini/antigravity-cli")).unwrap();
        fs::write(
            real_home.join(".gemini/antigravity-cli/settings.json"),
            "{\"enableTelemetry\": true}\n",
        )
        .unwrap();
        let _home = home_override(&real_home);
        let _guard = env_for_provision_with_home_override(&homes);

        let runtime = provision_agy_home(&workspace, "hello prompt", "run-prov-1").unwrap();
        assert!(runtime.process_home.starts_with(&homes));
        assert_ne!(runtime.process_home, real_home);

        let gemini = runtime.process_home.join(".gemini");
        assert!(gemini.join("antigravity-cli/cache/onboarding.json").is_file());
        assert!(gemini.join("antigravity-cli/settings.json").is_file());
        assert!(gemini.join("config/hooks.json").is_file());
        assert!(gemini.join("AGENTS.md").parent().unwrap().is_dir());
        assert!(!gemini.join("settings.json").exists());
        assert!(!gemini.join("trustedFolders.json").exists());
        assert!(runtime.control_dir.join("initial-prompt.txt").is_file());
        assert_eq!(
            fs::read_to_string(runtime.control_dir.join("initial-prompt.txt")).unwrap(),
            "hello prompt"
        );
        assert_eq!(
            read_workspace_path_stamp(&runtime.control_dir).unwrap(),
            fs::canonicalize(&workspace).unwrap()
        );

        let bridged = runtime.process_home.join(LOGIN_KEYCHAIN_LEAF);
        let meta = fs::symlink_metadata(&bridged).expect("login keychain bridge");
        assert!(meta.file_type().is_symlink());
        assert_eq!(fs::read_link(&bridged).unwrap(), keychain_file);

        // Operator ~/.gemini must not be the run's .gemini.
        let operator_gemini = fs::canonicalize(real_home.join(".gemini")).unwrap();
        let run_gemini = fs::canonicalize(&gemini).unwrap();
        assert_ne!(operator_gemini, run_gemini);

        let settings = fs::read_to_string(gemini.join("antigravity-cli/settings.json")).unwrap();
        assert!(!settings.contains("enableTelemetry"));
        assert!(settings.contains("trustedWorkspaces"));

        assert_agy_posture(&runtime.process_home, &workspace).unwrap();
    }

    #[test]
    fn posture_fails_when_private_state_is_symlinked_out() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let real_home = tmp.path().join("real-home");
        fs::create_dir_all(&real_home).unwrap();
        let _home = home_override(&real_home);
        let _guard = env_for_provision_with_home_override(&homes);

        let runtime = provision_agy_home(&workspace, "p", "run-leak-1").unwrap();
        let dest = runtime
            .process_home
            .join(".gemini/antigravity-cli/conversation_summaries.db");
        let outside = tmp.path().join("shared-db");
        fs::write(&outside, b"shared").unwrap();
        let _ = fs::remove_file(&dest);
        std::os::unix::fs::symlink(&outside, &dest).unwrap();

        let err = assert_agy_posture(&runtime.process_home, &workspace)
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlink"), "{err}");
        assert!(err.contains("conversation_summaries.db"), "{err}");
    }

    #[test]
    fn two_runs_do_not_share_private_state_paths() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let a = process_home_for_run_with(&homes, "run-a");
        let b = process_home_for_run_with(&homes, "run-b");
        assert_ne!(a, b);
        assert_ne!(
            a.join(".gemini/antigravity-cli/settings.json"),
            b.join(".gemini/antigravity-cli/settings.json")
        );
    }

    fn process_home_for_run_with(homes: &Path, run_id: &str) -> PathBuf {
        let _guard = env_for_provision(homes);
        process_home_for_run(run_id).unwrap()
    }

    #[test]
    fn safe_to_delete_refuses_operator_home() {
        let tmp = TempDir::new().unwrap();
        let _guard = env_for_provision(&tmp.path().join("homes"));
        let err = assert_agy_home_safe_to_delete(Path::new("/Users/someone/.gemini"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside"), "{err}");
    }
}
