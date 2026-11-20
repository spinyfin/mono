//! Antigravity PreToolUse authorization: one adapter entry, reused guards.
//!
//! Progress stays silent on stdout. This module writes the engine-owned
//! policy, content-binding attestation, and the single `PreToolUse` command
//! that forwards, runs the existing guard chain, and returns `auto_approve`,
//! `deny`, or `deny_unless_prior_grant`. Native `SessionStart` is not a
//! pre-prompt readiness signal; loaded hook counts are not authorization.
//!
//! The global-root progress map wires all six measured lifecycle events.
//! Authorization replaces its single PreToolUse entry before atomic
//! installation and content binding; the other five remain forwarders.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use boss_engine_codex_hook_trust::sha256_hex_prefixed;
use boss_engine_structured_output::StructuredOutputKind;
use boss_ssh_transport::shell_quote;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::home::{agy_homes_root, brain_link, process_home_for_run, session_settings_path_for_run};
use crate::hook_guards::{Guard, guard_commands, write_executable};
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

#[path = "startup.rs"]
mod startup;
pub use startup::verify_startup_authorization;

/// Outer hook timeout for the PreToolUse authorization entry. One value for
/// every worker kind, so the entry the progress wiring declares (which cannot
/// see whether a checkleft guard is armed) is always the entry installed. It
/// is sized for the checkleft push guard, the longest guard in the chain; the
/// other guards enforce their own, much shorter, budgets inside the adapter.
pub const AUTHORIZATION_HOOK_TIMEOUT_SEC: u64 = crate::CHECKLEFT_GUARD_HOOK_TIMEOUT_SECS;

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

fn private_forwarder_config(process_home: &Path, config: &ProgressObservationConfig) -> ProgressObservationConfig {
    ProgressObservationConfig {
        forwarder_binary: gemini_config_dir(process_home).join("boss-event"),
        ..config.clone()
    }
}

fn artifact_write_paths_json(worker_kind: WorkerKind, run_id: &str) -> Value {
    match worker_kind {
        WorkerKind::Reviewer => json!([boss_engine_structured_output::default_path_string(
            run_id,
            StructuredOutputKind::ReviewResult,
        )]),
        _ => json!([]),
    }
}

fn authorization_command(process_home: &Path, config: &ProgressObservationConfig) -> String {
    let adapter_config = ProgressObservationConfig {
        forwarder_binary: adapter_path(process_home),
        ..config.clone()
    };
    format!(
        "BOSS_AGY_FORWARDER={} BOSS_AGY_POLICY={} {}",
        shell_quote(&config.forwarder_binary.display().to_string()),
        shell_quote(&policy_path(process_home).display().to_string()),
        forwarder_command(&adapter_config)
    )
}

fn pre_tool_use_entry(process_home: &Path, obs: &ProgressObservationConfig) -> Value {
    json!({
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": authorization_command(process_home, obs),
            "timeout": AUTHORIZATION_HOOK_TIMEOUT_SEC,
        }],
    })
}

/// Returns the six-event forwarder map with its PreToolUse entry replaced by
/// the authorization adapter. Fails when the run's private home cannot be
/// resolved, rather than declaring a path that does not exist.
pub fn authorization_hooks_map(config: &ProgressObservationConfig) -> anyhow::Result<serde_json::Map<String, Value>> {
    let process_home = process_home_for_run(&config.run_id)?;
    let config = private_forwarder_config(&process_home, config);
    let mut hooks = forwarder_hooks_map(&config);
    hooks.insert(
        "PreToolUse".to_owned(),
        json!([pre_tool_use_entry(&process_home, &config)]),
    );
    Ok(hooks)
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

/// Guard commands the execution requires: the shared Boss chain plus the
/// adapter-only tool-surface and Reviewer/Triage publish guards.
fn authorization_guards(worker_kind: WorkerKind, interception: &ToolUseInterceptionConfig) -> Vec<Guard> {
    let mut guards = guard_commands(interception);
    guards.push(Guard {
        command: format!(
            "python3 -c {}",
            shell_quote(&crate::codex::codex_tool_surface_guard_script())
        ),
        matcher: "*",
        timeout_sec: Some(30),
        run_once: false,
    });
    if matches!(worker_kind, WorkerKind::Reviewer | WorkerKind::Triage) {
        guards.push(Guard {
            command: format!(
                "python3 -c {}",
                shell_quote(&crate::codex::codex_reviewer_publish_guard_script())
            ),
            matcher: "*",
            timeout_sec: Some(30),
            run_once: false,
        });
    }
    guards
}

fn guards_json(guards: &[Guard]) -> Vec<Value> {
    guards
        .iter()
        .map(|guard| {
            json!({
                "command": guard.command,
                "matcher": guard.matcher,
                "timeout_sec": guard.timeout_sec.unwrap_or(30),
                "run_once": guard.run_once,
            })
        })
        .collect()
}

fn required_artifact_paths(
    process_home: &Path,
    obs: &ProgressObservationConfig,
    interception: &ToolUseInterceptionConfig,
) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let obs = &private_forwarder_config(process_home, obs);
    let mut artifacts = collect_guard_artifact_paths(interception);
    artifacts.extend([
        ("adapter".into(), adapter_path(process_home)),
        ("policy".into(), policy_path(process_home)),
        ("hooks".into(), hooks_file_path(process_home)),
        ("settings".into(), session_settings_path_for_run(&obs.run_id)?),
        ("forwarder".into(), obs.forwarder_binary.clone()),
    ]);
    Ok(artifacts)
}

/// Pure expected policy for this run: guard commands, matchers, timeouts, and
/// required artifact paths. Install writes this document; startup verification
/// compares the installed policy to it before probing.
pub(super) fn expected_authorization_policy(
    process_home: &Path,
    worker_kind: WorkerKind,
    obs: &ProgressObservationConfig,
    interception: &ToolUseInterceptionConfig,
) -> anyhow::Result<Value> {
    let guards = authorization_guards(worker_kind, interception);
    if guards.is_empty() {
        bail!("AgyDriver refuses to arm zero PreToolUse guards (ToolUseInterception declared)");
    }
    let obs = &private_forwarder_config(process_home, obs);
    let artifacts = required_artifact_paths(process_home, obs, interception)?;
    let required_artifacts: serde_json::Map<String, Value> = artifacts
        .iter()
        .map(|(name, path)| (name.clone(), json!(path)))
        .collect();
    Ok(json!({
        "version": 1,
        "authorization_command": authorization_command(process_home, obs),
        "hook_timeout_sec": AUTHORIZATION_HOOK_TIMEOUT_SEC,
        "required_artifacts": required_artifacts,
        "run_id": obs.run_id,
        "worker_kind": worker_kind_name(worker_kind),
        "approval": approval_for(worker_kind),
        "workspace_path": obs.workspace_path.display().to_string(),
        "process_home": process_home.display().to_string(),
        "protected_paths": protected_paths(process_home, interception),
        // Where agy keeps per-conversation scratch and truncated-output files.
        // The adapter exempts only `<this>/<the hook payload's conversation id>`
        // from `protected_paths`, never the directory itself or a sibling.
        "conversation_scratch_root": brain_link(process_home).display().to_string(),
        "artifact_write_paths": artifact_write_paths_json(worker_kind, &obs.run_id),
        "forwarder": obs.forwarder_binary.display().to_string(),
        "attestation_path": attestation_path(process_home).display().to_string(),
        "guards": guards_json(&guards),
    }))
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
    super::home::validate_private_layout(process_home)?;
    let policy = expected_authorization_policy(process_home, worker_kind, obs, interception)?;
    let artifacts = required_artifact_paths(process_home, obs, interception)?;

    let config_dir = gemini_config_dir(process_home);
    fs::create_dir_all(&config_dir).with_context(|| format!("creating {}", config_dir.display()))?;

    // App updates replace the shared binary; bind to a run-owned snapshot.
    let private_obs = private_forwarder_config(process_home, obs);
    fs::copy(&obs.forwarder_binary, &private_obs.forwarder_binary).context("copying the per-run progress forwarder")?;
    let obs = &private_obs;

    let adapter = adapter_path(process_home);
    write_executable(&adapter, &crate::guard_python::with_command_tokenizer(ADAPTER_SCRIPT))?;

    let policy_file = policy_path(process_home);
    let attestation_file = attestation_path(process_home);
    let hooks_path = hooks_file_path(process_home);
    write_json_atomic(&policy_file, &policy)?;
    let mut hooks = forwarder_hooks_map(obs);
    hooks.insert("PreToolUse".into(), json!([pre_tool_use_entry(process_home, obs)]));
    write_hooks_document(process_home, &json!({ HOOK_GROUP: hooks }))?;
    validate_configuration(process_home, &policy)?;

    // Every declared artifact must exist; missing guards/settings never shrink
    // the binding to a misleadingly healthy subset.
    let entries = artifacts
        .into_iter()
        .map(|(name, path)| {
            Ok(AttestationEntry {
                name,
                path: path.display().to_string(),
                sha256: hash_file(&path)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let attestation = AuthorizationAttestation {
        generated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        entries,
    };
    write_json_atomic(&attestation_file, &serde_json::to_value(&attestation)?)?;

    Ok(vec![
        hooks_path,
        adapter,
        policy_file,
        attestation_file,
        obs.forwarder_binary.clone(),
    ])
}

/// Re-check that every attested artifact still has the bytes written at install.
pub fn verify_attestation(process_home: &Path) -> anyhow::Result<AuthorizationAttestation> {
    let path = attestation_path(process_home);
    let body = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let attestation: AuthorizationAttestation =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    let policy: Value =
        serde_json::from_slice(&fs::read(policy_path(process_home)).context("reading authorization policy")?)
            .context("authorization policy is malformed")?;
    validate_configuration(process_home, &policy)?;
    let required = policy["required_artifacts"]
        .as_object()
        .context("policy missing required artifact set")?;
    for name in ["adapter", "hooks", "policy", "settings", "forwarder"] {
        if !required.contains_key(name) {
            bail!("binding missing required artifact {name}");
        }
    }
    if required["adapter"] != json!(adapter_path(process_home))
        || required["hooks"] != json!(hooks_file_path(process_home))
        || required["policy"] != json!(policy_path(process_home))
        || required["settings"]
            != json!(session_settings_path_for_run(
                policy["run_id"].as_str().context("policy run_id")?
            )?)
        || required["forwarder"] != policy["forwarder"]
    {
        bail!("authorization binding has unexpected run paths");
    }
    if attestation.entries.len() != required.len() {
        bail!("authorization binding has incomplete artifact set");
    }
    for (name, expected) in required {
        if attestation
            .entries
            .iter()
            .filter(|e| &e.name == name && json!(e.path) == *expected)
            .count()
            != 1
        {
            bail!("authorization binding missing or misplaced {name}");
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["adapter", "forwarder"] {
            let path = required[name].as_str().context("executable path")?;
            let meta = fs::metadata(path).with_context(|| format!("authorization binding: {name} missing: {path}"))?;
            if meta.permissions().mode() & 0o111 == 0 {
                bail!("authorization binding: {name} is not executable");
            }
        }
    }
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

fn validate_configuration(process_home: &Path, policy: &Value) -> anyhow::Result<()> {
    let workspace = PathBuf::from(policy["workspace_path"].as_str().context("workspace path")?);
    let settings_path = session_settings_path_for_run(policy["run_id"].as_str().context("policy run_id")?)?;
    let settings =
        fs::read_to_string(&settings_path).with_context(|| format!("reading {}", settings_path.display()))?;
    validate_session_settings(&settings, &workspace)?;
    let hooks: Value =
        serde_json::from_slice(&fs::read(hooks_file_path(process_home)).context("reading authorization hooks")?)
            .context("authorization hooks are malformed")?;
    let entry = &hooks["boss"]["PreToolUse"];
    let command = policy["authorization_command"]
        .as_str()
        .context("expected authorization command")?;
    let expected_entry = json!([{
        "matcher": "*",
        "hooks": [{"type": "command", "command": command, "timeout": policy["hook_timeout_sec"]}]
    }]);
    if entry != &expected_entry || hooks.as_object().is_none_or(|groups| groups.len() != 1) {
        bail!("PreToolUse configuration does not match the wildcard command contract");
    }
    Ok(())
}

/// Validate the final session contract: the renderer's own parse and
/// validation (the same rule grammar, pins and strictness it applies when it
/// writes the file), plus the authorization-specific requirement that the
/// session trusts every spelling of the working directory and nothing else.
fn validate_session_settings(settings: &str, workspace: &Path) -> anyhow::Result<()> {
    let settings = super::permissions::parse_rendered(settings)?;
    let workspace = fs::canonicalize(workspace)?;
    let required = crate::scoped_home::trust_path_variants(&workspace);
    if settings.trusted_workspaces() != required.as_slice() {
        bail!(
            "session settings must trust every spelling of the working directory \
             and nothing else (need {required:?}, got {:?})",
            settings.trusted_workspaces()
        );
    }
    Ok(())
}

#[cfg(test)]
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
    probe_authorization_with_timeout(process_home, std::time::Duration::from_secs(60))
}

fn probe_authorization_with_timeout(
    process_home: &Path,
    timeout: std::time::Duration,
) -> anyhow::Result<AuthorizationProbeReport> {
    verify_attestation(process_home)?;
    let hooks: Value = serde_json::from_slice(&fs::read(hooks_file_path(process_home))?)?;
    let command = hooks
        .pointer("/boss/PreToolUse/0/hooks/0/command")
        .and_then(Value::as_str)
        .context("missing or disabled PreToolUse authorization command")?;
    if !command.contains(&shell_quote(&adapter_path(process_home).display().to_string())) {
        bail!("PreToolUse authorization command is disabled or malformed");
    }
    let policy: Value = serde_json::from_slice(&fs::read(policy_path(process_home))?)?;
    // A review-guide worker's real guard approves only pinned source reads or a
    // guide submission, so its positive probe is a read inside the guide
    // workspace, with the same workspace variable the pane exports.
    let review_guide = policy["worker_kind"].as_str() == Some("review_guide");
    let guide_workspace = policy["workspace_path"].as_str().unwrap_or_default().to_owned();
    let run = |payload: Value| -> anyhow::Result<Value> {
        // The shared process runner kills the entire probe process group on
        // expiry rather than waiting indefinitely for adapter output.
        let (input, mut writer) = std::os::unix::net::UnixStream::pair()?;
        writer.write_all(payload.to_string().as_bytes())?;
        drop(writer);
        let input: std::os::fd::OwnedFd = input.into();
        let output = boss_command_runner::output_blocking_timeout_in_group(
            Command::new("/bin/sh")
                .args(["-c", command])
                .env("BOSS_AGY_PROBE", "1")
                .env(
                    "BOSS_REVIEW_GUIDE_WORKSPACE",
                    if review_guide { guide_workspace.as_str() } else { "" },
                )
                .stdin(Stdio::from(input))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
            timeout,
            |_| {},
        )?;
        if !output.status.success() {
            bail!(
                "installed authorization command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        serde_json::from_slice(&output.stdout).context("installed authorization command produced malformed output")
    };
    let payload = |cmd: &str| {
        json!({
            "hookEventName": "PreToolUse",
            "conversationId": "probe",
            "toolCall": {"id":"probe", "name":"run_command", "args":{
                "CommandLine":cmd, "Cwd":policy["workspace_path"]
            }}
        })
    };
    let positive = run(payload(if review_guide {
        "cat boss-agy-auth-positive.txt"
    } else {
        "echo boss-agy-auth-positive"
    }))?;
    let denied = run(payload("sudo rm -rf /tmp/boss-agy-auth-denied-sentinel"))?;
    // Only the reused launch guard denies this command; structural policy allows
    // it. This checks deny liveness in addition to the adapter's own deny.
    let guard_denied = run(payload("open -a Boss"))?;
    let expected = policy["approval"].as_str().context("approval")?;
    if positive["decision"] != expected || denied["decision"] != "deny" || guard_denied["decision"] != "deny" {
        bail!(
            "pre-prompt authorization probe failed: positive={positive}, structural={denied}, reused_guard={guard_denied}"
        );
    }
    Ok(AuthorizationProbeReport {
        ok: true,
        positive_decision: expected.into(),
        denied_decision: "deny".into(),
        denied_reason: denied["reason"].as_str().map(str::to_owned),
        reason: None,
    })
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
#[cfg(test)]
pub(super) fn render_hooks_json(config: &ProgressObservationConfig) -> anyhow::Result<String> {
    let body = json!({ HOOK_GROUP: forwarder_hooks_map(config) });
    Ok(serde_json::to_string_pretty(&body)? + "\n")
}

/// Overwrite the per-run home's `hooks.json` (provisioned as a no-op canary)
/// with the forwarder wiring. Returns the path written.
#[cfg(test)]
pub(super) fn write_hooks(process_home: &Path, config: &ProgressObservationConfig) -> anyhow::Result<PathBuf> {
    write_hooks_document(
        process_home,
        &serde_json::from_str::<Value>(&render_hooks_json(config)?)?,
    )
}

fn write_json_atomic(path: &Path, body: &Value) -> anyhow::Result<()> {
    let parent = path.parent().context("JSON has parent")?;
    fs::create_dir_all(parent)?;
    let staging = path.with_extension("json.tmp");
    fs::write(&staging, serde_json::to_string_pretty(body)? + "\n")?;
    fs::rename(&staging, path).with_context(|| format!("installing {}", path.display()))
}

fn write_hooks_document(process_home: &Path, body: &Value) -> anyhow::Result<PathBuf> {
    super::home::validate_private_layout(process_home)?;
    let path = hooks_file_path(process_home);
    write_json_atomic(&path, body)?;
    Ok(path)
}

#[cfg(test)]
#[path = "hooks_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "forwarder_tests.rs"]
mod forwarder_tests;
