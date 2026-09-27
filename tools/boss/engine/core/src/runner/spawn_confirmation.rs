//! Bounded spawn-time confirmation that the worker driver actually started.
//!
//! After the pane's initial input is delivered (today: a login-shell `-c`
//! that sources `.boss/initial-input.sh` and execs the CLI with the prompt
//! already in argv), the engine waits for two independent pieces of
//! evidence before treating the spawn as successful:
//!
//! 1. **Composer readiness** — driver-specific PTY evidence that the CLI
//!    is actually up (`PaneMonitorSpec` agent / starting / prompt markers).
//!    For argv delivery this is the analog of "the composer can accept
//!    input": the CLI has exec'd and rendered its surface, which is what
//!    proves the sourced script did not die at `execve()`.
//! 2. **Turn start** — the first driver hook / session event
//!    (`LiveWorkerStateRegistry::has_driver_signal_for_run`), or, on the
//!    remote path, a persisted `work_runs.transcript_path`.
//!
//! Either wait timing out fails the spawn immediately with a named error
//! so the coordinator records `pane_spawn_failed` instead of leaving the
//! execution `Spawning` for `spawn_ack_sweep`'s later generic reap.

use std::time::Duration;

use anyhow::anyhow;
use boss_protocol::PaneMonitorSpec;

/// How long spawn may wait for driver-specific PTY evidence that the CLI
/// is up. Sized well above a healthy driver's first paint (seconds) and
/// well below [`crate::live_worker_state::DRIVER_START_GRACE_SECS`] so a
/// doomed exec is a spawn failure, not a five-minute `Spawning` hang.
pub(crate) const COMPOSER_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long spawn may wait after the driver is up for the first
/// driver-originated hook / session event. A healthy `SessionStart` /
/// `UserPromptSubmit` fires within seconds of exec; folder-trust is
/// pre-seeded at provision time, so this does not need the 300s sweep
/// window.
pub(crate) const TURN_START_TIMEOUT: Duration = Duration::from_secs(45);

/// Poll period for both waits.
pub(crate) const SPAWN_CONFIRM_POLL: Duration = Duration::from_millis(100);

/// True when `pane_text` shows this driver's TUI is up — agent chrome,
/// a startup banner, or the composer prompt. Any one marker is enough:
/// the question is "has this CLI started?", not "is it idle?".
pub(crate) fn pane_shows_driver_ready(pane_text: &str, spec: &PaneMonitorSpec) -> bool {
    spec.agent_markers
        .iter()
        .any(|marker| pane_text.contains(marker.as_str()))
        || spec
            .starting_markers
            .iter()
            .any(|marker| pane_text.contains(marker.as_str()))
        || spec
            .prompt_prefixes
            .iter()
            .any(|marker| pane_text.contains(marker.as_str()))
}

/// Poll `ready` until it returns true or `timeout` elapses.
#[cfg(test)]
pub(crate) async fn wait_until<F, Fut>(timeout: Duration, poll: Duration, mut ready: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = tokio::time::Instant::now();
    loop {
        if ready().await {
            return true;
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return false;
        }
        let remaining = timeout.saturating_sub(elapsed);
        tokio::time::sleep(poll.min(remaining)).await;
    }
}

pub(crate) fn composer_not_ready_error(driver_name: &str, run_id: &str, timeout: Duration) -> anyhow::Error {
    anyhow!(
        "refusing to complete {driver_name} spawn for {run_id}: driver composer never became ready within {}s \
         (no pane marker from the driver's monitor spec after prompt delivery); recording a failed spawn",
        timeout.as_secs()
    )
}

pub(crate) fn turn_did_not_start_error(driver_name: &str, run_id: &str, timeout: Duration) -> anyhow::Error {
    anyhow!(
        "refusing to complete {driver_name} spawn for {run_id}: no driver hook or session event arrived within \
         {}s after prompt delivery; recording a failed spawn",
        timeout.as_secs()
    )
}

/// Run the two spawn-time waits against injected predicates so tests can
/// exercise timeout and success without a live tmux pane.
#[cfg(test)]
pub(crate) async fn confirm_spawn_started<Ready, ReadyFut, Started, StartedFut>(
    driver_name: &str,
    run_id: &str,
    composer_timeout: Duration,
    turn_timeout: Duration,
    poll: Duration,
    composer_ready: Ready,
    turn_started: Started,
) -> anyhow::Result<()>
where
    Ready: FnMut() -> ReadyFut,
    ReadyFut: std::future::Future<Output = bool>,
    Started: FnMut() -> StartedFut,
    StartedFut: std::future::Future<Output = bool>,
{
    if !wait_until(composer_timeout, poll, composer_ready).await {
        return Err(composer_not_ready_error(driver_name, run_id, composer_timeout));
    }
    if !wait_until(turn_timeout, poll, turn_started).await {
        return Err(turn_did_not_start_error(driver_name, run_id, turn_timeout));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{AgentDriver, ClaudeDriver, CodexDriver, GrokDriver};
    use crate::live_worker_state::{DriverSignalKind, LiveWorkerStateRegistry};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn claude_spec() -> PaneMonitorSpec {
        ClaudeDriver.pane_monitor_spec().expect("claude has a pane spec")
    }

    fn codex_spec() -> PaneMonitorSpec {
        CodexDriver::default()
            .pane_monitor_spec()
            .expect("codex has a pane spec")
    }

    fn grok_spec() -> PaneMonitorSpec {
        GrokDriver::default().pane_monitor_spec().expect("grok has a pane spec")
    }

    #[test]
    fn claude_composer_ready_matches_agent_chrome() {
        assert!(pane_shows_driver_ready(
            "Claude Code 2.1.283\nauto mode on",
            &claude_spec()
        ));
        assert!(pane_shows_driver_ready("Accessing workspace: /tmp/ws", &claude_spec()));
        assert!(pane_shows_driver_ready("❯ ", &claude_spec()));
        assert!(!pane_shows_driver_ready("login: ", &claude_spec()));
    }

    #[test]
    fn codex_composer_ready_matches_agent_chrome() {
        assert!(pane_shows_driver_ready(">_ OpenAI Codex (v0.15)", &codex_spec()));
        assert!(pane_shows_driver_ready("Booting MCP server: boss", &codex_spec()));
        assert!(!pane_shows_driver_ready("login: ", &codex_spec()));
    }

    #[test]
    fn grok_composer_ready_matches_agent_chrome() {
        assert!(pane_shows_driver_ready("Shift+Tab:mode  always-approve", &grok_spec()));
        assert!(pane_shows_driver_ready("Starting session…", &grok_spec()));
        assert!(pane_shows_driver_ready("│ ❯ ", &grok_spec()));
        assert!(!pane_shows_driver_ready("❯ Use the shell", &grok_spec()));
    }

    #[tokio::test]
    async fn wait_until_returns_true_when_predicate_flips() {
        let ready = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ready);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });
        let seen = wait_until(Duration::from_millis(200), Duration::from_millis(5), || async {
            ready.load(Ordering::SeqCst)
        })
        .await;
        assert!(seen);
    }

    #[tokio::test]
    async fn wait_until_returns_false_when_predicate_never_flips() {
        let seen = wait_until(Duration::from_millis(30), Duration::from_millis(5), || async { false }).await;
        assert!(!seen);
    }

    #[tokio::test]
    async fn confirm_spawn_started_fails_when_composer_never_ready() {
        let err = confirm_spawn_started(
            "claude",
            "exec-missing-composer",
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_millis(5),
            || async { false },
            || async { true },
        )
        .await
        .expect_err("must fail the spawn");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "{msg}");
        assert!(msg.contains("exec-missing-composer"), "{msg}");
        assert!(msg.contains("composer never became ready"), "{msg}");
        assert!(msg.contains("failed spawn"), "{msg}");
    }

    #[tokio::test]
    async fn confirm_spawn_started_fails_when_no_turn_start_evidence_arrives() {
        let err = confirm_spawn_started(
            "codex",
            "exec-silent-turn",
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_millis(5),
            || async { true },
            || async { false },
        )
        .await
        .expect_err("must fail the spawn");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "{msg}");
        assert!(msg.contains("exec-silent-turn"), "{msg}");
        assert!(msg.contains("no driver hook or session event"), "{msg}");
        assert!(msg.contains("failed spawn"), "{msg}");
    }

    #[tokio::test]
    async fn confirm_spawn_started_succeeds_when_composer_and_turn_start_arrive() {
        confirm_spawn_started(
            "grok",
            "exec-ok",
            Duration::from_millis(50),
            Duration::from_millis(50),
            Duration::from_millis(5),
            || async { true },
            || async { true },
        )
        .await
        .expect("ready composer plus turn-start evidence must pass");
    }

    #[tokio::test]
    async fn turn_start_predicate_can_read_driver_signal_registry() {
        let registry = Arc::new(LiveWorkerStateRegistry::new());
        registry.register_spawn(1, "exec-hook", "opus", 42, None);
        let live = Arc::clone(&registry);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            live.record_driver_signal("exec-hook", DriverSignalKind::HookEvent);
        });
        confirm_spawn_started(
            "claude",
            "exec-hook",
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(5),
            || async { pane_shows_driver_ready("Claude Code", &claude_spec()) },
            || async { registry.has_driver_signal_for_run("exec-hook") },
        )
        .await
        .expect("a recorded hook must count as turn-start evidence");
    }

    #[test]
    fn per_driver_large_prompt_delivery_is_ready_once_the_cli_surface_paints() {
        // argv delivery submits the prompt at exec; the turn starts once the
        // driver's own surface is up. These snapshots are the per-driver
        // evidence that a large-prompt spawn actually submitted.
        assert!(pane_shows_driver_ready(
            "Claude Code\n❯ working through a 100KB prompt",
            &claude_spec()
        ));
        assert!(pane_shows_driver_ready(
            ">_ OpenAI Codex\n• Working (1s • esc to interrupt)",
            &codex_spec()
        ));
        assert!(pane_shows_driver_ready(
            "Grok 4.6  Shift+Tab:mode\nStarting session",
            &grok_spec()
        ));
    }
}
