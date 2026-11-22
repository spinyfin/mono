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

use super::environment::{AgyProcessEnvironment, resolve_agy_binary};
#[cfg(test)]
use crate::scoped_home::LOGIN_KEYCHAIN_LEAF;
use crate::transcript_store::{
    AGY_BRAIN_LEAF, AGY_TRANSCRIPT_DRIVER, durable_transcript_containment_root, provision_durable_link_in,
    transcript_store_root, verified_durable_link,
};

/// Process-global lock for tests that mutate [`AGY_HOMES_ROOT_ENV`]. Shared
/// with Grok via [`crate::scoped_home::SCOPED_HOME_ENV_TEST_LOCK`] so the
/// two drivers cannot race their homes-root overrides.
pub use crate::scoped_home::SCOPED_HOME_ENV_TEST_LOCK as AGY_HOMES_ENV_TEST_LOCK;

/// Env override for the root under which per-run `agy` homes live.
pub const AGY_HOMES_ROOT_ENV: &str = "BOSS_AGY_HOMES_DIR";

/// When set to `1`/`true`, skip the live `agy` posture probe. This is the only
/// switch for the probe: skipping the binary identity checks does not skip it,
/// so a stand-in binary can take the vendor CLI's place in a provisioning test.
pub const AGY_SKIP_POSTURE_ASSERT_ENV: &str = "BOSS_AGY_SKIP_POSTURE_ASSERT";

/// Default leaf under the system temp when [`AGY_HOMES_ROOT_ENV`] is unset.
pub(crate) const AGY_HOMES_DIR_NAME: &str = "boss-agy-homes";

pub(crate) const GEMINI_DIR: &str = ".gemini";
pub(crate) const STATE_DIR: &str = "antigravity-cli";
pub(crate) const PROCESS_HOME_LEAF: &str = "process-home";
/// Transcript paths relative to the container, for homes provisioned **before**
/// the durable store existed. Their transcripts exist only here, so teardown
/// and retention keep them. A home whose `brain` is the durable link (every
/// home [`provision_agy_home`] creates) holds no such evidence; see
/// [`contains_agy_transcripts`].
pub const AGY_TRANSCRIPT_PATHS: &[&[&str]] = &[
    &[PROCESS_HOME_LEAF, ".gemini", "antigravity-cli", "conversations"],
    &[PROCESS_HOME_LEAF, ".gemini", "antigravity-cli", AGY_BRAIN_LEAF],
];

/// Whether reclaiming `container` would destroy transcript evidence that has
/// no other copy. Fails closed when transcript evidence cannot be read.
///
/// A home provisioned with the durable store links `brain` into
/// `<state>/executions/<run>/transcripts/agy/sessions`, so the transcript
/// survives the home and the home is freely reclaimable — including its
/// `conversations/` database, which is CLI state, not Boss evidence. Only a
/// legacy home (a real `brain/` directory) is protected.
pub fn contains_agy_transcripts(container: &Path) -> bool {
    if container.is_file() {
        return false;
    }
    if brain_link_is_symlink(&container.join(PROCESS_HOME_LEAF)) {
        return false;
    }
    AGY_TRANSCRIPT_PATHS.iter().any(|parts| {
        let path = parts.iter().fold(container.to_path_buf(), |path, part| path.join(part));
        match fs::read_dir(path) {
            Ok(mut entries) => entries.next().is_some(),
            Err(error) => error.kind() != std::io::ErrorKind::NotFound,
        }
    })
}

/// `<process-home>/.gemini/antigravity-cli`.
fn agy_state_dir(process_home: &Path) -> PathBuf {
    process_home.join(GEMINI_DIR).join(STATE_DIR)
}

/// The CLI's own `settings.json`: `<process-home>/.gemini/antigravity-cli/settings.json`.
/// Boss seeds it and the CLI rewrites it on every start; see
/// [`restore_private_cli_settings_after_cli_start`].
fn private_cli_settings_path(process_home: &Path) -> PathBuf {
    agy_state_dir(process_home).join("settings.json")
}

/// The path of the durable-transcript link: `<process-home>/.gemini/antigravity-cli/brain`.
pub(super) fn brain_link(process_home: &Path) -> PathBuf {
    agy_state_dir(process_home).join(AGY_BRAIN_LEAF)
}

fn brain_link_is_symlink(process_home: &Path) -> bool {
    fs::symlink_metadata(brain_link(process_home)).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// The run-id segment this home's durable transcripts are filed under: the
/// container directory's name, i.e. the *sanitized* run id. Deriving it from
/// the path rather than from the caller's raw run id keeps provisioning,
/// verification, containment and the stamped-path rewrite in agreement even
/// for a run id the sanitizer changes.
fn durable_run_id(process_home: &Path) -> anyhow::Result<String> {
    process_home
        .parent()
        .and_then(Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_owned)
        .with_context(|| format!("agy process HOME {} has no run container name", process_home.display()))
}

/// Link `brain` to this run's durable transcript directory.
///
/// A provisioning error, never a best-effort completion step: a worker that is
/// killed or orphaned is still writing through the link, and a failed setup
/// must be visible before the CLI starts.
fn provision_durable_brain(process_home: &Path) -> anyhow::Result<PathBuf> {
    let run_id = durable_run_id(process_home)?;
    let store_root = transcript_store_root()?;
    provision_durable_link_in(&brain_link(process_home), &store_root, AGY_TRANSCRIPT_DRIVER, &run_id).with_context(
        || {
            format!(
                "linking durable transcript storage into agy HOME {}",
                process_home.display()
            )
        },
    )
}

/// Verify an installed `brain` link targets exactly this run's durable
/// directory. A link that targets anything else is refused, never followed.
fn verify_durable_brain(process_home: &Path) -> anyhow::Result<PathBuf> {
    let run_id = durable_run_id(process_home)?;
    let store_root = transcript_store_root()?;
    verified_durable_link(&brain_link(process_home), &store_root, AGY_TRANSCRIPT_DRIVER, &run_id)
}

/// The canonical directory a local `agy` transcript for `run_id` must remain
/// beneath at every read.
///
/// `Ok(Some)` for a home provisioned with the durable store — whether the home
/// still exists (the link is verified) or has been reclaimed (the durable
/// directory outlives it). `Ok(None)` for a legacy home whose `brain` is a real
/// directory, or a run that never provisioned one: the stamped path stays
/// authoritative there. A link that targets anything but this run's durable
/// directory is an `Err`, so a retargeted link makes consumers reject the
/// transcript rather than read through it.
pub(super) fn transcript_containment_root(run_id: &str) -> anyhow::Result<Option<PathBuf>> {
    let process_home = process_home_for_run(run_id)?;
    durable_transcript_containment_root(
        &brain_link(&process_home),
        AGY_TRANSCRIPT_DRIVER,
        &durable_run_id(&process_home)?,
    )
}
/// Global-root hook directory, relative to the per-run `HOME`.
const HOOKS_DIR_RELATIVE: &str = ".gemini/config";
const CONTROL_LEAF: &str = "control";
/// Rules-only directory passed to the CLI with `--add-dir`.
const RULES_ROOT_LEAF: &str = "rules-root";
/// Boss worker rules, at the root of the `--add-dir` directory. `agy` loads
/// `AGENTS.md` there as an always-on rule. A file under `.agents/rules/` is
/// not: it needs `trigger: always_on` frontmatter, which the shared rules
/// renderer does not emit, and without it the rule is silently never loaded
/// (live rules-discovery probe, `agy` 1.5.9).
const RULES_FILENAME: &str = "AGENTS.md";
const WORKSPACE_PATH_FILENAME: &str = "boss-workspace-path";

const ONBOARDING_JSON: &str = r#"{"consumerOnboardingComplete": true, "enterpriseOnboardingComplete": false, "onboardingComplete": true}
"#;

/// Check existing paths before creating or overwriting any run state.
pub(super) fn validate_private_layout(process_home: &Path) -> anyhow::Result<()> {
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
    // The one exception is the `brain` link Boss itself installs into the
    // durable transcript store: it must target exactly this run's durable
    // directory, and is not descended into (what lies beyond it is the store's,
    // not this private tree's).
    let brain_is_durable_link = brain_link_is_symlink(process_home);
    if brain_is_durable_link {
        verify_durable_brain(process_home)?;
    }
    let brain = brain_link(process_home);
    check_private_tree(
        &process_home.join(".gemini"),
        brain_is_durable_link.then_some(brain.as_path()),
    )?;
    check_private_tree(
        &process_home.parent().context("run container")?.join(CONTROL_LEAF),
        None,
    )?;
    check_private_tree(
        &process_home.parent().context("run container")?.join("rules-root"),
        None,
    )?;
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

fn check_private_tree(path: &Path, verified_durable_link: Option<&Path>) -> anyhow::Result<()> {
    if verified_durable_link == Some(path) {
        return Ok(());
    }
    check_private_entry(path)?;
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            check_private_tree(&entry?.path(), verified_durable_link)?;
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

/// The private CLI `settings.json` (trusted workspaces — every spelling of
/// the working directory; the CLI also saves its own always-allow grants
/// here) for the HOME whose hook directory is `hooks_dir`
/// (`<process-home>/.gemini/config`).
pub fn cli_settings_path_for_hooks_dir(hooks_dir: &Path) -> PathBuf {
    let gemini = hooks_dir.parent().unwrap_or(hooks_dir);
    gemini.join("antigravity-cli").join("settings.json")
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
    Ok(agy_run_container_for_run(run_id)?
        .join(RULES_ROOT_LEAF)
        .join(RULES_FILENAME))
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

pub fn render_private_cli_settings_json(workspace: &Path) -> String {
    let paths = trust_path_variants(workspace);
    serde_json::to_string_pretty(&serde_json::json!({
        "trustedWorkspaces": paths,
        "queuedMessages": super::permissions::QUEUED_MESSAGES,
    }))
    .expect("private CLI settings (trustedWorkspaces, queuedMessages) are serializable")
        + "\n"
}

pub fn render_session_settings_json(workspace: &Path) -> String {
    // Trust and queued input. Permission settings come from the permission renderer.
    // Skipping onboarding does not answer data-collection consent;
    // enableTelemetry is omitted.
    render_private_cli_settings_json(workspace)
}

/// The file's top-level key names a hook group and `SessionStart` is a bare
/// handler, the shape `install_authorization` writes. Any other shape is
/// rejected by the CLI when the hook runs ("command hook must specify
/// 'command'") while the session carries on without hooks.
fn render_provision_hooks_json() -> String {
    r#"{
  "boss": {
    "SessionStart": [
      {"type": "command", "command": "true"}
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
    provision_home(workspace, prompt_text, run_id, true)
}

pub(super) fn quota_probe_run_id() -> String {
    format!("quota-probe-{}", std::process::id())
}

/// The quota command itself exercises authentication; avoid a redundant models request.
pub(super) fn provision_quota_home(workspace: &Path) -> anyhow::Result<AgyRuntimeState> {
    provision_home(workspace, "unused", &quota_probe_run_id(), false)
}

fn provision_home(
    workspace: &Path,
    prompt_text: &str,
    run_id: &str,
    probe_models: bool,
) -> anyhow::Result<AgyRuntimeState> {
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
    // Worker homes: `brain/<cid>/.system_generated/logs/transcript_full.jsonl`
    // is the file every hook payload stamps as `transcriptPath`. Link it into
    // the durable per-execution store now, before the CLI can write a
    // conversation, so the transcript outlives this home.
    // Quota probes are not recorded on work_executions, so the retention
    // sweep never owns their store entries; they skip the durable link and
    // delete the whole container after the command (see reclaim_quota_probe_container).
    if probe_models {
        provision_durable_brain(&process_home)?;
    }
    let hooks_dir = process_home.join(HOOKS_DIR_RELATIVE);
    fs::create_dir_all(&hooks_dir).with_context(|| format!("creating hooks directory {}", hooks_dir.display()))?;

    // Onboarding screen skip. This is not a data-collection consent answer:
    // enableTelemetry is not written, and the Terms checkbox is not ticked.
    fs::write(antigravity.join("cache/onboarding.json"), render_onboarding_json())
        .with_context(|| format!("writing onboarding.json under {}", antigravity.display()))?;
    write_private_cli_settings(&process_home, workspace)?;
    fs::write(&session_settings_path, render_session_settings_json(workspace))
        .with_context(|| format!("writing session settings {}", session_settings_path.display()))?;
    // SessionStart `true` canary so the global-root file exists before
    // `write_permission_config` installs PreToolUse authorization.
    // `install_authorization` overwrites this file with the Boss hook group;
    // nothing else written here survives installation.
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

    // A relaunch must observe this CLI's initialization, never an old log.
    match fs::remove_file(&log_file) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("removing previous agy launch log"),
    }
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

    if probe_models && !skip_posture_assert() {
        probe_agy_models(&environment).context("live agy models posture probe")?;
        // The probe is a CLI start in this home, and a start does not leave
        // the private settings as they were written above.
        restore_private_cli_settings_after_cli_start(&process_home, workspace)
            .context("restoring the private CLI settings after the agy models probe")?;
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

/// Write the private CLI settings the next `agy` start in this home reads.
fn write_private_cli_settings(process_home: &Path, workspace: &Path) -> anyhow::Result<()> {
    let path = private_cli_settings_path(process_home);
    fs::write(&path, render_private_cli_settings_json(workspace))
        .with_context(|| format!("writing private CLI settings {}", path.display()))
}

/// Put the private CLI settings back after an `agy` start in this home, and
/// check the posture of the home as the pane launch will find it.
///
/// Provisioning ends here so the returned home holds the settings Boss
/// rendered rather than what the CLI wrote back during the probe. The posture
/// is asserted on that final state. The private tree is validated first so
/// nothing the CLI created is written through. Changed keys are logged by
/// name.
///
/// An `agy` start rewrites `settings.json` while it loads its settings: it
/// parses the file, serialises it again in its own sparse form, and writes the
/// result back when the bytes differ. The sparse form leaves out every key
/// whose value equals the CLI's built-in default, and `"queue"` is the built-in
/// default for `queuedMessages`. So the pin [`assert_agy_posture`] requires
/// does not survive a start, the `models` probe included. Read from the pinned
/// 1.5.9 binary: `store.NewManager` runs `initCliSettings` → `ReadCliSettings`
/// → `validateCliSettings`, which calls `CliSetting.MarshalSparse` (a per-field
/// comparison against the default settings), compares the result with the
/// bytes on disk and calls `WriteCliSettings`; `models` is dispatched after
/// `NewManager` returns.
///
/// The pane launch is a start too, so the pin is gone again once the worker is
/// running. [`super::AgyDriver::spawn_invocation`] is the last posture check
/// of this file, and it runs before the CLI starts in the pane. A retry
/// re-provisions first. After that start, the mode a session runs in comes
/// from `--settings`, which the CLI never writes back.
fn restore_private_cli_settings_after_cli_start(process_home: &Path, workspace: &Path) -> anyhow::Result<()> {
    validate_private_layout(process_home)?;
    let path = private_cli_settings_path(process_home);
    let rendered = render_private_cli_settings_json(workspace);
    let left = fs::read_to_string(&path).unwrap_or_default();
    if left != rendered {
        let (dropped, changed, added) = settings_key_changes(&rendered, &left);
        tracing::info!(
            process_home = %process_home.display(),
            ?dropped,
            ?changed,
            ?added,
            "agy rewrote its private settings during the models probe; rendering them again for the launch",
        );
        write_private_cli_settings(process_home, workspace)?;
    }
    assert_agy_posture(process_home, workspace)
}

/// Top-level keys that `after` lacks, holds with another value, or holds in
/// addition, relative to `before`. Text that is not a JSON object has no keys.
fn settings_key_changes(before: &str, after: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let keys =
        |text: &str| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(text).unwrap_or_default();
    let (before, after) = (keys(before), keys(after));
    let dropped = before.keys().filter(|key| !after.contains_key(*key)).cloned().collect();
    let changed = before
        .iter()
        .filter(|(key, value)| after.get(*key).is_some_and(|now| now != *value))
        .map(|(key, _)| key.clone())
        .collect();
    let added = after.keys().filter(|key| !before.contains_key(*key)).cloned().collect();
    (dropped, changed, added)
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

    let settings =
        fs::read_to_string(private_cli_settings_path(process_home)).context("reading private CLI settings.json")?;
    let settings_json: serde_json::Value =
        serde_json::from_str(&settings).context("parsing private CLI settings.json")?;
    if settings_json.get("enableTelemetry").is_some() {
        bail!("private CLI settings must not answer data-collection consent (enableTelemetry)");
    }
    let queued_messages = settings_json.get("queuedMessages");
    if queued_messages.and_then(|value| value.as_str()) != Some(super::permissions::QUEUED_MESSAGES) {
        // Say what the file holds: a missing key and another mode have
        // different causes, and the home is reclaimed with the failed spawn.
        match queued_messages {
            Some(found) => bail!("private CLI settings must pin queuedMessages to queue (found {found})"),
            None => bail!(
                "private CLI settings must pin queuedMessages to queue (found no queuedMessages key, \
                 which is how an agy start leaves the file)"
            ),
        }
    }
    let trusted = trusted_workspace_list(&settings_json);
    assert_working_directory_pre_trusted("private CLI settings.json", &trusted, workspace)?;

    // `--settings` overrides the private file at pane launch, so a home whose
    // private file is complete but whose session document lists only the
    // canonical spelling still opens the folder-trust dialog.
    if let Some(session_path) = process_home
        .parent()
        .map(|container| container.join(CONTROL_LEAF).join("session-settings.json"))
        .filter(|path| path.is_file())
    {
        let session = fs::read_to_string(&session_path)
            .with_context(|| format!("reading session settings {}", session_path.display()))?;
        let session_json: serde_json::Value =
            serde_json::from_str(&session).context("parsing session --settings JSON")?;
        assert_working_directory_pre_trusted(
            "session --settings file",
            &trusted_workspace_list(&session_json),
            workspace,
        )?;
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

fn trusted_workspace_list(settings: &serde_json::Value) -> Vec<String> {
    settings
        .get("trustedWorkspaces")
        .and_then(|value| value.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// `agy` matches `trustedWorkspaces` by exact string. A missing `/var` or
/// `/tmp` spelling of the cwd is the folder-trust dialog, not a later prompt.
fn assert_working_directory_pre_trusted(source: &str, trusted: &[String], workspace: &Path) -> anyhow::Result<()> {
    if !workspace.is_absolute() {
        bail!(
            "{source} cannot pre-trust relative working directory {}; \
             the launch would show the folder-trust dialog",
            workspace.display()
        );
    }
    let required = trust_path_variants(workspace);
    let missing: Vec<&String> = required
        .iter()
        .filter(|path| !trusted.iter().any(|got| got == *path))
        .collect();
    if !missing.is_empty() {
        bail!(
            "{source} does not pre-trust every spelling of the working directory {}; \
             missing {missing:?} (need {required:?}, got {trusted:?}); \
             the launch would show the folder-trust dialog",
            workspace.display()
        );
    }
    Ok(())
}

fn probe_agy_models(environment: &AgyProcessEnvironment) -> anyhow::Result<()> {
    let environment = AgyProcessEnvironment::resolve(
        environment.process_home(),
        &environment.log_file().with_file_name("agy-models-probe.log"),
        environment.session_settings_path(),
        environment.agy_binary().to_path_buf(),
    )?;
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

/// Delete a quota-probe container entirely.
///
/// Quota probes are not recorded on `work_executions`, so the agy
/// home-retention sweep never owns them. Worker teardown
/// ([`reclaim_agy_run`]) keeps `control/` and the durable transcript target;
/// that would leak a `quota-probe-<pid>` directory on every engine restart.
pub(super) fn reclaim_quota_probe_container(container: &Path) -> anyhow::Result<()> {
    assert_agy_home_safe_to_delete(container)?;
    match fs::remove_dir_all(container) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("deleting quota probe container {}", container.display())),
    }
}

/// Remove run state at teardown.
///
/// Keep the control directory (CLI log, launcher diagnostics and settings)
/// until the bounded agy home-retention sweep expires it. Teardown also runs
/// on failed spawns, before a conversation or transcript need exist.
/// A home provisioned with the durable store keeps no process state: `brain/` is a link
/// whose target — the `transcript_full.jsonl` the Stop payload's
/// `transcriptPath` names — lives under the per-execution store, and removing
/// the link leaves that target untouched. A legacy home (a real `brain/`
/// directory, provisioned before the store existed) still keeps `brain/` and
/// `conversations/`, because they hold the only copy.
pub fn reclaim_agy_run(process_home: &Path) -> anyhow::Result<()> {
    let container = process_home.parent().context("agy run container")?;
    assert_agy_home_safe_to_delete(container)?;
    validate_private_layout(process_home)?;
    let mut keep: Vec<Vec<&str>> = if brain_link_is_symlink(process_home) {
        Vec::new()
    } else {
        AGY_TRANSCRIPT_PATHS.iter().map(|path| path.to_vec()).collect()
    };
    keep.push(vec![CONTROL_LEAF]);
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
