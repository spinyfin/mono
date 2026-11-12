//! Bounded spawn-time confirmation that the worker driver actually started.
//!
//! After the pane's initial input is delivered (today: a login-shell `-c`
//! that sources `.boss/initial-input.sh` and runs the private terminal
//! launcher), the engine waits for two independent pieces of
//! evidence before treating the spawn as successful:
//!
//! 1. **Composer readiness** — driver-specific PTY evidence that the CLI
//!    is actually up (`PaneMonitorSpec` agent markers), or a fresh driver signal.
//!    The launcher delivers the brief once the composer can accept
//!    input: the CLI has exec'd and rendered its surface, which is what
//!    proves the sourced script did not die at `execve()`. A driver hook
//!    or persisted `transcript_path` is strictly stronger than a scraped
//!    pane marker, so turn-start evidence also satisfies this wait.
//! 2. **Turn start** — fresh evidence for the **current** run: a driver
//!    hook / session event (`LiveWorkerStateRegistry::has_driver_signal_for_run`)
//!    or a `transcript_path` on the current `work_runs` row
//!    (`WorkDb::transcript_path_for_execution`). Historical runs of the
//!    same execution do not count.
//!
//! 3. **Prompt accepted** — evidence that the driver began a turn
//!    ([`DriverSignals::has_prompt_accepted_for_run`]: `UserPromptSubmit` or
//!    any later turn event). A `SessionStart` satisfies the second wait and
//!    says nothing here: a CLI can be up with its brief pasted into the
//!    composer and never submitted. If no turn begins once the launcher has
//!    reported its Return, the engine presses Return again (never the paste)
//!    a bounded number of times, then fails the spawn. See
//!    [`confirm_prompt_submitted`].
//!
//! Any wait timing out fails the spawn immediately with a named error
//! so the coordinator records `pane_spawn_failed` instead of leaving the
//! execution `Spawning` for `spawn_ack_sweep`'s later generic reap.
//! "Composer never became ready" is reported only when neither composer
//! chrome nor turn-start evidence arrives.

use std::path::PathBuf;
use std::time::Duration;

use boss_protocol::PaneMonitorSpec;

pub use boss_execution_contracts::{
    SpawnConfirmationError, composer_not_ready_error, confirm_prompt_accepted_or_reap, prompt_not_delivered_error,
    prompt_not_submitted_error, remote_prompt_not_submitted_error, spawn_confirmation_error, turn_did_not_start_error,
    wait_until,
};

use crate::driver::initial_prompt::{DELIVERY_BUDGET, DeliveryState};
use crate::driver::{AgentDriver, ProgressIngress, ProgressObservationConfig};
use crate::host::DriverSignals;
use crate::work::WorkDb;

/// How long spawn may wait for driver-specific PTY evidence that the CLI
/// is up. Sized well above a healthy driver's first paint (seconds) and
/// well below `live_worker_state::DRIVER_START_GRACE_SECS` so a
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
/// the injected discovery deadline. Use
/// [`turn_start_timeout_for_driver`] rather than this constant at the
/// spawn call site.
pub(crate) const TURN_START_TIMEOUT: Duration = Duration::from_secs(45);

/// How long a driver may take to report a turn once Return has reached it,
/// for **hook-callback** drivers. Use [`prompt_confirm_timeout_for_driver`]
/// at the spawn call site.
///
/// Measured against Claude Code 2.1.322 with 47 KB briefs, from the launcher
/// writing Return to the `UserPromptSubmit` hook running: 37-198 ms on a host
/// at its normal load (92 runs), and 0.13-4.0 s with 250 busy-loop processes
/// on 12 cores (34 runs). 30 s is 7.5 times that peak. When it expires the
/// engine presses Return again; it fails the spawn only after
/// [`SUBMIT_RETRIES`] further waits of the same length.
pub const SUBMIT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times Return is re-sent to a delivered prompt that produced no
/// turn before the spawn fails.
pub const SUBMIT_RETRIES: u32 = 2;

/// How long the launcher may go without reporting that it sent Return (or
/// that it gave up) before the engine stops waiting. The launcher bounds its
/// own stages and reports a failure itself, so this only catches a launcher
/// that died without a trace; it sits above the launcher's whole budget so
/// the engine never fails a spawn the launcher is still working on.
///
/// Measured from pane spawn to the launcher's Return: 0.6-4.9 s at normal
/// load (71 runs) and 13.9-63.8 s with 250 busy-loop processes on 12 cores
/// (30 runs), where Claude Code took up to 46 s to commit a deferred paste.
/// This is 4.7 times that peak.
pub(crate) const DELIVERY_REPORT_TIMEOUT: Duration = Duration::from_secs(DELIVERY_BUDGET.as_secs() + 30);

/// Poll period for the confirmation waits.
pub const SPAWN_CONFIRM_POLL: Duration = Duration::from_millis(100);

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
/// the injected discovery deadline.
pub fn turn_start_timeout_for_driver(driver: &dyn AgentDriver, discovery_timeout: Duration) -> Duration {
    hook_or_discovery_timeout(driver, TURN_START_TIMEOUT, discovery_timeout)
}

/// Wait for turn evidence after Return, sized like
/// [`turn_start_timeout_for_driver`]: hook-callback drivers report
/// `UserPromptSubmit` directly and use [`SUBMIT_CONFIRM_TIMEOUT`];
/// [`ProgressIngress::AgentJsonlFile`] drivers report it through the rollout
/// the engine must first discover, so they inherit that budget.
pub fn prompt_confirm_timeout_for_driver(driver: &dyn AgentDriver, discovery_timeout: Duration) -> Duration {
    hook_or_discovery_timeout(driver, SUBMIT_CONFIRM_TIMEOUT, discovery_timeout)
}

fn hook_or_discovery_timeout(driver: &dyn AgentDriver, hook: Duration, discovery: Duration) -> Duration {
    let dummy = ProgressObservationConfig {
        events_socket_path: PathBuf::from("/dev/null"),
        lease_id: String::new(),
        run_id: String::new(),
        workspace_path: PathBuf::from("/"),
        forwarder_binary: PathBuf::from("/dev/null"),
    };
    match driver.progress_observation_wiring(&dummy) {
        ProgressIngress::AgentJsonlFile(_) => discovery,
        ProgressIngress::HookCallback(_) | ProgressIngress::StdoutJsonl => hook,
    }
}

/// The bounds on confirming that a delivered prompt became a turn.
#[derive(Debug, Clone, Copy)]
pub struct PromptSubmitPolicy {
    /// See [`DELIVERY_REPORT_TIMEOUT`].
    pub delivery_timeout: Duration,
    /// See [`SUBMIT_CONFIRM_TIMEOUT`]; applies afresh after each Return.
    pub confirm_timeout: Duration,
    /// See [`SUBMIT_RETRIES`].
    pub retries: u32,
    pub poll: Duration,
}

impl PromptSubmitPolicy {
    pub fn for_driver(driver: &dyn AgentDriver, discovery_timeout: Duration) -> Self {
        Self {
            delivery_timeout: DELIVERY_REPORT_TIMEOUT,
            confirm_timeout: prompt_confirm_timeout_for_driver(driver, discovery_timeout),
            retries: SUBMIT_RETRIES,
            poll: SPAWN_CONFIRM_POLL,
        }
    }

    /// The longest a worker whose prompt is never accepted can be held
    /// before it is failed, when nothing can re-send Return to it.
    pub fn unattended_timeout(&self) -> Duration {
        self.delivery_timeout + self.confirm_timeout
    }
}

/// Fresh turn-start evidence for the **current** run of `execution_id`.
/// A historical `work_runs` row with a transcript does not count; a driver
/// hook that never persisted `transcript_path` does.
pub fn current_run_has_turn_start_evidence(
    db: &WorkDb,
    live_states: Option<&dyn DriverSignals>,
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
    composer_ready: Ready,
    mut turn_started: Started,
) -> anyhow::Result<()>
where
    Ready: FnMut() -> ReadyFut,
    ReadyFut: std::future::Future<Output = bool>,
    Started: FnMut() -> StartedFut,
    StartedFut: std::future::Future<Output = bool>,
{
    // Poll signals independently: a stuck tmux capture must not hide a hook.
    let composer_ready = tokio::select! {
        biased;
        started = wait_until(composer_timeout, poll, &mut turn_started) => {
            if started { return Ok(()); }
            false
        }
        ready = wait_until(composer_timeout, poll, composer_ready) => ready,
    };
    if !composer_ready {
        // Recheck evidence at the deadline before classifying a dead exec.
        if tokio::time::timeout(poll, turn_started()).await.unwrap_or(false) {
            return Ok(());
        }
        return Err(composer_not_ready_error(driver_name, run_id, composer_timeout));
    }
    if !wait_until(turn_timeout, poll, turn_started).await {
        return Err(turn_did_not_start_error(driver_name, run_id, turn_timeout));
    }
    Ok(())
}

/// Third spawn-time wait: the delivered prompt must become a turn.
///
/// `accepted` is turn evidence for the current run. `delivery` is what the
/// launcher has durably reported. `resubmit` presses Return in the worker's
/// pane and nothing else.
///
/// Return is re-sent only after the launcher reports that it wrote the paste
/// and its own Return ([`DeliveryState::Submitted`]), so it can never land in
/// a startup dialog, and only when a full `confirm_timeout` has passed with
/// no turn. Re-sending is idempotent: the brief is pasted exactly once, by
/// the launcher, and Return on an empty composer does nothing.
pub async fn confirm_prompt_submitted<Accepted, AcceptedFut, Delivery, DeliveryFut, Resubmit, ResubmitFut, Cancelled>(
    driver_name: &str,
    run_id: &str,
    policy: PromptSubmitPolicy,
    mut accepted: Accepted,
    mut delivery: Delivery,
    mut resubmit: Resubmit,
    mut cancelled: Cancelled,
) -> anyhow::Result<()>
where
    Accepted: FnMut() -> AcceptedFut,
    AcceptedFut: std::future::Future<Output = bool>,
    Delivery: FnMut() -> DeliveryFut,
    DeliveryFut: std::future::Future<Output = DeliveryState>,
    Resubmit: FnMut() -> ResubmitFut,
    Cancelled: FnMut() -> bool,
    ResubmitFut: std::future::Future<Output = anyhow::Result<()>>,
{
    let started = tokio::time::Instant::now();
    let elapsed_ms = || started.elapsed().as_millis() as u64;
    let mut retries = 0;
    // When Return last reached the driver: first the launcher's, then ours.
    let mut submitted_at = None;
    loop {
        if cancelled() {
            anyhow::bail!("execution cancelled during prompt confirmation");
        }
        if accepted().await {
            tracing::info!(
                run_id,
                driver = driver_name,
                retries,
                elapsed_ms = elapsed_ms(),
                "prompt submit confirmed: the driver started a turn",
            );
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        match submitted_at {
            None => match delivery().await {
                DeliveryState::Submitted => {
                    tracing::info!(
                        run_id,
                        driver = driver_name,
                        elapsed_ms = elapsed_ms(),
                        "prompt submit sent by the launcher; awaiting turn evidence",
                    );
                    submitted_at = Some(now);
                }
                DeliveryState::Failed => {
                    tracing::warn!(
                        run_id,
                        driver = driver_name,
                        elapsed_ms = elapsed_ms(),
                        "prompt submit failed: the launcher reported the brief undelivered",
                    );
                    return Err(prompt_not_delivered_error(
                        driver_name,
                        run_id,
                        "the launcher reported it could not deliver the brief",
                    ));
                }
                DeliveryState::Pending if now.duration_since(started) >= policy.delivery_timeout => {
                    tracing::warn!(
                        run_id,
                        driver = driver_name,
                        elapsed_ms = elapsed_ms(),
                        "prompt submit failed: the launcher never reported delivering the brief",
                    );
                    return Err(prompt_not_delivered_error(
                        driver_name,
                        run_id,
                        format!(
                            "the launcher reported neither delivery nor failure within {}s",
                            policy.delivery_timeout.as_secs()
                        ),
                    ));
                }
                DeliveryState::Pending => {}
            },
            Some(at) if now.duration_since(at) >= policy.confirm_timeout => {
                if retries == policy.retries {
                    tracing::warn!(
                        run_id,
                        driver = driver_name,
                        retries,
                        elapsed_ms = elapsed_ms(),
                        "prompt submit failed: no turn evidence after the final Return",
                    );
                    return Err(prompt_not_submitted_error(
                        driver_name,
                        run_id,
                        retries,
                        started.elapsed(),
                    ));
                }
                if cancelled() {
                    anyhow::bail!("execution cancelled before prompt resubmission");
                }
                retries += 1;
                let sent = tokio::time::timeout(policy.confirm_timeout, resubmit())
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("pressing Return timed out")));
                tracing::warn!(
                    run_id,
                    driver = driver_name,
                    attempt = retries,
                    of = policy.retries,
                    elapsed_ms = elapsed_ms(),
                    error = ?sent.err().map(|err| format!("{err:#}")),
                    "prompt submit retried: no turn evidence since the last Return, pressed Return again",
                );
                submitted_at = Some(tokio::time::Instant::now());
            }
            Some(_) => {}
        }
        tokio::time::sleep(policy.poll).await;
    }
}

/// Remote turn-start wait: same [`wait_until`] loop as the local path, then
/// `reap` (pid kill) when no evidence arrives.
pub async fn confirm_turn_start_or_reap<Started, StartedFut, Reap, ReapFut>(
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
        let discovery_timeout = Duration::from_secs(123);
        assert_eq!(
            turn_start_timeout_for_driver(&ClaudeDriver, discovery_timeout),
            TURN_START_TIMEOUT,
            "hook-callback drivers wait for SessionStart/UserPromptSubmit"
        );
        assert_eq!(
            turn_start_timeout_for_driver(&GrokDriver::default(), discovery_timeout),
            TURN_START_TIMEOUT
        );
        assert_eq!(
            turn_start_timeout_for_driver(&CodexDriver::default(), discovery_timeout),
            discovery_timeout,
            "AgentJsonlFile drivers inherit rollout discovery's budget"
        );
    }

    #[tokio::test]
    async fn stalled_capture_cannot_extend_dead_exec_deadline() {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            confirm_spawn_started(
                "claude",
                "dead-exec",
                Duration::from_millis(20),
                Duration::from_millis(20),
                Duration::from_millis(5),
                std::future::pending::<bool>,
                || async { false },
            ),
        )
        .await
        .expect("capture must be cancelled at the deadline");
        assert!(result.unwrap_err().to_string().contains("composer never became ready"));
    }

    #[tokio::test]
    async fn hook_without_any_visible_marker_passes() {
        confirm_spawn_started(
            "claude",
            "hook-only",
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_millis(5),
            || async { false },
            || async { true },
        )
        .await
        .expect("driver evidence proves both readiness and turn start");
    }

    #[test]
    fn claude_composer_ready_for_each_permission_mode_footer() {
        // Banner scrolled off: only the mode footer / effort line remains.
        // Auto mode, bypass-permissions mode and a bare effort indicator
        // must each be enough on their own.
        for screen in [
            "❯ \n  ⏵⏵ auto mode on (shift+tab to cycle)",
            "❯ \n  ⏵⏵ bypass permissions on (shift+tab to cycle)",
            "❯ \n  /effort",
            "Claude Code v2.1.283",
        ] {
            assert!(pane_shows_driver_ready(screen, &claude_spec()), "{screen:?}");
        }
        // A bypass-mode screen with neither banner nor a known footer is a
        // genuine miss; capture depth, not this predicate, must cover it.
        assert!(!pane_shows_driver_ready("❯ \n  ? for shortcuts", &claude_spec()));
    }

    #[test]
    fn shell_and_error_output_is_not_claude_ready() {
        for screen in [
            "bash: claude: command not found",
            "zsh: exec format error: claude\n❯ ",
            "Accessing workspace: /tmp/ws\nQuick safety check: trust this folder?",
        ] {
            assert!(!pane_shows_driver_ready(screen, &claude_spec()), "{screen:?}");
        }
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
    fn prompt_confirm_timeout_follows_the_driver_ingress() {
        let discovery_timeout = Duration::from_secs(123);
        for hook_driver in [&ClaudeDriver as &dyn AgentDriver, &GrokDriver::default()] {
            let policy = PromptSubmitPolicy::for_driver(hook_driver, discovery_timeout);
            assert_eq!(policy.confirm_timeout, SUBMIT_CONFIRM_TIMEOUT);
            assert_eq!(policy.delivery_timeout, DELIVERY_REPORT_TIMEOUT);
            assert_eq!(policy.retries, SUBMIT_RETRIES);
            assert_eq!(
                policy.unattended_timeout(),
                DELIVERY_REPORT_TIMEOUT + SUBMIT_CONFIRM_TIMEOUT
            );
        }
        assert_eq!(
            PromptSubmitPolicy::for_driver(&CodexDriver::default(), discovery_timeout).confirm_timeout,
            discovery_timeout,
            "a rollout-fed driver reports its turn through the file the engine must first discover"
        );
        assert!(
            DELIVERY_REPORT_TIMEOUT > DELIVERY_BUDGET,
            "the engine must outwait a launcher that is still working"
        );
    }

    const SHORT_POLICY: PromptSubmitPolicy = PromptSubmitPolicy {
        delivery_timeout: Duration::from_millis(500),
        confirm_timeout: Duration::from_millis(100),
        retries: 2,
        poll: Duration::from_millis(5),
    };

    /// Drive [`confirm_prompt_submitted`] against a worker whose launcher
    /// reports `delivery` after `delivered_after`, and which starts its turn
    /// once Return has been re-sent `accepts_after_resubmits` times (`None`:
    /// never, `Some(0)`: on the launcher's own Return). Returns the result,
    /// how many times Return was re-sent, and whether any of those came
    /// before the launcher reported delivery.
    async fn confirm_prompt(
        delivery: DeliveryState,
        delivered_after: Duration,
        accepts_after_resubmits: Option<u32>,
    ) -> (anyhow::Result<()>, u32, bool) {
        use std::sync::atomic::AtomicU32;
        let started = tokio::time::Instant::now();
        let resubmits = AtomicU32::new(0);
        let early_resubmit = AtomicBool::new(false);
        let reported = || started.elapsed() >= delivered_after;
        let result = confirm_prompt_submitted(
            "claude",
            "exec-prompt",
            SHORT_POLICY,
            || async {
                reported()
                    && delivery == DeliveryState::Submitted
                    && accepts_after_resubmits.is_some_and(|needed| resubmits.load(Ordering::SeqCst) >= needed)
            },
            || async { if reported() { delivery } else { DeliveryState::Pending } },
            || async {
                if !reported() {
                    early_resubmit.store(true, Ordering::SeqCst);
                }
                resubmits.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || false,
        )
        .await;
        (
            result,
            resubmits.load(Ordering::SeqCst),
            early_resubmit.load(Ordering::SeqCst),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn an_accepted_prompt_is_confirmed_without_pressing_return() {
        let (result, resubmits, _) = confirm_prompt(DeliveryState::Submitted, Duration::from_millis(50), Some(0)).await;
        result.expect("turn evidence must confirm the prompt");
        assert_eq!(resubmits, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn return_is_resent_only_after_the_launcher_delivered_and_a_full_wait_passed() {
        // The launcher takes four confirm windows to deliver. No Return may
        // be pressed while it is still working, however long that takes.
        let (result, resubmits, early) =
            confirm_prompt(DeliveryState::Submitted, Duration::from_millis(400), Some(1)).await;
        result.expect("the re-sent Return starts the turn");
        assert_eq!(resubmits, 1);
        assert!(!early, "Return was pressed before the launcher delivered the brief");
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompt_that_never_becomes_a_turn_fails_after_bounded_retries() {
        let started = tokio::time::Instant::now();
        let (result, resubmits, _) = confirm_prompt(DeliveryState::Submitted, Duration::ZERO, None).await;
        let err = result.expect_err("must fail the spawn");
        assert_eq!(resubmits, SHORT_POLICY.retries);
        assert!(matches!(
            spawn_confirmation_error(&err),
            Some(SpawnConfirmationError::PromptNotSubmitted { retries: 2, .. })
        ));
        let msg = err.to_string();
        assert!(
            msg.contains("exec-prompt") && msg.contains("never started a turn"),
            "{msg}"
        );
        // One wait after the launcher's Return and one after each retry.
        let waited = started.elapsed();
        assert!(
            waited >= SHORT_POLICY.confirm_timeout * 3 && waited < SHORT_POLICY.confirm_timeout * 4,
            "{waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_undelivered_brief_fails_without_pressing_return() {
        let (failed, resubmits, _) = confirm_prompt(DeliveryState::Failed, Duration::from_millis(50), None).await;
        let err = failed.expect_err("a launcher failure must fail the spawn");
        assert_eq!(resubmits, 0);
        assert!(matches!(
            spawn_confirmation_error(&err),
            Some(SpawnConfirmationError::PromptNotDelivered { .. })
        ));
        assert!(err.to_string().contains("could not deliver the brief"), "{err}");

        let started = tokio::time::Instant::now();
        let (silent, resubmits, _) = confirm_prompt(DeliveryState::Pending, Duration::ZERO, None).await;
        let err = silent.expect_err("a launcher that never reports must fail the spawn");
        assert_eq!(resubmits, 0);
        assert!(err.to_string().contains("neither delivery nor failure"), "{err}");
        assert!(started.elapsed() >= SHORT_POLICY.delivery_timeout);
        assert!(spawn_confirmation_error(&err).is_some_and(SpawnConfirmationError::is_unsubmitted_prompt));
    }

    #[tokio::test(start_paused = true)]
    async fn a_return_that_cannot_be_sent_still_ends_in_a_failed_spawn() {
        let err = confirm_prompt_submitted(
            "claude",
            "exec-no-pane",
            SHORT_POLICY,
            || async { false },
            || async { DeliveryState::Submitted },
            || async { Err(anyhow::anyhow!("no such session")) },
            || false,
        )
        .await
        .expect_err("must fail the spawn");
        assert!(err.to_string().contains("never started a turn"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn remote_prompt_never_accepted_reaps_then_fails() {
        let reaped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&reaped);
        let err = confirm_prompt_accepted_or_reap(
            "claude",
            "exec-remote-idle",
            Duration::from_millis(30),
            Duration::from_millis(5),
            || async { false },
            || async move { flag.store(true, Ordering::SeqCst) },
        )
        .await
        .expect_err("a remote worker that never starts a turn must fail the spawn");
        assert!(reaped.load(Ordering::SeqCst), "timeout must reap the remote pid");
        assert!(matches!(
            spawn_confirmation_error(&err),
            Some(SpawnConfirmationError::RemotePromptNotSubmitted { .. })
        ));
        assert!(spawn_confirmation_error(&err).is_some_and(SpawnConfirmationError::is_unsubmitted_prompt));
        let msg = err.to_string();
        assert!(msg.contains("unknown"), "{msg}");
        assert!(!msg.contains("re-sent 0"), "{msg}");

        let reaped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&reaped);
        confirm_prompt_accepted_or_reap(
            "claude",
            "exec-remote-working",
            Duration::from_millis(30),
            Duration::from_millis(5),
            || async { true },
            || async move { flag.store(true, Ordering::SeqCst) },
        )
        .await
        .expect("turn evidence must complete the remote wait");
        assert!(!reaped.load(Ordering::SeqCst), "an accepted prompt must not be reaped");
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
}
