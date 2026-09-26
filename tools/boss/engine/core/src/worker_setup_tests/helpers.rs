//! Shared fixtures and helpers for the `worker_setup` test modules.
//!
//! Every sibling test module reaches these via `use super::helpers::*;` and
//! reaches the `worker_setup` items under test via `use super::super::*;`.

use super::super::*;
use std::sync::Mutex;

pub(crate) use tempfile::TempDir;

use crate::driver::AgentDriver;
pub(crate) use crate::driver::ClaudeDriver;

/// Convenience wrapper: render CLAUDE.md using the ClaudeDriver's preamble and
/// config_dir. Tests that care about exact content should pass driver info
/// explicitly, but most tests just need the rendered string for a Claude worker.
pub(crate) fn claude_md_for(input: &WorkerSetupInput) -> String {
    render_claude_md(
        input,
        ClaudeDriver.agent_rules_preamble(),
        ClaudeDriver.descriptor().config_dir,
    )
}

/// Serializes tests that touch the worker-settings dir within one
/// process. `write_workspace_files` materialises per-workspace settings
/// JSON under [`worker_settings_dir`]; a concurrent writer of the same
/// workspace's file otherwise races. Gate scripts are content-addressed
/// and write-once, so they no longer share a mutable path, but the
/// settings JSON still does.
///
/// Cross-process isolation (Bazel shards / `runs_per_test` copies) is
/// handled by [`worker_settings_dir`] preferring `$TEST_TMPDIR` when
/// set — unique per test action. This mutex only covers in-process
/// parallelism (Rust's default multi-threaded test runner), where every
/// thread shares one `TEST_TMPDIR`. Recovers from poisoning so one
/// failing test doesn't cascade.
static SHARED_SETTINGS_DIR_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock_shared_settings_dir() -> std::sync::MutexGuard<'static, ()> {
    SHARED_SETTINGS_DIR_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Private trust store for synchronous workspace setup on the current thread.
/// Other tests, including pane provisioning, cannot resolve this destination.
pub(crate) struct ClaudeConfigGuard {
    _config: crate::driver::test_support::ClaudeConfigOverride,
    _home: TempDir,
}

impl ClaudeConfigGuard {
    pub(crate) fn new() -> Self {
        let home = TempDir::new().unwrap();
        let config = crate::driver::test_support::claude_config_override(&home.path().join(".claude.json"));
        Self {
            _config: config,
            _home: home,
        }
    }
}

pub(crate) fn sample_input() -> WorkerSetupInput {
    WorkerSetupInput {
        run_id: "run-sample".into(),
        lease_id: "lease-uuid-abc".into(),
        workspace_path: PathBuf::from("/Users/brianduff/Documents/dev/workspaces/mono-agent-007"),
        events_socket_path: PathBuf::from("/Users/brianduff/Library/Application Support/Boss/events.sock"),
        boss_event_path: PathBuf::from("/Users/brianduff/Library/Application Support/Boss/bin/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
        automation_outcome_proposals_seam_enabled: false,
        is_review_supervisor: false,
        is_post_merge_reviewer: false,
        pr_created_proposals_seam_enabled: false,
    }
}
