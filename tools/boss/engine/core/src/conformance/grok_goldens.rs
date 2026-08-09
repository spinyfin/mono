//! Grok reference-driver byte-for-byte spawn-command goldens.
//!
//! Unlike `ClaudeDriver::spawn_invocation` (pure given a [`SpawnRequest`]),
//! `GrokDriver::spawn_invocation` reads run-scoped state off disk (session
//! id, workspace-path stamp) with a random-UUID fallback when that state is
//! absent (`tools/boss/engine/driver/src/grok.rs`'s own comment: "Fallbacks
//! are for fixtures that skip provision only") — calling it directly would
//! make a byte-exact golden non-deterministic. `build_grok_pane_command` is
//! the pure builder the production `spawn_invocation` delegates to for the
//! actual command line, taking workspace and session id as plain
//! parameters; pinning goldens against it is pinning the exact same code
//! path production runs, with no second implementation to drift from.
//!
//! ## v1 scope
//!
//! Only the pane spawn command is pinned here. Grok's `settings.json` /
//! `CLAUDE.md`-equivalent surfaces don't apply the way Claude's goldens
//! (`claude_goldens.rs`) do: `progress_observation_wiring` declares
//! `HookWiringDestination::DriverOwned` (Grok hooks are written under
//! `$GROK_HOME/hooks/` by `hooks::write_hooks`, never merged into the
//! generic `render_settings_json` a `WorkerSettingsFile`-destination driver
//! uses) — extend deliberately if that surface gains its own driver-facing
//! contract worth pinning.

use std::path::PathBuf;

use crate::driver::SpawnRequest;
use crate::driver::grok::build_grok_pane_command;

/// Deterministic fixture inputs — no host temp dir, no real UUID
/// generation, so the golden is portable across machines and CI.
const GOLDEN_SESSION_ID: &str = "019f974c-3d59-7533-b320-3963123c809b";

fn golden_workspace() -> PathBuf {
    PathBuf::from("/Users/brianduff/Documents/dev/workspaces/mono-agent-007")
}

#[test]
fn golden_grok_pane_command_default_model_no_effort() {
    let request = SpawnRequest {
        model: "grok-4.5",
        effort: None,
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
        run_id: None,
    };
    let command = build_grok_pane_command(&request, &golden_workspace(), GOLDEN_SESSION_ID);
    assert_eq!(
        command,
        "grok --model 'grok-4.5' --no-alt-screen --always-approve --trust \
         --session-id '019f974c-3d59-7533-b320-3963123c809b' \
         --cwd '/Users/brianduff/Documents/dev/workspaces/mono-agent-007' \
         --no-subagents --no-memory \"$(cat .grok/initial-prompt.txt)\"\n",
        "Grok pane spawn command (no effort) must match the pinned contract byte-for-byte",
    );
}

#[test]
fn golden_grok_pane_command_with_reasoning_effort() {
    let request = SpawnRequest {
        model: "grok-4.5",
        effort: Some("high"),
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
        run_id: None,
    };
    let command = build_grok_pane_command(&request, &golden_workspace(), GOLDEN_SESSION_ID);
    assert_eq!(
        command,
        "grok --model 'grok-4.5' --reasoning-effort 'high' --no-alt-screen --always-approve \
         --trust --session-id '019f974c-3d59-7533-b320-3963123c809b' \
         --cwd '/Users/brianduff/Documents/dev/workspaces/mono-agent-007' \
         --no-subagents --no-memory \"$(cat .grok/initial-prompt.txt)\"\n",
        "Grok pane spawn command (with --reasoning-effort) must match the pinned contract \
         byte-for-byte",
    );
}

#[test]
fn golden_grok_pane_command_never_emits_worktree_flags() {
    // Design "Out of scope": Grok's built-in git-worktree support (-w,
    // `grok worktree`) must stay unused — cube owns workspace provisioning.
    // Belt on top of the production debug_assert in
    // `GrokDriver::spawn_invocation`.
    let request = SpawnRequest {
        model: "grok-4.5",
        // Use a live-accepted effort value so this golden stays aligned with
        // what GrokDriver actually emits for Large/Max (both map to `high`).
        effort: Some("high"),
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
        run_id: None,
    };
    let command = build_grok_pane_command(&request, &golden_workspace(), GOLDEN_SESSION_ID);
    assert!(
        !command.contains("--worktree") && !command.split_whitespace().any(|tok| tok == "-w"),
        "Grok pane spawn command must never emit worktree flags; got {command}",
    );
}
