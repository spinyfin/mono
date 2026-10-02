//! Bounded spawn-time confirmation that the worker driver actually started.
//!
//! After the pane's initial input is delivered (today: a login-shell `-c`
//! that sources `.boss/initial-input.sh` and execs the CLI with the prompt
//! already in argv), the engine waits for two independent pieces of
//! evidence before treating the spawn as successful:
//!
//! 1. **Composer readiness** — driver-specific PTY evidence that the CLI
//!    is actually up (`PaneMonitorSpec` agent markers).
//!    For argv delivery this is the analog of "the composer can accept
//!    input": the CLI has exec'd and rendered its surface, which is what
//!    proves the sourced script did not die at `execve()`. A driver hook
//!    or persisted `transcript_path` is strictly stronger than a scraped
//!    pane marker, so turn-start evidence also satisfies this wait.
//! 2. **Turn start** — fresh evidence for the **current** run: a driver
//!    hook / session event (`LiveWorkerStateRegistry::has_driver_signal_for_run`)
//!    or a `transcript_path` on the current `work_runs` row
//!    (`WorkDb::transcript_path_for_execution`). Historical runs of the
//!    same execution do not count.
//!
//! Either wait timing out fails the spawn immediately with a named error
//! so the coordinator records `pane_spawn_failed` instead of leaving the
//! execution `Spawning` for `spawn_ack_sweep`'s later generic reap.
//! "Composer never became ready" is reported only when neither composer
//! chrome nor turn-start evidence arrives.

use std::path::PathBuf;
use std::time::Duration;

use boss_protocol::PaneMonitorSpec;

/// Typed spawn-confirmation failure so the coordinator can classify the
/// attention body without matching error-message substrings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SpawnConfirmationError {
    #[error(
        "refusing to complete {driver_name} spawn for {run_id}: driver composer never became ready within {timeout_secs}s \
         (no pane marker from the driver's monitor spec after prompt delivery); recording a failed spawn"
    )]
    ComposerNotReady {
        driver_name: String,
        run_id: String,
        timeout_secs: u64,
    },
    #[error(
        "refusing to complete {driver_name} spawn for {run_id}: no driver hook or session event arrived within \
         {timeout_secs}s after prompt delivery; recording a failed spawn"
    )]
    TurnDidNotStart {
        driver_name: String,
        run_id: String,
        timeout_secs: u64,
    },
}

/// Walk an anyhow chain for a [`SpawnConfirmationError`].
pub(crate) fn spawn_confirmation_error(err: &anyhow::Error) -> Option<&SpawnConfirmationError> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<SpawnConfirmationError>())
}

use crate::driver::{AgentDriver, ProgressIngress, ProgressObservationConfig};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;

/// How long spawn may wait for driver-specific PTY evidence that the CLI
/// is up. Sized well above a healthy driver's first paint (seconds) and
/// well below [`crate::live_worker_state::DRIVER_START_GRACE_SECS`] so a
/// doomed exec is a spawn failure, not a five-minute `Spawning` hang.
pub(crate) const COMPOSER_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long spawn may wait after the driver is up for the first
/// driver-originated hook / session event, for **hook-callback** drivers
/// (Claude, Grok). A healthy `SessionStart` / `UserPromptSubmit` fires
/// within seconds of exec; folder-trust is pre-seeded at provision time.
///
/// Codex (and any other [`ProgressIngress::AgentJsonlFile`] driver) does
/// not emit SessionStart: its first driver signal is rollout discovery,
/// which the engine already budgets at
/// [`crate::agent_jsonl_discovery::DISCOVERY_TIMEOUT`]. Use
/// [`turn_start_timeout_for_driver`] rather than this constant at the
/// spawn call site.
pub(crate) const TURN_START_TIMEOUT: Duration = Duration::from_secs(45);

/// Poll period for both waits.
pub(crate) const SPAWN_CONFIRM_POLL: Duration = Duration::from_millis(100);

/// True when `pane_text` shows this driver's TUI is up. Only
/// [`PaneMonitorSpec::agent_markers`] count: a bare composer glyph (Claude's
/// `❯`, also starship's default) or a startup banner (`Booting MCP server:`)
/// is not proof the sourced script exec'd the CLI.
pub(crate) fn pane_shows_driver_ready(pane_text: &str, spec: &PaneMonitorSpec) -> bool {
    spec.agent_markers
        .iter()
        .any(|marker| pane_text.contains(marker.as_str()))
}

/// Turn-start wait for `driver`, sized from how that driver first proves
/// it started. Hook-callback drivers use [`TURN_START_TIMEOUT`];
/// [`ProgressIngress::AgentJsonlFile`] drivers use rollout
/// [`crate::agent_jsonl_discovery::DISCOVERY_TIMEOUT`].
pub(crate) fn turn_start_timeout_for_driver(driver: &dyn AgentDriver) -> Duration {
    let dummy = ProgressObservationConfig {
        events_socket_path: PathBuf::from("/dev/null"),
        lease_id: String::new(),
        run_id: String::new(),
        workspace_path: PathBuf::from("/"),
        forwarder_binary: PathBuf::from("/dev/null"),
    };
    match driver.progress_observation_wiring(&dummy) {
        ProgressIngress::AgentJsonlFile(_) => crate::agent_jsonl_discovery::DISCOVERY_TIMEOUT,
        ProgressIngress::HookCallback(_) | ProgressIngress::StdoutJsonl => TURN_START_TIMEOUT,
    }
}

/// Fresh turn-start evidence for the **current** run of `execution_id`.
/// A historical `work_runs` row with a transcript does not count; a driver
/// hook that never persisted `transcript_path` does.
pub(crate) fn current_run_has_turn_start_evidence(
    db: &WorkDb,
    live_states: Option<&LiveWorkerStateRegistry>,
    execution_id: &str,
) -> bool {
    if live_states.is_some_and(|registry| registry.has_driver_signal_for_run(execution_id)) {
        return true;
    }
    db.transcript_path_for_execution(execution_id)
        .ok()
        .flatten()
        .is_some_and(|path| !path.is_empty())
}

/// Poll `ready` until it returns true or `timeout` elapses.
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
    anyhow::Error::from(SpawnConfirmationError::ComposerNotReady {
        driver_name: driver_name.to_owned(),
        run_id: run_id.to_owned(),
        timeout_secs: timeout.as_secs(),
    })
}

pub(crate) fn turn_did_not_start_error(driver_name: &str, run_id: &str, timeout: Duration) -> anyhow::Error {
    anyhow::Error::from(SpawnConfirmationError::TurnDidNotStart {
        driver_name: driver_name.to_owned(),
        run_id: run_id.to_owned(),
        timeout_secs: timeout.as_secs(),
    })
}

/// Run the two spawn-time waits against injected predicates so production
/// (local pane spawn) and tests share one loop.
///
/// Phase 1 treats turn-start evidence as readiness: a driver hook is
/// stronger proof the CLI exec'd than a scraped pane marker, so a spawn
/// whose turn has already started is not reaped for missing chrome.
pub(crate) async fn confirm_spawn_started<Ready, ReadyFut, Started, StartedFut>(
    driver_name: &str,
    run_id: &str,
    composer_timeout: Duration,
    turn_timeout: Duration,
    poll: Duration,
    mut composer_ready: Ready,
    mut turn_started: Started,
) -> anyhow::Result<()>
where
    Ready: FnMut() -> ReadyFut,
    ReadyFut: std::future::Future<Output = bool>,
    Started: FnMut() -> StartedFut,
    StartedFut: std::future::Future<Output = bool>,
{
    let composer_deadline = tokio::time::Instant::now() + composer_timeout;
    loop {
        if turn_started().await {
            return Ok(());
        }
        if composer_ready().await {
            break;
        }
        let now = tokio::time::Instant::now();
        if now >= composer_deadline {
            return Err(composer_not_ready_error(driver_name, run_id, composer_timeout));
        }
        let remaining = composer_deadline.saturating_duration_since(now);
        tokio::time::sleep(poll.min(remaining)).await;
    }
    if !wait_until(turn_timeout, poll, turn_started).await {
        return Err(turn_did_not_start_error(driver_name, run_id, turn_timeout));
    }
    Ok(())
}

/// Remote turn-start wait: same [`wait_until`] loop as the local path, then
/// `reap` (pid kill) when no evidence arrives.
pub(crate) async fn confirm_turn_start_or_reap<Started, StartedFut, Reap, ReapFut>(
    driver_name: &str,
    run_id: &str,
    timeout: Duration,
    poll: Duration,
    turn_started: Started,
    reap: Reap,
) -> anyhow::Result<()>
where
    Started: FnMut() -> StartedFut,
    StartedFut: std::future::Future<Output = bool>,
    Reap: FnOnce() -> ReapFut,
    ReapFut: std::future::Future<Output = ()>,
{
    if wait_until(timeout, poll, turn_started).await {
        return Ok(());
    }
    reap().await;
    Err(turn_did_not_start_error(driver_name, run_id, timeout))
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
        assert!(pane_shows_driver_ready("Claude Code\n❯ ", &claude_spec()));
        assert!(
            !pane_shows_driver_ready("Accessing workspace: /tmp/ws", &claude_spec()),
            "a starting banner without agent chrome is not composer-ready"
        );
        assert!(
            !pane_shows_driver_ready("❯ ", &claude_spec()),
            "Claude's prompt glyph is also starship's default; a login shell must not pass"
        );
        assert!(!pane_shows_driver_ready("login: ", &claude_spec()));
    }

    #[test]
    fn codex_composer_ready_matches_agent_chrome() {
        assert!(pane_shows_driver_ready(">_ OpenAI Codex (v0.15)", &codex_spec()));
        assert!(
            !pane_shows_driver_ready("Booting MCP server: boss", &codex_spec()),
            "an MCP boot banner is not agent chrome"
        );
        assert!(!pane_shows_driver_ready("login: ", &codex_spec()));
    }

    #[test]
    fn grok_composer_ready_matches_agent_chrome() {
        assert!(pane_shows_driver_ready("Shift+Tab:mode  always-approve", &grok_spec()));
        assert!(pane_shows_driver_ready("Grok 4.6  Shift+Tab:mode\n│ ❯ ", &grok_spec()));
        assert!(
            !pane_shows_driver_ready("Starting session…", &grok_spec()),
            "a starting banner without agent chrome is not composer-ready"
        );
        assert!(!pane_shows_driver_ready("│ ❯ ", &grok_spec()));
        assert!(!pane_shows_driver_ready("❯ Use the shell", &grok_spec()));
    }

    #[test]
    fn turn_start_timeout_follows_the_driver_ingress() {
        assert_eq!(
            turn_start_timeout_for_driver(&ClaudeDriver),
            TURN_START_TIMEOUT,
            "hook-callback drivers wait for SessionStart/UserPromptSubmit"
        );
        assert_eq!(
            turn_start_timeout_for_driver(&GrokDriver::default()),
            TURN_START_TIMEOUT
        );
        assert_eq!(
            turn_start_timeout_for_driver(&CodexDriver::default()),
            crate::agent_jsonl_discovery::DISCOVERY_TIMEOUT,
            "AgentJsonlFile drivers inherit rollout discovery's budget"
        );
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
            || async { false },
        )
        .await
        .expect_err("must fail the spawn");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "{msg}");
        assert!(msg.contains("exec-missing-composer"), "{msg}");
        assert!(msg.contains("composer never became ready"), "{msg}");
        assert!(msg.contains("failed spawn"), "{msg}");
        assert!(matches!(
            spawn_confirmation_error(&err),
            Some(SpawnConfirmationError::ComposerNotReady { .. })
        ));
    }

    #[tokio::test]
    async fn confirm_spawn_started_succeeds_when_turn_starts_without_composer_chrome() {
        confirm_spawn_started(
            "claude",
            "exec-hook-without-chrome",
            Duration::from_millis(50),
            Duration::from_millis(50),
            Duration::from_millis(5),
            || async { false },
            || async { true },
        )
        .await
        .expect("turn-start evidence must satisfy composer readiness");
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
        assert!(matches!(
            spawn_confirmation_error(&err),
            Some(SpawnConfirmationError::TurnDidNotStart { .. })
        ));
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

    #[tokio::test]
    async fn remote_turn_start_timeout_reaps_then_fails() {
        let reaped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&reaped);
        let err = confirm_turn_start_or_reap(
            "claude",
            "exec-remote-silent",
            Duration::from_millis(30),
            Duration::from_millis(5),
            || async { false },
            || {
                let flag = Arc::clone(&flag);
                async move {
                    flag.store(true, Ordering::SeqCst);
                }
            },
        )
        .await
        .expect_err("a silent remote worker must fail the spawn");
        assert!(reaped.load(Ordering::SeqCst), "timeout must reap the remote pid");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "{msg}");
        assert!(msg.contains("exec-remote-silent"), "{msg}");
        assert!(msg.contains("no driver hook or session event"), "{msg}");
    }

    #[tokio::test]
    async fn remote_turn_start_succeeds_without_reaping() {
        let reaped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&reaped);
        confirm_turn_start_or_reap(
            "claude",
            "exec-remote-ok",
            Duration::from_millis(50),
            Duration::from_millis(5),
            || async { true },
            || {
                let flag = Arc::clone(&flag);
                async move {
                    flag.store(true, Ordering::SeqCst);
                }
            },
        )
        .await
        .expect("turn-start evidence must complete the remote wait");
        assert!(!reaped.load(Ordering::SeqCst));
    }

    #[test]
    fn current_run_turn_start_ignores_an_old_transcript_bearing_run() {
        use boss_protocol::{ExecutionStatus, FinishExecutionRunInput};

        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let chore = crate::test_support::create_test_chore(&db, product.id.clone(), "Relaunch");
        let ready = crate::test_support::create_ready_chore_execution(&db, chore.id.clone());
        let (execution, old_run) = db
            .start_execution_run(&ready.id, "worker-1", "foo", "lease-1", "ws-1", "/tmp/ws-1")
            .unwrap();
        db.set_run_transcript_path_if_unset(&execution.id, "/tmp/old-session.jsonl")
            .unwrap();
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(execution.id.clone())
                .run_id(old_run.id)
                .execution_status(ExecutionStatus::WaitingHuman)
                .run_status("completed")
                .clear_workspace_lease(false)
                .build(),
        )
        .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'ready' WHERE id = ?1",
                rusqlite::params![&execution.id],
            )
            .unwrap();
        db.start_execution_run(&execution.id, "worker-1", "foo", "lease-2", "ws-2", "/tmp/ws-2")
            .unwrap();

        let historical = db
            .list_runs(&execution.id)
            .unwrap()
            .iter()
            .any(|run| run.transcript_path.as_deref().is_some_and(|path| !path.is_empty()));
        assert!(historical, "fixture must include an older transcript-bearing run");
        assert!(
            !current_run_has_turn_start_evidence(&db, None, &execution.id),
            "a silent relaunch must not inherit an old run's transcript_path"
        );
    }

    #[test]
    fn current_run_turn_start_accepts_a_driver_signal_without_transcript_path() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let chore = crate::test_support::create_test_chore(&db, product.id.clone(), "Hook only");
        let ready = crate::test_support::create_ready_chore_execution(&db, chore.id.clone());
        let (execution, _run) = db
            .start_execution_run(&ready.id, "worker-1", "foo", "lease-1", "ws-1", "/tmp/ws-1")
            .unwrap();
        let registry = LiveWorkerStateRegistry::new();
        registry.register_spawn(1, execution.id.clone(), "opus", 42, None);
        registry.record_driver_signal(&execution.id, DriverSignalKind::HookEvent);
        assert!(db.transcript_path_for_execution(&execution.id).unwrap().is_none());
        assert!(current_run_has_turn_start_evidence(&db, Some(&registry), &execution.id));
    }
}
