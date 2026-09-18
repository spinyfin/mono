//! This assertion needs a whole private HOME, not just a private workspace.
//! Keep it in a separate Bazel test action: the hermetic test wrapper provides
//! a private HOME, and no concurrent pane-spawn test can provision Claude in it.
//! Do not swap the process-wide HOME inside the multithreaded engine unit tests.

use boss_engine::worker_setup::{WorkerKind, WorkerSetupInput, write_workspace_files};
use std::path::PathBuf;
use tempfile::TempDir;

/// Regression guard: `write_workspace_files` must not produce Claude
/// artifacts for a non-Claude driver. A Grok (or any other non-Claude)
/// worker never runs `claude`, so it must get neither a `~/.claude.json`
/// trust stamp (that driver's own `provision_workspace` already handled
/// its own trust, e.g. Grok's `trusted_folders.toml`) nor Claude's specific
/// gitignore constant — both must now be driver-supplied via
/// `AgentDriver::pre_trust_workspace` / `AgentDriver::config_dir_gitignore`,
/// not hardcoded at the `write_workspace_files` call site.
#[test]
fn write_workspace_files_does_not_pre_trust_claude_json_for_non_claude_driver() {
    use boss_engine::driver::test_support::{StubDriver, stub_descriptor};
    use boss_engine::driver::{Capability, CapabilitySet};

    let dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-stub-trust".into(),
        lease_id: "lease-stub-trust".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
        automation_outcome_proposals_seam_enabled: false,
        is_review_supervisor: false,
        is_post_merge_reviewer: false,
    };

    let mut descriptor = stub_descriptor();
    descriptor.name = "stub-grok";
    descriptor.config_dir = ".stub-grok";
    descriptor.agent_rules_filename = "AGENTS.md";
    let driver = StubDriver::new(descriptor, CapabilitySet::new([Capability::Spawn]));

    let written = write_workspace_files(&input, &driver).unwrap();

    // The stub's own config-dir gitignore (trait default) is still written —
    // this is the driver's own file in the driver's own dir, not Claude's.
    assert!(written.gitignore_path.exists());
    assert_eq!(
        std::fs::read_to_string(&written.gitignore_path).unwrap(),
        "*\n",
        "non-Claude driver must still get the trait-default catch-all gitignore",
    );

    // No Claude-specific trust artifact may appear in this test action's
    // private HOME, so `~/.claude.json` existing at all
    // here means the Claude-only pre-trust path ran for a driver that
    // isn't Claude.
    let claude_json = boss_engine::driver::claude::claude_global_config_path().unwrap();
    assert!(
        !claude_json.exists(),
        "non-Claude driver must not write ~/.claude.json; got: {}",
        claude_json.display(),
    );
}
