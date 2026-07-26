//! Claude reference-driver byte-for-byte goldens.
//!
//! Every surface is produced through the driver interface (or through the
//! `worker_setup` renderers that take `&dyn AgentDriver`), never by reaching
//! past the trait into Claude-only helpers. A regression here means an
//! extraction changed the worker-visible contract.

use std::path::PathBuf;

use crate::driver::{AgentDriver, ClaudeDriver, EnvDirective, SpawnRequest};
use crate::worker_setup::{WorkerKind, WorkerSetupInput, render_claude_md, render_settings_json};

/// Deterministic fixture input — absolute paths, no host temp dir, so the
/// CLAUDE.md / deny-rule goldens are portable across machines and CI.
fn golden_input() -> WorkerSetupInput {
    WorkerSetupInput {
        run_id: "run-conformance-golden".into(),
        lease_id: "lease-conformance-golden".into(),
        workspace_path: PathBuf::from("/Users/brianduff/Documents/dev/workspaces/mono-agent-007"),
        events_socket_path: PathBuf::from("/Users/brianduff/Library/Application Support/Boss/events.sock"),
        boss_event_path: PathBuf::from("/Users/brianduff/Library/Application Support/Boss/bin/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    }
}

/// Host-temp paths land in the path-guard / checkleft hook commands. Rewrite
/// them to a stable placeholder so the settings.json golden is portable.
fn normalize_host_paths(rendered: &str) -> String {
    // temp_dir() may or may not carry a trailing slash (macOS often does).
    // Strip it so the replacement always leaves a single `/` after `$TMPDIR`.
    let tmp = std::env::temp_dir().display().to_string();
    let tmp = tmp.trim_end_matches('/').to_owned();
    rendered
        .replace(&tmp, "$TMPDIR")
        .replace(&tmp.replace(' ', "\\ "), "$TMPDIR")
}

// ── Spawn line ──────────────────────────────────────────────────────────────

#[test]
fn golden_spawn_line_opus_with_settings() {
    let settings = PathBuf::from("/tmp/boss-worker-settings/mono-agent-007.json");
    let plan = ClaudeDriver.spawn_invocation(SpawnRequest {
        model: "opus",
        effort: None,
        settings_path: Some(settings.as_path()),
        non_opus_auto_mode: false,
        permission_mode_override: None,
    });
    assert_eq!(
        plan.env,
        vec![EnvDirective::Unset("ANTHROPIC_API_KEY".to_owned())],
        "Claude spawn must unset ANTHROPIC_API_KEY",
    );
    assert_eq!(
        plan.command,
        "claude --model opus --permission-mode auto --settings '/tmp/boss-worker-settings/mono-agent-007.json' \"$(cat .claude/initial-prompt.txt)\"\n",
        "Opus + settings spawn line must match the pre-refactor contract byte-for-byte",
    );
}

#[test]
fn golden_spawn_line_sonnet_skip_permissions() {
    let plan = ClaudeDriver.spawn_invocation(SpawnRequest {
        model: "sonnet",
        effort: Some("low"),
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: None,
    });
    assert_eq!(
        plan.command,
        "claude --model sonnet --effort low --dangerously-skip-permissions \"$(cat .claude/initial-prompt.txt)\"\n",
    );
}

#[test]
fn golden_spawn_line_sonnet_corp_auto_mode() {
    let plan = ClaudeDriver.spawn_invocation(SpawnRequest {
        model: "sonnet",
        effort: Some("high"),
        settings_path: None,
        non_opus_auto_mode: true,
        permission_mode_override: None,
    });
    assert_eq!(
        plan.command,
        "claude --model sonnet --effort high --permission-mode auto \"$(cat .claude/initial-prompt.txt)\"\n",
    );
}

#[test]
fn golden_spawn_line_dont_ask_override() {
    let plan = ClaudeDriver.spawn_invocation(SpawnRequest {
        model: "sonnet",
        effort: None,
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: Some("dontAsk"),
    });
    assert_eq!(
        plan.command,
        "claude --model sonnet --permission-mode dontAsk \"$(cat .claude/initial-prompt.txt)\"\n",
    );
}

// ── Deny rules ──────────────────────────────────────────────────────────────

#[test]
fn golden_deny_rules_standard_worker() {
    let input = golden_input();
    let rendered = render_settings_json(&input, &ClaudeDriver);
    let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    let deny: Vec<String> = parsed["permissions"]["deny"]
        .as_array()
        .expect("permissions.deny")
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();

    let boss_dir = "/Users/brianduff/Library/Application Support/Boss";
    let expected = vec![
        format!("Read({boss_dir})"),
        format!("Read({boss_dir}/**)"),
        format!("Edit({boss_dir})"),
        format!("Edit({boss_dir}/**)"),
        "Bash(bossctl)".to_owned(),
        "Bash(bossctl:*)".to_owned(),
        "Bash(boss engine start)".to_owned(),
        "Bash(boss engine start:*)".to_owned(),
        "Bash(boss engine stop)".to_owned(),
        "Bash(boss engine stop:*)".to_owned(),
    ];
    assert_eq!(
        deny, expected,
        "standard-worker deny rules must match the pre-refactor set byte-for-byte",
    );
}

// ── settings.json ───────────────────────────────────────────────────────────

#[test]
fn golden_settings_json_standard_worker() {
    let input = golden_input();
    let rendered = normalize_host_paths(&render_settings_json(&input, &ClaudeDriver as &dyn AgentDriver));
    let expected = normalize_host_paths(GOLDEN_SETTINGS_STANDARD);
    assert_eq!(
        rendered, expected,
        "settings.json must match the golden byte-for-byte (host temp paths normalised to $TMPDIR)",
    );
}

// ── CLAUDE.md ───────────────────────────────────────────────────────────────

#[test]
fn golden_claude_md_standard_worker() {
    let input = golden_input();
    let driver = ClaudeDriver;
    let rendered = render_claude_md(&input, driver.agent_rules_preamble(), driver.descriptor().config_dir);
    assert_eq!(
        rendered, GOLDEN_CLAUDE_MD_STANDARD,
        "CLAUDE.md must match the golden byte-for-byte through the driver preamble + renderer",
    );
}

#[test]
fn golden_claude_md_starts_with_driver_preamble() {
    // Belt: even if the body drifts in a tolerated way, the driver-owned
    // preamble (the part that names "claude hooks") must still lead the file
    // after the title — otherwise PromptComposition extraction regressed.
    let input = golden_input();
    let driver = ClaudeDriver;
    let rendered = render_claude_md(&input, driver.agent_rules_preamble(), driver.descriptor().config_dir);
    let preamble = driver.agent_rules_preamble();
    assert!(
        rendered.contains(preamble),
        "rendered CLAUDE.md must embed the driver's agent_rules_preamble verbatim",
    );
    assert!(
        preamble.contains("claude hooks"),
        "Claude preamble must name claude hooks (PromptComposition surface)",
    );
}

// ── Embedded goldens ────────────────────────────────────────────────────────
//
// Generated from the production renderers against `golden_input()` at the
// time the harness landed. Host-temp paths are stored with a `$TMPDIR`
// placeholder; the test normalises the live render the same way before
// comparing. Update deliberately when the worker-visible contract changes.

// `.golden` suffix so formatters (oxfmt/prettier) do not rewrite the
// byte-exact snapshots the harness pins.
const GOLDEN_SETTINGS_STANDARD: &str = include_str!("goldens/settings_standard.json.golden");
const GOLDEN_CLAUDE_MD_STANDARD: &str = include_str!("goldens/claude_md_standard.md.golden");
