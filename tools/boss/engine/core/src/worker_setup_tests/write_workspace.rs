use super::super::*;
use super::helpers::*;

#[test]
fn write_workspace_files_creates_claude_dir_and_writes_all_files() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-1".into(),
        lease_id: "test-lease".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };

    let written = write_workspace_files(&input, &ClaudeDriver).unwrap();

    assert!(written.claude_md_path.exists());
    assert!(written.settings_path.exists());
    assert!(written.gitignore_path.exists());
    assert_eq!(written.claude_md_path, dir.path().join(".claude").join("CLAUDE.md"));
    assert_eq!(written.gitignore_path, dir.path().join(".claude").join(".gitignore"));

    let claude_md_contents = std::fs::read_to_string(&written.claude_md_path).unwrap();
    assert!(claude_md_contents.contains("test-lease"));

    // The settings file must be valid JSON on disk.
    let settings_contents = std::fs::read_to_string(&written.settings_path).unwrap();
    let _: serde_json::Value = serde_json::from_str(&settings_contents).unwrap();

    // Regression guard for the clobbered-`.claude/settings.json`
    // bug: the engine must NEVER drop a settings file into the
    // workspace tree (where `jj`/`git` could ship it). Neither the
    // shared `settings.json` nor the local-override
    // `settings.local.json` may exist under `.claude/`, and the
    // settings file it does write must live outside the workspace.
    let claude_dir = dir.path().join(".claude");
    assert!(
        !claude_dir.join("settings.json").exists(),
        "engine must not write .claude/settings.json into the workspace",
    );
    assert!(
        !claude_dir.join("settings.local.json").exists(),
        "engine must not write .claude/settings.local.json into the workspace",
    );
    assert!(
        !written.settings_path.starts_with(dir.path()),
        "worker settings file must live outside the workspace tree, got: {}",
        written.settings_path.display(),
    );

    // The .gitignore must use the catch-all `*` pattern so every
    // engine-injected file in `.claude/` (including dotfiles and
    // `.gitignore` itself) is hidden from `jj status` / `git status`.
    let gitignore_contents = std::fs::read_to_string(&written.gitignore_path).unwrap();
    assert_eq!(gitignore_contents, "*\n");
}

#[test]
fn write_workspace_files_pre_trusts_workspace_in_claude_json() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-trust".into(),
        lease_id: "lease-trust".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };

    write_workspace_files(&input, &ClaudeDriver).unwrap();

    // The redirected HOME now has a ~/.claude.json marking this
    // workspace as trusted, so the worker's claude session skips the
    // folder-trust dialog. Resolved via the driver's own accessor (HomeGuard
    // above redirects HOME), so this stays correct if the driver ever moves
    // where it seeds trust.
    let config_path = crate::driver::claude::claude_global_config_path().unwrap();
    let config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    let key = dir.path().display().to_string();
    assert_eq!(
        config["projects"][&key]["hasTrustDialogAccepted"],
        serde_json::Value::Bool(true),
    );
}

#[test]
fn write_workspace_files_overwrites_existing_files() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join("CLAUDE.md"), "stale content").unwrap();

    let input = WorkerSetupInput {
        run_id: "run-overwrite".into(),
        lease_id: "new-lease".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };

    write_workspace_files(&input, &ClaudeDriver).unwrap();
    let contents = std::fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
    assert!(contents.contains("new-lease"));
    assert!(!contents.contains("stale content"));
}

#[test]
fn claude_dir_for_appends_dot_claude() {
    let dir = claude_dir_for(Path::new("/some/workspace"));
    assert_eq!(dir, PathBuf::from("/some/workspace/.claude"));
}

/// Call-site cutover acceptance: `write_workspace_files` must invoke the
/// *passed* driver's trait methods (config_dir, agent_rules_preamble,
/// progress_observation_wiring), not hardcode Claude's. A stub registered
/// with a non-`.claude` config dir proves the registry-resolved driver is
/// load-bearing on this call path.
#[test]
fn write_workspace_files_uses_resolved_non_claude_driver() {
    use crate::driver::test_support::{StubDriver, stub_descriptor};
    use crate::driver::{Capability, CapabilitySet};

    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-stub".into(),
        lease_id: "lease-stub".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };

    let mut descriptor = stub_descriptor();
    descriptor.name = "stub-codex";
    descriptor.config_dir = ".stub";
    descriptor.agent_rules_filename = "AGENTS.md";
    let driver = StubDriver::new(descriptor, CapabilitySet::new([Capability::Spawn]));

    let written = write_workspace_files(&input, &driver).unwrap();

    // Config dir and rules file come from the stub's descriptor, not Claude's.
    assert_eq!(
        written.claude_md_path,
        dir.path().join(".stub").join("AGENTS.md"),
        "agent-rules path must use the resolved driver's config_dir + filename",
    );
    assert!(written.claude_md_path.exists());
    assert!(!dir.path().join(".claude").join("CLAUDE.md").exists());

    let rules = std::fs::read_to_string(&written.claude_md_path).unwrap();
    assert!(
        rules.contains("stub-driver preamble"),
        "agent-rules must include the stub's preamble, not Claude's; got: {rules:?}",
    );

    // StdoutJsonl ingress → no Claude boss-event lifecycle hooks. An empty
    // PreToolUse array may still be present (interception layer inserts the
    // key before extending); the seven Claude lifecycle events must not be.
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&written.settings_path).unwrap()).unwrap();
    let hooks = settings
        .get("hooks")
        .and_then(|h| h.as_object())
        .cloned()
        .unwrap_or_default();
    for claude_lifecycle in [
        "SessionStart",
        "UserPromptSubmit",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        assert!(
            !hooks.contains_key(claude_lifecycle),
            "stub StdoutJsonl driver must not emit Claude lifecycle hook {claude_lifecycle}, got: {hooks:?}",
        );
    }
}
