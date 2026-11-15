//! `agy` hook wiring: the `boss-event` progress forwarder, written to the
//! per-run home's global-root `hooks.json`.
//!
//! `agy` loads `$HOME/.gemini/config/hooks.json` with no flag, whether or not
//! any workspace is trusted, and `HOME` is the Boss-owned per-run home, so the
//! file is Boss-owned and never enters the repo or the agent's workspace
//! (pane viability study, Q3/Q4; design doc, "Global-root hook wiring"). That
//! is why this driver declares
//! [`crate::HookWiringDestination::DriverOwned`]: the engine must not merge
//! the map into the Claude-shaped worker settings file `agy` never reads.
//!
//! ## What the forwarder is
//!
//! Every wired event runs the unchanged `boss-event` shim, which splices
//! `_boss_run_id` into the payload and forwards the bytes to the engine
//! events socket, exactly as Claude and Grok do. The `agy` payload is *not*
//! canonicalised here: decoding the dialect is the engine-side
//! `normalize_progress_event`'s job, so there is one decoder rather than a
//! second "canonical" shape here that could drift from it.
//!
//! The event name is appended to each command as an argument. The shim does
//! not read argv, so the argument is not spliced into the payload; the
//! qualified `agy` (1.5.7 or later) stamps `hookEventName` on every payload.
//! The argument keeps the event distinguishable in process listings and
//! diagnostics, and is where a forwarder that does need it (1.1.12 payloads
//! carry no `hookEventName`) would read it (hook surface study, "Driver
//! follow-up").
//!
//! ## Reply and failure contract
//!
//! `agy` hooks run synchronously, so `SessionStart`, `PreInvocation` and
//! `PreToolUse` add the forwarder's latency to every step.
//!
//! * **Silent stdout.** The shim writes nothing to stdout on success, so the
//!   forwarder never emits a permission verdict of its own. That matters:
//!   in review mode an `allow` reply raises the approval dialog and `{}`
//!   denies the tool (permission study, section 7; shared-HOME study,
//!   section 2). Silence is not approval either: it leaves normal permission
//!   evaluation in place.
//! * **Delivery failure is observable and never an approval.** A non-zero
//!   exit from a hook denies the tool call it reports. The shim exits zero
//!   only when the event was delivered or queued to its on-disk buffer; it
//!   exits non-zero when it cannot deliver *or* buffer, or when the engine
//!   rejects the payload. On `PreToolUse` that denies the call. The
//!   command string must therefore pass the shim's exit status through
//!   untouched (no `|| true`), which `forwarder_exit_status_is_not_masked`
//!   pins.
//! * **Exactly one `PreToolUse` entry.** How several `PreToolUse` hooks
//!   combine was not measured, so this file registers one. The hook adapter
//!   that will answer with the authorization verdict (`auto_approve`,
//!   `deny` or `deny_unless_prior_grant`) takes over that single entry: it
//!   runs the same forwarder command as part of its work and emits the
//!   verdict itself. Until it lands, spawn refuses
//!   (`AgyDriver::launch_ready` is `false`), so nothing runs against this
//!   `PreToolUse` entry.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use boss_ssh_transport::shell_quote;
use serde_json::{Value, json};

use crate::ProgressObservationConfig;
use crate::hook_settings::forwarder_command;

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

/// Path of the global-root hooks file under a per-run process home.
pub(super) fn hooks_file_path(process_home: &Path) -> PathBuf {
    process_home.join(".gemini/config/hooks.json")
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
