//! Claude reference-driver byte-for-byte goldens.
//!
//! Every surface is produced through the driver interface (or through the
//! `worker_setup` renderers that take `&dyn AgentDriver`), never by reaching
//! past the trait into Claude-only helpers. A regression here means an
//! extraction changed the worker-visible contract.
//!
//! ## v1 scope
//!
//! Goldens pin **standard** worker kind only (`WorkerKind::Standard`). Other
//! worker kinds (e.g. answer-agent / automation-specific setups) are
//! intentionally unpinned in this harness revision; extend deliberately when
//! those surfaces gain a driver-extraction gate.

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
        automation_outcome_proposals_seam_enabled: false,
        is_review_supervisor: false,
        pr_created_proposals_seam_enabled: false,
    }
}

/// Host-temp paths land in the path-guard / checkleft hook commands. Rewrite
/// them to a stable placeholder so the settings.json golden is portable.
///
/// Must mirror [`crate::worker_setup`]'s `worker_settings_root`: under Bazel
/// tests `render_settings_json` roots gate scripts at `$TEST_TMPDIR` (unique
/// per test action / shard), not `std::env::temp_dir()`. Normalising only the
/// latter leaves the sandbox path in the rendered golden and fails CI.
fn normalize_host_paths(rendered: &str) -> String {
    // Prefer TEST_TMPDIR when set (Bazel), else the process temp dir. Either
    // may or may not carry a trailing slash — strip so the replacement always
    // leaves a single `/` after `$TMPDIR`.
    let tmp = match std::env::var_os("TEST_TMPDIR") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::env::temp_dir(),
    };
    let tmp = tmp.display().to_string();
    let tmp = tmp.trim_end_matches('/').to_owned();
    let with_tmp = rendered
        .replace(&tmp, "$TMPDIR")
        .replace(&tmp.replace(' ', "\\ "), "$TMPDIR");
    // Guard scripts live in a content-addressed subdirectory keyed on the
    // script sha256 (`path-guard-<hex>/boss-path-guard.py`). That hash
    // changes whenever the script bytes change, so it isn't portable
    // byte-for-byte; but the golden must still pin the *shape* — that the
    // guard lives under a per-content hashed directory, not a flat shared
    // path — since that shape is exactly what this content-addressing
    // work introduced. Rewrite the hash to a stable placeholder instead of
    // deleting the directory component.
    replace_content_addressed_guard_dirs(&with_tmp)
}

/// Rewrite `<kind>-<64 hex>/` path components to `<kind>-<sha256>/` so a
/// content-addressed guard path compares equal across builds while still
/// asserting that the hashed directory component is present.
fn replace_content_addressed_guard_dirs(s: &str) -> String {
    replace_kind_hash_dir(&replace_kind_hash_dir(s, "path-guard-"), "checkleft-push-guard-")
}

fn replace_kind_hash_dir(s: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find(prefix) {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + prefix.len()..];
        let hash = after.get(..64);
        let slash = after.as_bytes().get(64);
        if hash.is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit())) && slash == Some(&b'/') {
            out.push_str(prefix);
            out.push_str("<sha256>/");
            rest = &after[65..];
        } else {
            out.push_str(prefix);
            rest = after;
        }
    }
    out.push_str(rest);
    out
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
        run_id: None,
    });
    assert_eq!(
        plan.env,
        vec![EnvDirective::Unset("ANTHROPIC_API_KEY".to_owned())],
        "Claude spawn must unset ANTHROPIC_API_KEY",
    );
    assert_eq!(
        plan.command,
        "claude --model opus --disallowedTools=AskUserQuestion --permission-mode auto --settings '/tmp/boss-worker-settings/mono-agent-007.json' \"$(cat .claude/initial-prompt.txt)\"\n",
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
        run_id: None,
    });
    assert_eq!(
        plan.command,
        "claude --model sonnet --effort low --disallowedTools=AskUserQuestion --dangerously-skip-permissions \"$(cat .claude/initial-prompt.txt)\"\n",
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
        run_id: None,
    });
    assert_eq!(
        plan.command,
        "claude --model sonnet --effort high --disallowedTools=AskUserQuestion --permission-mode auto \"$(cat .claude/initial-prompt.txt)\"\n",
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
        run_id: None,
    });
    assert_eq!(
        plan.command,
        "claude --model sonnet --disallowedTools=AskUserQuestion --permission-mode dontAsk \"$(cat .claude/initial-prompt.txt)\"\n",
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
    // No `Read(...)` entries: any `Read()` deny rule arms Claude Code's
    // "compound command contains `cd` with a relative file read" permission
    // prompt (an existence-only predicate over the deny list), which stalls
    // an unattended worker. The read side of the Boss-data-dir fence is
    // carried by the path-guard PreToolUse hook, which is stricter — it
    // canonicalises relative paths against the call's `cwd`.
    let expected = vec![
        format!("Edit({boss_dir})"),
        format!("Edit({boss_dir}/**)"),
        "Bash(bossctl)".to_owned(),
        "Bash(bossctl:*)".to_owned(),
        "Bash(boss engine start)".to_owned(),
        "Bash(boss engine start:*)".to_owned(),
        "Bash(boss engine stop)".to_owned(),
        "Bash(boss engine stop:*)".to_owned(),
        r#"Bash("$BOSS_BIN" engine start)"#.to_owned(),
        r#"Bash("$BOSS_BIN" engine start:*)"#.to_owned(),
        r#"Bash("$BOSS_BIN" engine stop)"#.to_owned(),
        r#"Bash("$BOSS_BIN" engine stop:*)"#.to_owned(),
        "Bash($BOSS_BIN engine start)".to_owned(),
        "Bash($BOSS_BIN engine start:*)".to_owned(),
        "Bash($BOSS_BIN engine stop)".to_owned(),
        "Bash($BOSS_BIN engine stop:*)".to_owned(),
    ];
    assert_eq!(
        deny, expected,
        "standard-worker deny rules must match the pre-refactor set byte-for-byte",
    );
}

// ── settings.json ───────────────────────────────────────────────────────────

#[test]
fn replace_content_addressed_guard_dirs_pins_the_hashed_shape() {
    let hashed = "$TMPDIR/boss-worker-settings/path-guard-a8a5bfd127fda4353a55ebdcae8b58e154e46d4fbdf0b9df5cd0bc963e33fd16/boss-path-guard.py";
    assert_eq!(
        replace_content_addressed_guard_dirs(hashed),
        "$TMPDIR/boss-worker-settings/path-guard-<sha256>/boss-path-guard.py"
    );
    let checkleft = "$TMPDIR/boss-worker-settings/checkleft-push-guard-df95ca546c6b89098a3afd957409744c52c2334079f3f435cbcbe81bbb4e4e8d/boss-checkleft-push-guard.py";
    assert_eq!(
        replace_content_addressed_guard_dirs(checkleft),
        "$TMPDIR/boss-worker-settings/checkleft-push-guard-<sha256>/boss-checkleft-push-guard.py"
    );
}

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
