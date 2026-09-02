//! Grok hook wiring (design T-09): the `boss-event` progress forwarder plus
//! the five `PreToolUse` interception guards, both written into
//! `$GROK_HOME/hooks/` (never merged into the Claude worker settings file —
//! see [`crate::HookWiringDestination::DriverOwned`]).
//!
//! ## Why two different wiring shapes share this file
//!
//! The forwarder fires on every lifecycle event and hands its **raw** Grok
//! payload to the unmodified `boss-event` shim, exactly the shape
//! [`crate::claude::ClaudeDriver`] uses — `boss-event` is
//! schema-agnostic transport (it splices `_boss_run_id` and forwards bytes;
//! see `tools/boss/event-shim`), and the engine's per-driver
//! `normalize_progress_event` (design T-10, follow-on) is what decodes the
//! Grok dialect on the engine side. Canonicalising the payload here too
//! would make that follow-on normaliser a no-op rather than real work, and
//! would leave two different "canonical" shapes (the adapter's guess and
//! T-10's normaliser) that could silently drift apart.
//!
//! The five `PreToolUse` guards are different: they run **synchronously
//! in-hook** and decide whether the tool call proceeds, today, before T-10
//! exists. `driver/src/claude.rs`'s five guard scripts read Claude's
//! snake_case `tool_name`/`tool_input` and emit Claude's `{"decision":
//! "block"|"approve"}` vocabulary. Grok's native `PreToolUse` payload is
//! camelCase with different tool names, and — the load-bearing empirical
//! finding of
//! `tools/boss/docs/investigations/grok-pretooluse-decision-vocabulary-and-tool-name-map.md`
//! — Grok's `PreToolUse` does **not** honour Claude's `block`/`approve`
//! vocabulary at all; only `{"decision":"deny"}` or exit code 2 blocks, and
//! `{"decision":"block"}` is silently **fail-open** (Grok reserves `block`
//! for `Stop`/`SubagentStop` stop-gates instead). Wiring the five scripts to
//! Grok unmodified would therefore not "mostly work" — it would run every
//! guard, always approve, and look identical to a healthy configuration.
//!
//! [`GROK_HOOK_ADAPTER_SCRIPT`] is what makes the guards real: it sits in
//! front of each unchanged guard script, translating in both directions
//! (Grok payload → Boss canonical shape on the way in; Boss `block`/
//! `approve` → Grok `deny`/`allow` on the way out) so the five scripts named
//! in the design (`claude::BOSS_LAUNCH_GUARD_COMMAND`,
//! `claude::PR_REDIRECT_GUARD_COMMAND`, `claude::REVISION_PR_GUARD_COMMAND`,
//! plus the path guard and checkleft-push guard scripts materialised by
//! `core::worker_setup` and handed in via [`crate::PermissionInput`]) never
//! need a Grok-specific edit.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use boss_ssh_transport::shell_quote;
use serde_json::json;

use super::home::hooks_file_path;
use crate::claude::{
    BOSS_LAUNCH_GUARD_COMMAND, PR_REDIRECT_GUARD_COMMAND, REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND,
    REVISION_PR_GUARD_COMMAND,
};
use crate::{ProgressObservationConfig, ToolUseInterceptionConfig};

/// Filename of the canonicalisation adapter under `$GROK_HOME/hooks/`.
const ADAPTER_FILENAME: &str = "boss-grok-hook-adapter.py";

/// Every Grok hook lifecycle event Boss wires to the `boss-event` forwarder.
/// Matches the documented/observed event set (design §"The event / progress
/// stream") and mirrors Claude's own lifecycle event list.
const GROK_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "Notification",
    "SessionEnd",
];

/// Per-handler timeout (seconds) for the passive `boss-event` progress
/// forwarder on every lifecycle event, including `Stop`.
///
/// Grok's hook runner **blocks the TUI** for the full duration of every
/// in-flight command hook (bundled user-guide §Hooks: "long-running hooks
/// block the UI"). When `timeout` is omitted, non-gate events default to 5s
/// but **`Stop` / `SubagentStop` default to 600s** — the budget intended for
/// real stop-*gates* that run builds/tests. Boss's forwarder is passive
/// observability, never a gate: it must not inherit that 600s ceiling, or a
/// wedged events socket / slow `boss-event` write freezes the pane after the
/// final assistant text is already on screen (empty prompt, no keystrokes,
/// residual `[hooks: 1]` chrome) while the engine may already have flipped
/// the slot to Ready from a partially-delivered Stop.
///
/// 15s covers `boss-event`'s full connect-retry schedule (~10.2s) plus
/// headroom for a small write. On timeout Grok fails open (turn ends; UI
/// returns) — correct for a non-gating forwarder. Keep in sync with
/// `tools/boss/event-shim`'s retry budget.
pub const FORWARDER_HOOK_TIMEOUT_SEC: u64 = 15;

/// Build the `boss-event` forwarder command line — byte-for-byte the same
/// env-prefix shape [`crate::claude::ClaudeDriver`]'s
/// `progress_observation_wiring` uses. The raw Grok payload is forwarded
/// unchanged (see module docs for why).
fn forwarder_command(config: &ProgressObservationConfig) -> String {
    format!(
        "BOSS_EVENTS_SOCKET={socket} BOSS_LEASE_ID={lease} BOSS_RUN_ID={run_id} BOSS_WORKSPACE={workspace} {shim}",
        socket = shell_quote(&config.events_socket_path.display().to_string()),
        lease = shell_quote(&config.lease_id),
        run_id = shell_quote(&config.run_id),
        workspace = shell_quote(&config.workspace_path.display().to_string()),
        shim = shell_quote(&config.forwarder_binary.display().to_string()),
    )
}

/// The forwarder-only hooks map: every [`GROK_HOOK_EVENTS`] entry wired to
/// [`forwarder_command`], matcher `*`. Returned both by
/// [`super::GrokDriver::progress_observation_wiring`] (so the capability's
/// declared wiring reflects what is actually installed) and used as the
/// starting point for [`write_hooks`], which appends the guard entries onto
/// the same `PreToolUse` array so the forwarder still observes every tool
/// call before any guard can block it — matching Claude's ordering.
pub fn forwarder_hooks_map(config: &ProgressObservationConfig) -> serde_json::Map<String, serde_json::Value> {
    let forward_hook = json!({
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": forwarder_command(config),
            // Explicit on every event — critical on Stop, which otherwise
            // inherits Grok's 600s stop-gate default and can freeze the pane.
            "timeout": FORWARDER_HOOK_TIMEOUT_SEC,
        }],
    });
    let mut hooks = serde_json::Map::new();
    for event in GROK_HOOK_EVENTS {
        hooks.insert((*event).to_owned(), json!([forward_hook.clone()]));
    }
    hooks
}

/// One unchanged Boss guard command plus the Grok matcher that should fire
/// it (mirrors the matcher `claude::ClaudeDriver::tool_use_interception_wiring`
/// uses for the same guard: Grok's matcher engine resolves the `Bash` alias
/// to `run_terminal_command` — design §Hooks — so reusing `"Bash"` here fires
/// on Grok shell calls without a Grok-specific matcher spelling).
struct Guard {
    command: String,
    matcher: &'static str,
}

/// The five Boss `PreToolUse` guard commands, unchanged from Claude's. Order
/// matches `claude::ClaudeDriver::tool_use_interception_wiring` exactly.
/// Guards are omitted per the same rules Claude/Codex use (path guard +
/// checkleft guard need their materialised script path; PR-redirect +
/// checkleft are Standard-worker-only; revision guard is revision-only) —
/// the Boss-launch guard is the only one that is always present, so this
/// never returns empty as long as `config` reaches this function at all.
fn guard_commands(config: &ToolUseInterceptionConfig) -> Vec<Guard> {
    let mut out = Vec::new();

    // 1. Path guard (data-dir sandbox). Matcher `*`: it inspects
    //    file_path/notebook_path/path on every tool, not just Bash.
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

    // 2. Boss-launch guard. Always on, all workers.
    out.push(Guard {
        command: BOSS_LAUNCH_GUARD_COMMAND.to_owned(),
        matcher: "Bash",
    });

    // 3. PR-redirect guard. Standard workers only (local + remote).
    if config.is_standard_worker {
        out.push(Guard {
            command: PR_REDIRECT_GUARD_COMMAND.to_owned(),
            matcher: "Bash",
        });
    }

    // 4. Checkleft push guard. Local Standard workers only.
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

    // 5. Reviewer static-analysis guard. The shared Claude command is run by
    // Grok's adapter, so the three drivers enforce the same command policy.
    if config.is_reviewer {
        out.push(Guard {
            command: REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND.to_owned(),
            matcher: "Bash",
        });
    }

    // 6. Revision PR guard.
    if config.is_revision {
        out.push(Guard {
            command: REVISION_PR_GUARD_COMMAND.to_owned(),
            matcher: "Bash",
        });
    }

    out
}

/// Write the canonicalisation adapter script and the full hooks wiring
/// (forwarder on every lifecycle event, adapter-wrapped guards on
/// `PreToolUse`) into `$GROK_HOME/hooks/`. Overwrites the provisional canary
/// [`super::home`] wrote at `provision_workspace` time. Returns the absolute
/// paths written (for [`crate::PermissionArtifacts::config_files`]).
///
/// Refuses (rather than silently arming zero guards) if `guard_commands`
/// somehow returns empty — see [`Guard`]'s doc for why that should not
/// happen in practice; treated the same as Codex's `materialize_guards`
/// defensive check.
pub fn write_hooks(
    grok_home: &Path,
    obs_config: &ProgressObservationConfig,
    interception: &ToolUseInterceptionConfig,
) -> anyhow::Result<Vec<PathBuf>> {
    let hooks_dir = grok_home.join("hooks");
    fs::create_dir_all(&hooks_dir).with_context(|| format!("creating {}", hooks_dir.display()))?;

    let adapter_path = hooks_dir.join(ADAPTER_FILENAME);
    write_executable(&adapter_path, GROK_HOOK_ADAPTER_SCRIPT)?;

    let guards = guard_commands(interception);
    if guards.is_empty() {
        bail!("GrokDriver refuses to arm zero PreToolUse guards (ToolUseInterception declared)");
    }

    let mut hooks = forwarder_hooks_map(obs_config);
    let pre_tool_use = hooks
        .entry("PreToolUse".to_owned())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut()
        .expect("PreToolUse hook entry is always inserted as a JSON array");
    for guard in guards {
        let wrapped_command = format!(
            "{adapter} {guard}",
            adapter = shell_quote(&adapter_path.display().to_string()),
            guard = shell_quote(&guard.command),
        );
        pre_tool_use.push(json!({
            "matcher": guard.matcher,
            "hooks": [{"type": "command", "command": wrapped_command}],
        }));
    }

    let hooks_value = json!({"hooks": hooks});
    let hooks_path = hooks_file_path(grok_home);
    fs::write(&hooks_path, serde_json::to_string_pretty(&hooks_value)?)
        .with_context(|| format!("writing {}", hooks_path.display()))?;

    Ok(vec![hooks_path, adapter_path])
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

/// The driver-owned canonicalisation adapter (design T-09 / A-2).
///
/// Invoked as `<adapter> <guard-command>` (one argv, the guard's own full
/// shell command line, e.g. `BOSS_DATA_DIR=... python3 /path/to/guard.py` or
/// `python3 -c "..."`) with Grok's raw `PreToolUse` payload on stdin. See the
/// module docs for why this exists and why it is scoped to the guard path
/// only (not the progress forwarder).
///
/// Fails **closed** (emits `{"decision":"deny", ...}` and logs to stderr)
/// whenever it cannot positively canonicalise the invocation or the guard's
/// output: wrong/absent Grok hook identity, a stdin payload that is not a
/// JSON object, a guard exit that produces no parseable decision. Passing an
/// unrecognised payload through unmodified — or defaulting to allow — is
/// exactly the silent guardrail-loss failure mode this adapter exists to
/// prevent (see the fail-open matrix in the decision-vocabulary
/// investigation: Grok's own `PreToolUse` is fail-open on crash/timeout/
/// malformed-output, same as Claude's production posture, and this adapter
/// must not add a *second* fail-open path on top of that).
const GROK_HOOK_ADAPTER_SCRIPT: &str = r#"#!/usr/bin/env python3
"""Grok PreToolUse guard-script canonicalisation adapter (design T-09).

Boss's five PreToolUse guard scripts are written for Claude Code's hook
dialect: snake_case keys (tool_name, tool_input, ...), Claude tool names
(Bash, Write, Edit, ...), and a {"decision": "block"|"approve"} vocabulary.

Grok's native PreToolUse payload is camelCase (toolName, toolInput, ...)
with different tool names (run_terminal_command, write, search_replace),
and -- the load-bearing empirical finding this adapter exists to act on --
Grok's PreToolUse does NOT honour Claude's block/approve vocabulary at all:
only {"decision":"deny"} or exit code 2 blocks, and {"decision":"block"} is
silently fail-open (Grok reserves "block" for Stop/SubagentStop stop-gates
instead). See tools/boss/docs/investigations/
grok-pretooluse-decision-vocabulary-and-tool-name-map.md.

This script sits in front of an UNCHANGED guard command (argv[1], a full
shell command line) so the five guard scripts never need a Grok-specific
edit: it rewrites Grok's payload into Boss's canonical shape, feeds it to
the guard on stdin, and translates the guard's block/approve decision into
Grok's deny/allow vocabulary.

Fails CLOSED (deny) -- never approve -- whenever it cannot positively
canonicalise the invocation: wrong/absent Grok hook identity, a payload
that is not a JSON object, or a guard result it cannot parse as a known
decision. A silently-approved unrecognised invocation is exactly the
guardrail-loss failure mode this adapter exists to prevent.
"""
import json
import os
import subprocess
import sys

# Grok's snake_case hookEventName values -> Boss's canonical PascalCase
# hook_event_name. Superset of the events Boss currently wires; an
# unrecognised value passes through unchanged (this only affects the
# hook_event_name field a guard could branch on -- none of Boss's five
# guards do -- never the deny/approve decision path itself).
EVENT_NAME_MAP = {
    "session_start": "SessionStart",
    "user_prompt_submit": "UserPromptSubmit",
    "pre_tool_use": "PreToolUse",
    "post_tool_use": "PostToolUse",
    "stop": "Stop",
    "notification": "Notification",
    "session_end": "SessionEnd",
    "post_tool_use_failure": "PostToolUseFailure",
    "permission_denied": "PermissionDenied",
    "stop_failure": "StopFailure",
    "subagent_start": "SubagentStart",
    "subagent_stop": "SubagentStop",
    "pre_compact": "PreCompact",
    "post_compact": "PostCompact",
}

# Empirically confirmed on the wire (grok-pretooluse-decision-vocabulary-
# and-tool-name-map.md, Part 2). Only these three transfer; every other
# toolName is left as-is. That is the safe default: a guard that keys on
# tool_name == "Bash" simply does not match an unmapped name (early-approves
# that tool the same way it would for any other non-Bash tool), and the
# path guard's file_path/notebook_path/path checks do not depend on the
# tool name at all.
TOOL_NAME_MAP = {
    "run_terminal_command": "Bash",
    "write": "Write",
    "search_replace": "Edit",
}

RENAME_MAP = {
    "sessionId": "session_id",
    "toolInput": "tool_input",
    "toolResult": "tool_response",
    "stopHookActive": "stop_hook_active",
    "transcriptPath": "transcript_path",
    "permissionMode": "permission_mode",
}


def fail_closed(reason):
    sys.stderr.write("boss-grok-hook-adapter: DENY (fail-closed): %s\n" % (reason,))
    sys.stdout.write(json.dumps({"decision": "deny", "reason": reason}))
    sys.exit(0)


def canonicalise(payload):
    out = dict(payload)

    if "hookEventName" in out:
        raw_event = out.pop("hookEventName")
        out["hook_event_name"] = EVENT_NAME_MAP.get(raw_event, raw_event)

    if "toolName" in out:
        raw_tool = out.pop("toolName")
        out["tool_name"] = TOOL_NAME_MAP.get(raw_tool, raw_tool)

    for camel, snake in RENAME_MAP.items():
        if camel in out:
            out[snake] = out.pop(camel)

    return out


def translate_decision(raw_stdout, guard_exit_code):
    """Boss guard vocabulary (block/approve) -> Grok PreToolUse vocabulary
    (deny/allow). Anything else -- unparseable output, a decision string
    that is not block/approve/deny/allow -- fails closed."""
    try:
        decision_obj = json.loads(raw_stdout)
    except Exception:
        fail_closed("guard produced non-JSON stdout (exit=%s): %r" % (guard_exit_code, raw_stdout[:500]))

    decision = decision_obj.get("decision") if isinstance(decision_obj, dict) else None
    reason = decision_obj.get("reason") if isinstance(decision_obj, dict) else None

    if decision == "block":
        out = {"decision": "deny"}
        if reason is not None:
            out["reason"] = reason
        sys.stdout.write(json.dumps(out))
        sys.exit(0)

    if decision == "approve":
        sys.stdout.write(json.dumps({"decision": "allow"}))
        sys.exit(0)

    if decision in ("deny", "allow"):
        # Already Grok-native vocabulary (not emitted by Boss's five guards
        # today, but the adapter contract says pass it through unchanged).
        sys.stdout.write(json.dumps(decision_obj))
        sys.exit(0)

    fail_closed("guard emitted unrecognised decision %r (exit=%s)" % (decision, guard_exit_code))


def main():
    if len(sys.argv) != 2:
        fail_closed("adapter invoked without exactly one guard-command argument")

    guard_command = sys.argv[1]

    # Identity: this adapter must run only under a genuine Grok hook
    # invocation. Grok's hook runner injects these four values. Never key on
    # CLAUDE_PROJECT_DIR: Grok also exports that for its own native hooks.
    hook_identity = (
        os.environ.get("GROK_HOOK_EVENT"),
        os.environ.get("GROK_HOOK_NAME"),
        os.environ.get("GROK_SESSION_ID"),
        os.environ.get("GROK_WORKSPACE_ROOT"),
    )
    if not all(hook_identity):
        fail_closed(
            "GROK_HOOK_EVENT / GROK_HOOK_NAME / GROK_SESSION_ID / "
            "GROK_WORKSPACE_ROOT not present in the hook environment; "
            "refusing to run an interception guard outside a recognised Grok hook invocation"
        )

    raw_stdin = sys.stdin.read()
    try:
        payload = json.loads(raw_stdin)
    except Exception:
        fail_closed("stdin payload was not valid JSON: %r" % (raw_stdin[:500],))

    if not isinstance(payload, dict):
        fail_closed("stdin payload was not a JSON object")

    canonical = canonicalise(payload)

    # The guard command is an unmodified Boss guard invocation string (e.g.
    # "BOSS_DATA_DIR=... python3 /path/to/guard.py" or a `python3 -c "..."`
    # inline command) -- it needs a shell to apply its own env-var prefix or
    # quoting, exactly as it would if Claude's hook runner invoked it
    # directly. No timeout is imposed here beyond whatever the outer Grok
    # hook runner enforces: the checkleft push guard already carries its own
    # multi-minute internal timeout budget and fails open on its own terms;
    # an adapter-imposed shorter timeout would cut that budget short and
    # fail-closed a guard that was about to fail open correctly on its own.
    try:
        result = subprocess.run(
            ["/bin/sh", "-c", guard_command],
            input=json.dumps(canonical),
            capture_output=True,
            text=True,
        )
    except Exception as exc:
        fail_closed("failed to exec guard command: %r" % (exc,))
        return

    if result.stderr:
        sys.stderr.write(result.stderr)

    translate_decision(result.stdout, result.returncode)


if __name__ == "__main__":
    main()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn interception_config(overrides: impl FnOnce(&mut ToolUseInterceptionConfig)) -> ToolUseInterceptionConfig {
        let mut config = ToolUseInterceptionConfig {
            data_dir: None,
            path_guard_script: None,
            checkleft_guard_script: None,
            is_revision: false,
            is_standard_worker: true,
            is_reviewer: false,
            run_id: Some("run-1".into()),
            workspace_path: Some(PathBuf::from("/tmp/ws")),
        };
        overrides(&mut config);
        config
    }

    #[test]
    fn guard_commands_always_includes_boss_launch_guard() {
        let config = interception_config(|c| c.is_standard_worker = false);
        let guards = guard_commands(&config);
        assert_eq!(guards.len(), 1, "only the boss-launch guard should fire: {config:?}");
        assert_eq!(guards[0].command, BOSS_LAUNCH_GUARD_COMMAND);
        assert_eq!(guards[0].matcher, "Bash");
    }

    #[test]
    fn guard_commands_full_local_standard_worker_set() {
        let config = interception_config(|c| {
            c.data_dir = Some(PathBuf::from("/tmp/boss-data"));
            c.path_guard_script = Some(PathBuf::from("/tmp/settings/boss-path-guard.py"));
            c.checkleft_guard_script = Some(PathBuf::from("/tmp/settings/boss-checkleft-push-guard.py"));
        });
        let guards = guard_commands(&config);
        assert_eq!(guards.len(), 4, "path + launch + pr-redirect + checkleft: {config:?}");
        assert!(guards[0].command.contains("BOSS_DATA_DIR="));
        assert!(guards[0].command.contains("boss-path-guard.py"));
        assert_eq!(guards[0].matcher, "*");
        assert_eq!(guards[1].command, BOSS_LAUNCH_GUARD_COMMAND);
        assert_eq!(guards[2].command, PR_REDIRECT_GUARD_COMMAND);
        assert!(guards[3].command.contains("boss-checkleft-push-guard.py"));
    }

    #[test]
    fn guard_commands_revision_appends_revision_guard() {
        let config = interception_config(|c| c.is_revision = true);
        let guards = guard_commands(&config);
        assert_eq!(guards.len(), 3, "launch + pr-redirect + revision: {config:?}");
        assert_eq!(guards.last().unwrap().command, REVISION_PR_GUARD_COMMAND);
    }

    #[test]
    fn reviewer_gets_the_shared_static_analysis_guard() {
        let config = interception_config(|c| {
            c.is_standard_worker = false;
            c.is_reviewer = true;
        });
        let guards = guard_commands(&config);
        assert_eq!(guards.len(), 2, "launch + reviewer guard: {config:?}");
        assert_eq!(guards[1].command, REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND);
        assert_eq!(guards[1].matcher, "Bash");
    }

    #[test]
    fn forwarder_hooks_map_covers_every_lifecycle_event() {
        let config = ProgressObservationConfig {
            events_socket_path: PathBuf::from("/tmp/events.sock"),
            lease_id: "lease-1".into(),
            run_id: "run-1".into(),
            workspace_path: PathBuf::from("/tmp/ws"),
            forwarder_binary: PathBuf::from("/tmp/boss-event"),
        };
        let hooks = forwarder_hooks_map(&config);
        for event in GROK_HOOK_EVENTS {
            assert!(hooks.contains_key(*event), "missing forwarder wiring for {event}");
            let entry = &hooks[*event][0];
            let command = entry["hooks"][0]["command"].as_str().unwrap();
            assert!(command.contains("BOSS_RUN_ID='run-1'"), "{command}");
            assert!(command.contains("/tmp/boss-event"), "{command}");
            // Passive forwarder must never inherit Stop's 600s gate default.
            let timeout = entry["hooks"][0]["timeout"].as_u64();
            assert_eq!(
                timeout,
                Some(FORWARDER_HOOK_TIMEOUT_SEC),
                "{event} forwarder missing short timeout (got {timeout:?})"
            );
        }
    }

    #[test]
    fn write_hooks_writes_adapter_and_wraps_every_guard() {
        let tmp = tempfile::TempDir::new().unwrap();
        let grok_home = tmp.path().join("grok-home");
        fs::create_dir_all(&grok_home).unwrap();

        let obs_config = ProgressObservationConfig {
            events_socket_path: PathBuf::from("/tmp/events.sock"),
            lease_id: "lease-1".into(),
            run_id: "run-1".into(),
            workspace_path: PathBuf::from("/tmp/ws"),
            forwarder_binary: PathBuf::from("/tmp/boss-event"),
        };
        let interception = interception_config(|c| {
            c.data_dir = Some(PathBuf::from("/tmp/boss-data"));
            c.path_guard_script = Some(PathBuf::from("/tmp/settings/boss-path-guard.py"));
            // Isolate to exactly path guard + boss-launch guard for this test.
            c.is_standard_worker = false;
        });

        let written = write_hooks(&grok_home, &obs_config, &interception).unwrap();
        assert_eq!(written.len(), 2);

        let adapter_path = grok_home.join("hooks").join(ADAPTER_FILENAME);
        assert!(adapter_path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&adapter_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "adapter must be executable");
        }

        let hooks_path = hooks_file_path(&grok_home);
        let hooks_doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
        let pre_tool_use = hooks_doc["hooks"]["PreToolUse"].as_array().unwrap();
        // forwarder + path guard + boss-launch guard.
        assert_eq!(pre_tool_use.len(), 3, "{pre_tool_use:#?}");
        let forwarder_command = pre_tool_use[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(forwarder_command.contains("/tmp/boss-event"));
        assert_eq!(
            pre_tool_use[0]["hooks"][0]["timeout"].as_u64(),
            Some(FORWARDER_HOOK_TIMEOUT_SEC),
            "on-disk PreToolUse forwarder must carry the short timeout"
        );
        // Stop is the freeze-critical event: its omitted-timeout default is 600s.
        assert_eq!(
            hooks_doc["hooks"]["Stop"][0]["hooks"][0]["timeout"].as_u64(),
            Some(FORWARDER_HOOK_TIMEOUT_SEC),
            "on-disk Stop forwarder must not inherit Grok's 600s stop-gate default"
        );
        let guard_1 = pre_tool_use[1]["hooks"][0]["command"].as_str().unwrap();
        assert!(guard_1.starts_with(&shell_quote(&adapter_path.display().to_string())));
        assert!(guard_1.contains("BOSS_DATA_DIR="));
        let guard_2 = pre_tool_use[2]["hooks"][0]["command"].as_str().unwrap();
        assert!(guard_2.contains("python3 -c"));
    }

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run the real adapter script end to end against a stub guard, the way
    /// Grok itself would invoke it: `<adapter> <guard-command>` with a raw
    /// (camelCase) Grok payload on stdin.
    fn run_adapter(
        adapter_path: &Path,
        guard_command: &str,
        grok_payload: &serde_json::Value,
        real_hook_env: bool,
    ) -> (i32, String, String) {
        let mut cmd = std::process::Command::new("python3");
        cmd.arg(adapter_path).arg(guard_command);
        for key in [
            "GROK_HOOK_EVENT",
            "GROK_HOOK_NAME",
            "GROK_SESSION_ID",
            "GROK_WORKSPACE_ROOT",
            "CLAUDE_PROJECT_DIR",
            "GROK_AGENT",
        ] {
            cmd.env_remove(key);
        }
        if real_hook_env {
            // This fixture was recorded from the installed Grok 0.2.114 hook
            // runner. Loading it keeps this acceptance path tied to the
            // runner's contract rather than manufacturing one in the test.
            let hook_env: std::collections::BTreeMap<String, String> =
                serde_json::from_str(include_str!("testdata/grok-hook-env-0.2.114.json"))
                    .expect("real Grok hook environment fixture must be valid JSON");
            cmd.envs(hook_env);
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("spawn python3 adapter");
        {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(grok_payload.to_string().as_bytes())
                .unwrap();
        }
        let output = child.wait_with_output().expect("wait adapter");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn write_adapter_script(dir: &Path) -> PathBuf {
        let path = dir.join(ADAPTER_FILENAME);
        write_executable(&path, GROK_HOOK_ADAPTER_SCRIPT).unwrap();
        path
    }

    /// Acceptance (design T-09): a fixture worker attempting `gh pr create`
    /// through a raw Grok `run_terminal_command` payload is demonstrably
    /// refused via the unmodified PR-redirect guard, not silently approved.
    #[test]
    fn pr_redirect_guard_denies_gh_pr_create_via_grok_payload() {
        if !python3_available() {
            eprintln!("python3 not available; skipping adapter acceptance test");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter_path = write_adapter_script(tmp.path());

        let payload = json!({
            "hookEventName": "pre_tool_use",
            "sessionId": "s-1",
            "cwd": "/tmp/ws",
            "toolName": "run_terminal_command",
            "toolInput": {"command": "gh pr create --title x --body y"},
        });

        let (code, stdout, _stderr) = run_adapter(&adapter_path, PR_REDIRECT_GUARD_COMMAND, &payload, true);
        assert_eq!(code, 0);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(decision["decision"], "deny", "gh pr create must be refused: {stdout}");
        assert!(decision["reason"].as_str().unwrap().contains("cube pr"));
    }

    /// Same guard, `jj git push` — the other bare-VCS-push phrasing the
    /// design calls out by name.
    #[test]
    fn pr_redirect_guard_denies_jj_git_push_via_grok_payload() {
        if !python3_available() {
            eprintln!("python3 not available; skipping adapter acceptance test");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter_path = write_adapter_script(tmp.path());

        let payload = json!({
            "hookEventName": "pre_tool_use",
            "toolName": "run_terminal_command",
            "toolInput": {"command": "jj git push --branch my-feature"},
        });

        let (code, stdout, _stderr) = run_adapter(&adapter_path, PR_REDIRECT_GUARD_COMMAND, &payload, true);
        assert_eq!(code, 0);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(decision["decision"], "deny", "jj git push must be refused: {stdout}");
    }

    /// A benign shell command through the same guard must be allowed —
    /// otherwise the negative tests above would be meaningless (a guard
    /// that denies everything "passes" them too).
    #[test]
    fn pr_redirect_guard_allows_benign_command_via_grok_payload() {
        if !python3_available() {
            eprintln!("python3 not available; skipping adapter acceptance test");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter_path = write_adapter_script(tmp.path());

        let payload = json!({
            "hookEventName": "pre_tool_use",
            "toolName": "run_terminal_command",
            "toolInput": {"command": "echo hello"},
        });

        let (code, stdout, _stderr) = run_adapter(&adapter_path, PR_REDIRECT_GUARD_COMMAND, &payload, true);
        assert_eq!(code, 0);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(
            decision["decision"], "allow",
            "benign command must be allowed: {stdout}"
        );
    }

    /// Revision-PR guard: a revision worker attempting `gh pr create` (via
    /// the `write` tool spelling too, to prove the tool-name map is applied
    /// even off the Bash matcher) is refused.
    #[test]
    fn revision_pr_guard_denies_gh_pr_create_via_grok_payload() {
        if !python3_available() {
            eprintln!("python3 not available; skipping adapter acceptance test");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter_path = write_adapter_script(tmp.path());

        let payload = json!({
            "hookEventName": "pre_tool_use",
            "toolName": "run_terminal_command",
            "toolInput": {"command": "gh pr create --title x --body y"},
        });

        let (code, stdout, _stderr) = run_adapter(&adapter_path, REVISION_PR_GUARD_COMMAND, &payload, true);
        assert_eq!(code, 0);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(
            decision["decision"], "deny",
            "revision worker must not open a new PR: {stdout}"
        );
    }

    /// Boss-launch guard: attempting to run the installed Boss.app is
    /// refused via a Grok `run_terminal_command` payload.
    #[test]
    fn boss_launch_guard_denies_open_boss_app_via_grok_payload() {
        if !python3_available() {
            eprintln!("python3 not available; skipping adapter acceptance test");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter_path = write_adapter_script(tmp.path());

        let payload = json!({
            "hookEventName": "pre_tool_use",
            "toolName": "run_terminal_command",
            "toolInput": {"command": "open -a Boss"},
        });

        let (code, stdout, _stderr) = run_adapter(&adapter_path, BOSS_LAUNCH_GUARD_COMMAND, &payload, true);
        assert_eq!(code, 0);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(decision["decision"], "deny", "must refuse to launch Boss.app: {stdout}");
    }

    /// Path guard: a `write` tool call targeting a path under the Boss data
    /// dir is refused. Exercises the `write` -> `Write` tool-name rename and
    /// the guard's `file_path` check via a native (non-Bash) Grok tool.
    #[test]
    fn path_guard_denies_write_under_boss_data_dir_via_grok_payload() {
        if !python3_available() {
            eprintln!("python3 not available; skipping adapter acceptance test");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter_path = write_adapter_script(tmp.path());
        let data_dir = tmp.path().join("boss-data");
        fs::create_dir_all(&data_dir).unwrap();
        let guard_script = write_path_guard_script(tmp.path());

        let guard_command = format!(
            "BOSS_DATA_DIR={} python3 {}",
            shell_quote(&data_dir.display().to_string()),
            shell_quote(&guard_script.display().to_string()),
        );

        let payload = json!({
            "hookEventName": "pre_tool_use",
            "toolName": "write",
            "cwd": tmp.path().display().to_string(),
            "toolInput": {"file_path": data_dir.join("state.db").display().to_string(), "content": "x"},
        });

        let (code, stdout, _stderr) = run_adapter(&adapter_path, &guard_command, &payload, true);
        assert_eq!(code, 0);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(
            decision["decision"], "deny",
            "write under the Boss data dir must be refused: {stdout}"
        );
    }

    /// Fail-closed: missing runner-injected Grok hook identity must deny, never
    /// silently approve or pass the payload through unmodified.
    #[test]
    fn adapter_fails_closed_without_grok_hook_identity() {
        if !python3_available() {
            eprintln!("python3 not available; skipping adapter acceptance test");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter_path = write_adapter_script(tmp.path());

        let payload = json!({
            "hookEventName": "pre_tool_use",
            "toolName": "run_terminal_command",
            "toolInput": {"command": "echo hello"},
        });

        let (code, stdout, _stderr) = run_adapter(&adapter_path, PR_REDIRECT_GUARD_COMMAND, &payload, false);
        assert_eq!(code, 0);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(
            decision["decision"], "deny",
            "missing Grok hook identity must fail closed: {stdout}"
        );
    }

    /// Fail-closed: a malformed (non-JSON) stdin payload must deny.
    #[test]
    fn adapter_fails_closed_on_malformed_payload() {
        if !python3_available() {
            eprintln!("python3 not available; skipping adapter acceptance test");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter_path = write_adapter_script(tmp.path());

        let mut cmd = std::process::Command::new("python3");
        cmd.arg(&adapter_path)
            .arg(PR_REDIRECT_GUARD_COMMAND)
            .env("GROK_HOOK_EVENT", "pre_tool_use")
            .env("GROK_HOOK_NAME", "global/boss-provision:pre_tool_use[0].hooks[0]")
            .env("GROK_SESSION_ID", "test-session")
            .env("GROK_WORKSPACE_ROOT", "/tmp/ws")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        {
            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(b"not json at all").unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(
            decision["decision"], "deny",
            "malformed payload must fail closed: {stdout}"
        );
    }

    /// Fail-closed: a guard that emits an unrecognised decision string
    /// (not block/approve/deny/allow) must deny, not silently approve.
    #[test]
    fn adapter_fails_closed_on_unrecognised_guard_decision() {
        if !python3_available() {
            eprintln!("python3 not available; skipping adapter acceptance test");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter_path = write_adapter_script(tmp.path());

        let weird_guard = r#"python3 -c "import json; print(json.dumps({'decision':'permit'}))""#;
        let payload = json!({
            "hookEventName": "pre_tool_use",
            "toolName": "run_terminal_command",
            "toolInput": {"command": "echo hello"},
        });

        let (code, stdout, _stderr) = run_adapter(&adapter_path, weird_guard, &payload, true);
        assert_eq!(code, 0);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(
            decision["decision"], "deny",
            "unrecognised guard decision must fail closed: {stdout}"
        );
    }

    /// A guard's own `deny`/`allow` output (Grok-native vocabulary, not
    /// emitted by Boss's five guards today) passes through unchanged.
    #[test]
    fn adapter_passes_through_native_deny_and_allow() {
        if !python3_available() {
            eprintln!("python3 not available; skipping adapter acceptance test");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter_path = write_adapter_script(tmp.path());
        let payload = json!({
            "hookEventName": "pre_tool_use",
            "toolName": "run_terminal_command",
            "toolInput": {"command": "echo hello"},
        });

        let deny_guard = r#"python3 -c "import json; print(json.dumps({'decision':'deny','reason':'native deny'}))""#;
        let (code, stdout, _stderr) = run_adapter(&adapter_path, deny_guard, &payload, true);
        assert_eq!(code, 0);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(decision["decision"], "deny");
        assert_eq!(decision["reason"], "native deny");

        let allow_guard = r#"python3 -c "import json; print(json.dumps({'decision':'allow'}))""#;
        let (code, stdout, _stderr) = run_adapter(&adapter_path, allow_guard, &payload, true);
        assert_eq!(code, 0);
        let decision: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(decision["decision"], "allow");
    }

    /// Minimal standalone copy of the path guard's canonical-prefix logic is
    /// unnecessary — reuse the exact script content the design cites
    /// (`core/src/worker_setup.rs` `PATH_GUARD_SCRIPT`) so this test proves
    /// the *real* guard blocks, not a stand-in. `core` is a higher-level
    /// crate this one must not depend on, so the script body is duplicated
    /// verbatim here for the test fixture only (never used at runtime: the
    /// engine always supplies the real materialised path via
    /// `PermissionInput::path_guard_script`).
    fn write_path_guard_script(dir: &Path) -> PathBuf {
        let path = dir.join("boss-path-guard.py");
        write_executable(&path, PATH_GUARD_SCRIPT_FIXTURE).unwrap();
        path
    }

    const PATH_GUARD_SCRIPT_FIXTURE: &str = r#"#!/usr/bin/env python3
import json
import os
import shlex
import sys


def canonical(path, cwd):
    expanded = os.path.expanduser(os.path.expandvars(path))
    if not os.path.isabs(expanded):
        expanded = os.path.join(cwd, expanded)
    return os.path.realpath(expanded)


def is_inside(child, parent):
    parent = parent.rstrip(os.sep)
    if not parent:
        return False
    return child == parent or child.startswith(parent + os.sep)


def emit(decision, reason=None):
    out = {"decision": decision}
    if reason is not None:
        out["reason"] = reason
    sys.stdout.write(json.dumps(out))
    sys.exit(0)


def main():
    raw_dir = os.environ.get("BOSS_DATA_DIR", "").strip()
    if not raw_dir:
        emit("approve")
    data_dir = os.path.realpath(os.path.expanduser(raw_dir))

    try:
        payload = json.load(sys.stdin)
    except Exception:
        emit("approve")
    if not isinstance(payload, dict):
        emit("approve")

    tool = payload.get("tool_name") or ""
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        tool_input = {}
    cwd = payload.get("cwd") or os.getcwd()

    candidates = []
    for key in ("file_path", "notebook_path", "path"):
        value = tool_input.get(key)
        if isinstance(value, str) and value:
            candidates.append(value)

    if tool == "Bash":
        command = tool_input.get("command")
        if isinstance(command, str):
            try:
                tokens = shlex.split(command)
            except Exception:
                tokens = command.split()
            candidates.extend(tokens)

    for candidate in candidates:
        try:
            if is_inside(canonical(candidate, cwd), data_dir):
                emit("block", "blocked: inside boss data dir")
        except Exception:
            continue

    emit("approve")


if __name__ == "__main__":
    main()
"#;
}
