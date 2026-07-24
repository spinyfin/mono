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

/// Serializes tests that touch the *shared* worker-settings dir
/// (`worker_settings_dir()`, a fixed `$TMPDIR` path). `write_workspace_files`
/// truncate-writes the global `boss-path-guard.py` there; a concurrent
/// reader of that same file otherwise observes a half-written (empty)
/// script. The path isn't per-test overridable, so a lock is the
/// minimal isolation. Recovers from poisoning so one failing test
/// doesn't cascade.
static SHARED_SETTINGS_DIR_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock_shared_settings_dir() -> std::sync::MutexGuard<'static, ()> {
    SHARED_SETTINGS_DIR_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// RAII guard that points `$HOME` at a throwaway temp dir for the
/// duration of a test. `write_workspace_files` now calls
/// `pre_trust_workspace`, which writes `~/.claude.json`; without this
/// redirection a test run would pollute the developer's real
/// `~/.claude.json` with stale temp-dir project entries. `$HOME` is
/// process-global, so hold this only while `lock_shared_settings_dir()`
/// is held (every `write_workspace_files` test does). Restores the
/// prior `$HOME` on drop.
pub(crate) struct HomeGuard {
    _home: TempDir,
    original: Option<std::ffi::OsString>,
}

impl HomeGuard {
    pub(crate) fn new() -> Self {
        let home = TempDir::new().unwrap();
        let original = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
        }
        Self { _home: home, original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
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
    }
}
