//! Shared stubbed [`CommandRunner`] for tmux-teardown tests, so
//! `tmux_teardown.rs`, `worker_process_reaping.rs`, and
//! `worker_pane_lifecycle.rs` don't each hand-roll their own copy —
//! mirroring [`crate::tmux_adoption`]'s `FakeTmuxServer` pattern.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use boss_tmux::{CommandOutput, CommandRunner, Tmux};

/// Scripted `tmux` replies in exact call order. Panics on an unexpected
/// call, which is what makes "no kill-session was issued" assertable —
/// a refused teardown that nonetheless tried to kill fails the test by
/// running out of scripted replies.
#[derive(Default)]
pub(crate) struct StubRunner {
    outcomes: StdMutex<VecDeque<CommandOutput>>,
    calls: StdMutex<Vec<Vec<String>>>,
}

impl StubRunner {
    pub(crate) fn replies(replies: impl IntoIterator<Item = CommandOutput>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: StdMutex::new(replies.into_iter().collect()),
            calls: StdMutex::new(Vec::new()),
        })
    }

    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl CommandRunner for StubRunner {
    async fn run(&self, _program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        assert!(cwd.is_none());
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect());
        Ok(self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("stub runner received an unexpected tmux command"))
    }
}

pub(crate) fn ok(stdout: &str) -> CommandOutput {
    CommandOutput {
        success: true,
        code: Some(0),
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

pub(crate) fn failure(stderr: &str) -> CommandOutput {
    CommandOutput {
        success: false,
        code: Some(1),
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

pub(crate) fn fake_tmux(replies: impl IntoIterator<Item = CommandOutput>) -> (Tmux, Arc<StubRunner>) {
    let runner = StubRunner::replies(replies);
    (
        Tmux::with_runner("/opt/homebrew/bin/tmux", runner.clone()).unwrap(),
        runner,
    )
}
