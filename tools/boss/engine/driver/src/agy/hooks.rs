//! Antigravity PreToolUse authorization: one adapter entry, reused guards.
//!
//! Progress stays silent on stdout. This module writes the engine-owned
//! policy, content-binding attestation, and the single `PreToolUse` command
//! that forwards, runs the existing guard chain, and returns `auto_approve`,
//! `deny`, or `deny_unless_prior_grant`. Native `SessionStart` is not a
//! pre-prompt readiness signal; loaded hook counts are not authorization.
//!
//! Session-settings rendering and the non-PreToolUse `boss-event` forwarder
//! map belong to their own wiring. This module merges the authorization
//! entry into whatever `hooks.json` is already on disk.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use boss_engine_codex_hook_trust::sha256_hex_prefixed;
use boss_ssh_transport::shell_quote;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::home::{agy_homes_root, process_home_for_run, session_settings_path_for_run};
use crate::claude::{
    BOSS_LAUNCH_GUARD_COMMAND, REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND, REVISION_PR_GUARD_COMMAND,
    pr_redirect_guard_command,
};
use crate::hook_settings::forwarder_command;
use crate::{ProgressObservationConfig, ToolUseInterceptionConfig, WorkerKind};

/// Filename of the authorization adapter under `$HOME/.gemini/config/`.
pub const ADAPTER_FILENAME: &str = "boss-agy-hook-adapter.py";
/// Engine-owned policy the adapter reads at `BOSS_AGY_POLICY`.
pub const POLICY_FILENAME: &str = "boss-agy-auth-policy.json";
/// Content binding for settings, hooks, adapter, policy, and guard artifacts.
pub const ATTESTATION_FILENAME: &str = "boss-agy-auth-attestation.json";

/// Name of the one hook group in `hooks.json` holding the Boss wiring.
pub(super) const HOOK_GROUP: &str = "boss";

/// Events wired to the forwarder: the six the studies measured firing in
/// interactive mode on every qualified binary.
pub(super) const AGY_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "PreInvocation",
    "PostInvocation",
    "PreToolUse",
    "PostToolUse",
    "Stop",
];

/// Events that take a tool-name matcher group; the others are bare handlers,
/// the shape the measurements used.
const MATCHER_EVENTS: &[&str] = &["PreToolUse", "PostToolUse"];

/// Per-handler timeout (seconds) for every forwarder hook.
///
/// `agy` runs hooks synchronously and kills a handler that outlives its
/// timeout, denying the tool when the handler was a `PreToolUse` one. The
/// shim bounds its own work at 12s (`SHIM_TOTAL_BUDGET` in
/// `tools/boss/event-shim`); 15s leaves it the same teardown headroom the
/// Grok forwarder gets, so a slow engine makes the shim buffer the event
/// instead of the hook being killed with the event unbuffered.
pub(super) const FORWARDER_HOOK_TIMEOUT_SEC: u64 = 15;

/// The shim's own budget (`SHIM_TOTAL_BUDGET`, 12s) plus teardown headroom.
const _: () = assert!(FORWARDER_HOOK_TIMEOUT_SEC >= 12 + 3);


const ADAPTER_SCRIPT: &str = include_str!("adapter.py");

/// Outer hook timeout when the checkleft push guard is not in the chain.
/// Covers silent `boss-event` delivery (~10s retry) plus the remaining guards.
pub const AUTHORIZATION_HOOK_TIMEOUT_SEC: u64 = 60;

struct Guard {
    command: String,
    matcher: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttestationEntry {
    pub name: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorizationAttestation {
    pub generated_at_unix: u64,
    pub entries: Vec<AttestationEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationProbeReport {
    pub ok: bool,
    pub positive_decision: String,
    pub denied_decision: String,
    pub denied_reason: Option<String>,
    pub reason: Option<String>,
}

fn worker_kind_name(kind: WorkerKind) -> &'static str {
    match kind {
        WorkerKind::Standard => "standard",
        WorkerKind::Reviewer => "reviewer",
        WorkerKind::Triage => "triage",
        WorkerKind::AnswerAgent => "answer_agent",
        WorkerKind::ReviewGuide => "review_guide",
        WorkerKind::Progression => "progression",
    }
}

fn approval_for(kind: WorkerKind) -> &'static str {
    match kind {
        WorkerKind::Standard | WorkerKind::Reviewer | WorkerKind::Triage => "auto_approve",
        WorkerKind::AnswerAgent | WorkerKind::ReviewGuide | WorkerKind::Progression => "deny_unless_prior_grant",
    }
}

fn gemini_config_dir(process_home: &Path) -> PathBuf {
    process_home.join(".gemini/config")
}

pub fn hooks_file_path(process_home: &Path) -> PathBuf {
    gemini_config_dir(process_home).join("hooks.json")
}

pub fn adapter_path(process_home: &Path) -> PathBuf {
    gemini_config_dir(process_home).join(ADAPTER_FILENAME)
}

pub fn policy_path(process_home: &Path) -> PathBuf {
    gemini_config_dir(process_home).join(POLICY_FILENAME)
}

pub fn attestation_path(process_home: &Path) -> PathBuf {
    gemini_config_dir(process_home).join(ATTESTATION_FILENAME)
}

fn guard_commands(config: &ToolUseInterceptionConfig) -> Vec<Guard> {
    let mut out = Vec::new();
    if let (Some(data_dir), Some(guard_script)) = (&config.data_dir, &config.path_guard_script) {
        out.push(Guard {
            command: format!(
                "BOSS_DATA_DIR={dir} python3 {script}",
                dir = shell_quote(&data_dir.display().to_string()),
                script = shell_quote(&guard_script.display().to_string()),
            ),
            matcher: "*",
        });
    }
    if let Some(guard_script) = &config.git_store_guard_script {
        out.push(Guard {
            command: crate::git_store_guard::hook_command(guard_script, config.workspace_path.as_deref()),
            matcher: "Bash",
        });
    }
    out.push(Guard {
        command: BOSS_LAUNCH_GUARD_COMMAND.to_owned(),
        matcher: "Bash",
    });
    if config.is_standard_worker {
        out.push(Guard {
            command: pr_redirect_guard_command(config.is_revision),
            matcher: "Bash",
        });
    }
    if config.is_standard_worker
        && let Some(checkleft_script) = &config.checkleft_guard_script
    {
        out.push(Guard {
            command: format!(
                "python3 {script}",
                script = shell_quote(&checkleft_script.display().to_string())
            ),
            matcher: "Bash",
        });
    }
    if config.is_reviewer {
        out.push(Guard {
            command: REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND.to_owned(),
            matcher: "Bash",
        });
    }
    if config.is_revision {
        out.push(Guard {
            command: REVISION_PR_GUARD_COMMAND.to_owned(),
            matcher: "Bash",
        });
    }
    if config.is_review_guide {
        out.push(Guard {
            command: format!(
                "python3 -c {}",
                shell_quote(&crate::codex::codex_review_guide_guard_script())
            ),
            matcher: ".*",
        });
    }
    out
}

fn write_executable(path: &Path, body: &str) -> anyhow::Result<()> {
    fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).with_context(|| format!("chmod +x {}", path.display()))?;
    }
    Ok(())
}

fn hash_file(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("hashing {}", path.display()))?;
    Ok(sha256_hex_prefixed(&bytes))
}

fn protected_paths(process_home: &Path, interception: &ToolUseInterceptionConfig) -> Vec<String> {
    let mut paths = vec![
        process_home.join(".gemini").display().to_string(),
        process_home
            .parent()
            .unwrap_or(process_home)
            .join("control")
            .display()
            .to_string(),
        process_home
            .parent()
            .unwrap_or(process_home)
            .join("rules-root")
            .display()
            .to_string(),
        agy_homes_root().display().to_string(),
    ];
    if let Some(data_dir) = &interception.data_dir {
        paths.push(data_dir.display().to_string());
    }
    paths
}

fn authorization_command(process_home: &Path, config: &ProgressObservationConfig) -> String {
    format!(
        "BOSS_EVENTS_SOCKET={socket} BOSS_LEASE_ID={lease} BOSS_RUN_ID={run_id} BOSS_WORKSPACE={workspace} BOSS_AGY_POLICY={policy} {adapter}",
        socket = shell_quote(&config.events_socket_path.display().to_string()),
        lease = shell_quote(&config.lease_id),
        run_id = shell_quote(&config.run_id),
        workspace = shell_quote(&config.workspace_path.display().to_string()),
        policy = shell_quote(&policy_path(process_home).display().to_string()),
        adapter = shell_quote(&adapter_path(process_home).display().to_string()),
    )
}

fn hook_timeout_sec(interception: &ToolUseInterceptionConfig) -> u64 {
    if interception.is_standard_worker && interception.checkleft_guard_script.is_some() {
        crate::CHECKLEFT_GUARD_HOOK_TIMEOUT_SECS
    } else {
        AUTHORIZATION_HOOK_TIMEOUT_SEC
    }
}

fn pre_tool_use_entry(
    process_home: &Path,
    obs: &ProgressObservationConfig,
    interception: &ToolUseInterceptionConfig,
) -> Value {
    json!({
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": authorization_command(process_home, obs),
            "timeout": hook_timeout_sec(interception),
        }],
    })
}

/// Declared PreToolUse wiring. Other lifecycle events are installed by the
/// global-root progress forwarder, not duplicated here.
pub fn authorization_hooks_map(
    config: &ProgressObservationConfig,
    interception: Option<&ToolUseInterceptionConfig>,
) -> serde_json::Map<String, Value> {
    let process_home = process_home_for_run(&config.run_id)
        .unwrap_or_else(|_| agy_homes_root().join("unknown-run").join("process-home"));
    let timeout = interception
        .map(hook_timeout_sec)
        .unwrap_or(AUTHORIZATION_HOOK_TIMEOUT_SEC);
    let entry = json!([{
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": authorization_command(&process_home, config),
            "timeout": timeout,
        }],
    }]);
    let mut hooks = forwarder_hooks_map(config);
    hooks.insert("PreToolUse".to_owned(), entry);
    hooks
}

fn is_provision_canary(entry: &Value) -> bool {
    entry
        .pointer("/hooks/0/command")
        .and_then(Value::as_str)
        .is_some_and(|command| command == "true")
}

fn named_hook_group(existing: Option<&Value>) -> (String, serde_json::Map<String, Value>) {
    let Some(object) = existing.and_then(Value::as_object) else {
        return ("hooks".to_owned(), serde_json::Map::new());
    };
    for name in ["boss", "hooks"] {
        if let Some(group) = object.get(name).and_then(Value::as_object) {
            return (name.to_owned(), group.clone());
        }
    }
    (
        "hooks".to_owned(),
        object.values().find_map(Value::as_object).cloned().unwrap_or_default(),
    )
}

fn merge_authorization_entry(existing: Option<Value>, pre_tool_use: Value) -> Value {
    let (group_name, mut hooks) = named_hook_group(existing.as_ref());
    if let Some(session_start) = hooks.get("SessionStart").and_then(Value::as_array)
        && session_start.iter().all(is_provision_canary)
    {
        hooks.remove("SessionStart");
    }
    hooks.insert("PreToolUse".to_owned(), json!([pre_tool_use]));
    json!({ group_name: hooks })
}

fn collect_guard_artifact_paths(config: &ToolUseInterceptionConfig) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Some(path) = &config.path_guard_script {
        out.push(("guard:path".into(), path.clone()));
    }
    if let Some(path) = &config.git_store_guard_script {
        out.push(("guard:git_store".into(), path.clone()));
    }
    if let Some(path) = &config.checkleft_guard_script {
        out.push(("guard:checkleft".into(), path.clone()));
    }
    out
}

/// Install the adapter, policy, attestation, and PreToolUse authorization
/// entry under the per-run home. Refuses to arm zero guards.
pub fn install_authorization(
    process_home: &Path,
    worker_kind: WorkerKind,
    obs: &ProgressObservationConfig,
    interception: &ToolUseInterceptionConfig,
) -> anyhow::Result<Vec<PathBuf>> {
    if !process_home.exists() {
        bail!(
            "agy authorization install: process HOME {} does not exist; call provision_workspace first",
            process_home.display()
        );
    }
    let guards = guard_commands(interception);
    if guards.is_empty() {
        bail!("AgyDriver refuses to arm zero PreToolUse guards (ToolUseInterception declared)");
    }

    let config_dir = gemini_config_dir(process_home);
    fs::create_dir_all(&config_dir).with_context(|| format!("creating {}", config_dir.display()))?;

    let adapter = adapter_path(process_home);
    write_executable(&adapter, ADAPTER_SCRIPT)?;

    let policy_file = policy_path(process_home);
    let attestation_file = attestation_path(process_home);
    let hooks_path = hooks_file_path(process_home);
    let settings_path = session_settings_path_for_run(&obs.run_id).unwrap_or_else(|_| {
        process_home
            .parent()
            .unwrap_or(process_home)
            .join("control/session-settings.json")
    });

    let policy = json!({
        "version": 1,
        "run_id": obs.run_id,
        "worker_kind": worker_kind_name(worker_kind),
        "approval": approval_for(worker_kind),
        "workspace_path": obs.workspace_path.display().to_string(),
        "process_home": process_home.display().to_string(),
        "protected_paths": protected_paths(process_home, interception),
        "forwarder": obs.forwarder_binary.display().to_string(),
        "attestation_path": attestation_file.display().to_string(),
        "guards": guards.iter().map(|guard| json!({
            "command": guard.command,
            "matcher": guard.matcher,
        })).collect::<Vec<_>>(),
    });
    fs::write(&policy_file, serde_json::to_string_pretty(&policy)? + "\n")
        .with_context(|| format!("writing {}", policy_file.display()))?;

    let existing = fs::read_to_string(&hooks_path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok());
    let hooks_doc = merge_authorization_entry(existing, pre_tool_use_entry(process_home, obs, interception));
    fs::write(&hooks_path, serde_json::to_string_pretty(&hooks_doc)? + "\n")
        .with_context(|| format!("writing {}", hooks_path.display()))?;

    let mut entries = vec![
        AttestationEntry {
            name: "adapter".into(),
            path: adapter.display().to_string(),
            sha256: hash_file(&adapter)?,
        },
        AttestationEntry {
            name: "hooks".into(),
            path: hooks_path.display().to_string(),
            sha256: hash_file(&hooks_path)?,
        },
        AttestationEntry {
            name: "policy".into(),
            path: policy_file.display().to_string(),
            sha256: hash_file(&policy_file)?,
        },
    ];
    if settings_path.is_file() {
        entries.push(AttestationEntry {
            name: "settings".into(),
            path: settings_path.display().to_string(),
            sha256: hash_file(&settings_path)?,
        });
    }
    for (name, path) in collect_guard_artifact_paths(interception) {
        if path.is_file() {
            entries.push(AttestationEntry {
                name,
                path: path.display().to_string(),
                sha256: hash_file(&path)?,
            });
        }
    }
    let attestation = AuthorizationAttestation {
        generated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        entries,
    };
    fs::write(&attestation_file, serde_json::to_string_pretty(&attestation)? + "\n")
        .with_context(|| format!("writing {}", attestation_file.display()))?;

    Ok(vec![hooks_path, adapter, policy_file, attestation_file])
}

/// Re-check that every attested artifact still has the bytes written at install.
pub fn verify_attestation(process_home: &Path) -> anyhow::Result<AuthorizationAttestation> {
    let path = attestation_path(process_home);
    let body = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let attestation: AuthorizationAttestation =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    if attestation.entries.is_empty() {
        bail!("authorization attestation at {} has no entries", path.display());
    }
    let mut problems = Vec::new();
    for entry in &attestation.entries {
        let artifact = PathBuf::from(&entry.path);
        if !artifact.is_file() {
            problems.push(format!("{} missing: {}", entry.name, entry.path));
            continue;
        }
        let actual = hash_file(&artifact)?;
        if actual != entry.sha256 {
            problems.push(format!("{} changed: {}", entry.name, entry.path));
        }
    }
    if !problems.is_empty() {
        bail!("authorization binding failed: {}", problems.join("; "));
    }
    Ok(attestation)
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Harmless pre-prompt probe through the same authorization entry. Does not
/// launch a pane. Native SessionStart cannot establish this; hook counts cannot
/// either.
pub fn probe_authorization(process_home: &Path) -> anyhow::Result<AuthorizationProbeReport> {
    let binding = verify_attestation(process_home);
    let binding_ok = binding.is_ok();
    if !python3_available() {
        bail!("python3 is required to probe Antigravity authorization");
    }
    let policy = policy_path(process_home);
    if !policy.is_file() {
        bail!("authorization policy missing at {}", policy.display());
    }
    let adapter = adapter_path(process_home);
    let output = Command::new("python3")
        .arg(&adapter)
        .arg("--probe")
        .env("BOSS_AGY_POLICY", &policy)
        .env("BOSS_AGY_SKIP_FORWARD", "1")
        .output()
        .with_context(|| format!("running {} --probe", adapter.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim()).with_context(|| {
        format!(
            "authorization probe produced non-JSON stdout (status {:?}): {stdout}",
            output.status.code()
        )
    })?;
    let report = AuthorizationProbeReport {
        ok: parsed.get("ok").and_then(Value::as_bool).unwrap_or(false) && binding_ok,
        positive_decision: parsed
            .pointer("/positive/decision")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        denied_decision: parsed
            .pointer("/denied/decision")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        denied_reason: parsed
            .pointer("/denied/reason")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reason: parsed
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| binding.err().map(|error| error.to_string())),
    };
    if !report.ok {
        bail!(
            "pre-prompt authorization probe failed: {}",
            report
                .reason
                .as_deref()
                .unwrap_or("positive or denied sentinel mismatch")
        );
    }
    Ok(report)
}

/// Build [`ToolUseInterceptionConfig`] from permission input the same way
/// Grok and Codex do, so the reused guards see identical flags.
pub fn interception_from_permission(input: &crate::PermissionInput) -> ToolUseInterceptionConfig {
    ToolUseInterceptionConfig {
        data_dir: if input.is_remote {
            None
        } else {
            input.events_socket_path.parent().map(|p| p.to_path_buf())
        },
        path_guard_script: if input.is_remote {
            None
        } else {
            input.path_guard_script.clone()
        },
        checkleft_guard_script: if input.is_remote {
            None
        } else {
            input.checkleft_guard_script.clone()
        },
        git_store_guard_script: if input.is_remote {
            None
        } else {
            input.git_store_guard_script.clone()
        },
        is_revision: input.execution_kind == "revision_implementation"
            || input.task_kind.as_deref() == Some("revision"),
        is_standard_worker: input.worker_kind == WorkerKind::Standard,
        is_reviewer: input.worker_kind == WorkerKind::Reviewer,
        is_review_guide: input.worker_kind == WorkerKind::ReviewGuide,
        run_id: Some(input.run_id.clone()),
        workspace_path: Some(input.workspace_path.clone()),
    }
}

pub fn observation_from_permission(input: &crate::PermissionInput) -> ProgressObservationConfig {
    ProgressObservationConfig {
        events_socket_path: input.events_socket_path.clone(),
        lease_id: input.lease_id.clone(),
        run_id: input.run_id.clone(),
        workspace_path: input.workspace_path.clone(),
        forwarder_binary: input.boss_event_path.clone(),
    }
}

/// The shell command one event runs: the shared forwarder command with the
/// event name as its trailing argument.
fn event_command(config: &ProgressObservationConfig, event: &str) -> String {
    format!("{} {}", forwarder_command(config), shell_quote(event))
}

/// Event name -> `agy` hook entries, every [`AGY_HOOK_EVENTS`] event wired to
/// the forwarder. Returned by `progress_observation_wiring` so the declared
/// wiring is what [`write_hooks`] installs.
pub(super) fn forwarder_hooks_map(config: &ProgressObservationConfig) -> serde_json::Map<String, Value> {
    let mut hooks = serde_json::Map::new();
    for event in AGY_HOOK_EVENTS {
        let handler = json!({
            "type": "command",
            "command": event_command(config, event),
            "timeout": FORWARDER_HOOK_TIMEOUT_SEC,
        });
        let entry = if MATCHER_EVENTS.contains(event) {
            json!({"matcher": "*", "hooks": [handler]})
        } else {
            handler
        };
        hooks.insert((*event).to_owned(), json!([entry]));
    }
    hooks
}

/// Render the whole `hooks.json` body for the forwarder wiring.
pub(super) fn render_hooks_json(config: &ProgressObservationConfig) -> anyhow::Result<String> {
    let body = json!({ HOOK_GROUP: forwarder_hooks_map(config) });
    Ok(serde_json::to_string_pretty(&body)? + "\n")
}

/// Overwrite the per-run home's `hooks.json` (provisioned as a no-op canary)
/// with the forwarder wiring. Returns the path written.
pub(super) fn write_hooks(process_home: &Path, config: &ProgressObservationConfig) -> anyhow::Result<PathBuf> {
    let path = hooks_file_path(process_home);
    let parent = path.parent().context("hooks.json has a parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    // Write beside the target and rename so a reader never sees a partial file.
    let staging = path.with_extension("json.tmp");
    fs::write(&staging, render_hooks_json(config)?).with_context(|| format!("writing {}", staging.display()))?;
    fs::rename(&staging, &path).with_context(|| format!("installing {}", path.display()))?;
    Ok(path)
}


#[cfg(test)]
#[path = "hooks_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "forwarder_tests.rs"]
mod forwarder_tests;
