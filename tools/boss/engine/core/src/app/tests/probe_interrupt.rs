//! Interrupting probe delivery: cut the turn short, prove it stopped, then
//! type — and fail visibly when the turn will not stop.
//!
//! The defect these cover is the one `bossctl probe` used to state in its own
//! output: *"delivery is BEST-EFFORT … that boundary may never arrive while
//! the worker is still alive"*. A probe exists to redirect a worker that is
//! currently going the wrong way, so the case it could not serve — a worker
//! mid-run whose next turn boundary is its last — was the common case. Every
//! test here pins one property of the fix:
//!
//! * the interrupt happens **before** the write, and the write happens only
//!   after the turn is confirmed ended;
//! * an interrupt that does not take produces a **terminal, visibly failed**
//!   probe with **nothing typed into the pane**, not a quiet downgrade to
//!   boundary delivery;
//! * an already-parked worker is written to directly, with no pointless
//!   interrupt discarding work it was not doing;
//! * long multi-line probe text reaches the pane whole, through the same
//!   bracketed-paste path that fixed the tty canonical-line-length
//!   truncation — not a second hand-rolled pty write.

use super::*;

use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use boss_protocol::{ProbeInterruptOutcome, WorkerEvent};
use boss_tmux::{CommandOutput, CommandRunner, Tmux};

use crate::app::executions;
use crate::app::probe_interrupt::INTERRUPT_NOTICE;

/// Records every tmux invocation (and any stdin fed to `load-buffer`) so a
/// test can assert on the exact gesture sequence: which keys went out, in
/// which order, and what text — if any — reached the pane.
///
/// Asserting on the *whole* sequence is the point. "An Escape was sent" and
/// "an Escape was sent and then nothing was typed" are the difference between
/// a correct failure and a corrupted one, and only the full call list can tell
/// them apart.
#[derive(Default)]
struct RecordingTmux {
    calls: StdMutex<Vec<Vec<String>>>,
    stdin: StdMutex<Vec<Vec<u8>>>,
}

impl RecordingTmux {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    fn stdin(&self) -> Vec<Vec<u8>> {
        self.stdin.lock().unwrap().clone()
    }

    /// How many `send-keys <session> Escape` calls were issued — i.e. how many
    /// interrupt presses actually reached the pty.
    fn escape_presses(&self) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.contains(&"send-keys".to_owned()) && call.last().map(String::as_str) == Some("Escape"))
            .count()
    }

    /// Whether any call carried prompt text into the pane, by either route
    /// `Tmux::send_keys` uses: `load-buffer` for multi-line text, literal
    /// `send-keys -l` chunks for single-line.
    fn wrote_text(&self) -> bool {
        self.calls()
            .iter()
            .any(|call| call.contains(&"load-buffer".to_owned()) || call.contains(&"-l".to_owned()))
    }

    fn success() -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

#[async_trait]
impl CommandRunner for RecordingTmux {
    async fn run(&self, _program: &Path, args: &[OsString], _cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect());
        Ok(Self::success())
    }

    async fn run_with_stdin(
        &self,
        _program: &Path,
        args: &[OsString],
        _cwd: Option<&Path>,
        stdin: &[u8],
    ) -> std::io::Result<CommandOutput> {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect());
        self.stdin.lock().unwrap().push(stdin.to_vec());
        Ok(Self::success())
    }
}

/// Point `server_state`'s pane delivery at a recording tmux and register
/// `run_id` as a tmux-hosted pane on `slot_id`.
fn tmux_hosted(server_state: &Arc<ServerState>, run_id: &str, slot_id: u8) -> Arc<RecordingTmux> {
    let runner = Arc::new(RecordingTmux::default());
    server_state
        .worker_registry
        .register_tmux_run_slot(run_id, slot_id, "boss-probe-interrupt");
    *server_state.pane_delivery_tmux_override.write().unwrap() =
        Some(Tmux::with_runner("/usr/bin/tmux", runner.clone()).unwrap());
    runner
}

fn dispatch_for(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

fn probe_request(run_id: &str, text: &str) -> FrontendRequest {
    FrontendRequest::ProbeRun {
        run_id: run_id.to_owned(),
        text: text.to_owned(),
        urgent: false,
        interrupt: true,
    }
}

/// Park `slot_id` at its prompt, as the driver's turn-boundary signal would
/// once the interrupted turn actually unwinds.
fn park_slot(server_state: &ServerState, slot_id: u8) {
    server_state.live_worker_states.apply_event(
        slot_id,
        &WorkerEvent::Stop {
            session_id: "test-sess".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Interrupted,
        },
    );
}

/// Spawn a task that watches `runner` for the interrupt keystroke and then
/// parks the slot — standing in for the driver's turn-boundary signal
/// arriving after a real Esc (Claude's `Stop` hook, Codex's rollout
/// `turn_aborted`, Grok's recovery observer).
///
/// Deliberately keyed on the Escape actually being *sent*: parking
/// unconditionally would let a test pass even if the engine typed first and
/// interrupted second, which is the exact ordering bug this whole path
/// exists to prevent.
fn park_once_interrupted(
    server_state: &Arc<ServerState>,
    runner: &Arc<RecordingTmux>,
    slot_id: u8,
) -> tokio::task::JoinHandle<()> {
    let server_state = server_state.clone();
    let runner = runner.clone();
    tokio::spawn(async move {
        for _ in 0..2000 {
            if runner.escape_presses() > 0 {
                park_slot(&server_state, slot_id);
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("engine never sent an interrupt keystroke");
    })
}

/// Confirm the injected text the way the worker's CLI does, by firing the
/// `UserPromptSubmit` hook once the pane write has gone out. Reads the text
/// back out of what actually reached tmux rather than reconstructing it, so
/// the confirmation cannot accidentally paper over a mangled write.
fn confirm_write_when_it_lands(
    server_state: &Arc<ServerState>,
    runner: &Arc<RecordingTmux>,
    run_id: &str,
) -> tokio::task::JoinHandle<()> {
    let server_state = server_state.clone();
    let runner = runner.clone();
    let run_id = run_id.to_owned();
    tokio::spawn(async move {
        for _ in 0..4000 {
            if let Some(written) = runner.stdin().first() {
                let prompt = String::from_utf8_lossy(written).into_owned();
                dispatch_live_worker_state(
                    &server_state,
                    &crate::events_socket::IncomingHookEvent::for_test(
                        WorkerEvent::UserPromptSubmit {
                            session_id: "test-sess".into(),
                            prompt,
                        },
                        Some(run_id.clone()),
                        None,
                    ),
                )
                .await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("engine never wrote the probe text into the pane");
    })
}

// ── The headline behaviour ──────────────────────────────────────────────────

/// A probe against a **genuinely mid-turn** worker interrupts the turn,
/// confirms it ended, and only then writes — and the write carries the notice
/// telling the worker its last action was cut off.
///
/// The ordering assertion is the substance: the Escape must precede the text.
/// A write issued into a still-running turn is how injected input gets
/// swallowed or re-read as an answer to something else.
#[tokio::test]
async fn interrupting_probe_cuts_the_turn_short_before_writing() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 3, None);
    let runner = tmux_hosted(&server_state, &run_id, 3);
    let parker = park_once_interrupted(&server_state, &runner, 3);
    let confirmer = confirm_write_when_it_lands(&server_state, &runner, &run_id);

    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        probe_request(&run_id, "stop — you are building the wrong target"),
    )
    .await;
    parker.await.expect("parker task");
    confirmer.await.expect("confirmer task");

    match sole_response(&sink).await {
        FrontendEvent::ProbeDelivered {
            state,
            interrupt,
            interrupt_attempts,
            ..
        } => {
            assert_eq!(
                state,
                ProbeDeliveryState::Consumed,
                "the worker's CLI confirmed the prompt, so this is a delivery, not an intention",
            );
            assert_eq!(interrupt, ProbeInterruptOutcome::Interrupted);
            assert_eq!(interrupt_attempts, 1, "one Escape was enough for this driver");
        }
        other => panic!("expected ProbeDelivered, got {other:?}"),
    }

    // Ordering: interrupt first, text second. Never the reverse.
    let calls = runner.calls();
    let escape_at = calls
        .iter()
        .position(|call| call.last().map(String::as_str) == Some("Escape"))
        .expect("an Escape must have been sent");
    let text_at = calls
        .iter()
        .position(|call| call.contains(&"load-buffer".to_owned()))
        .expect("the probe text must have been written");
    assert!(
        escape_at < text_at,
        "the interrupt must precede the write; got calls {calls:?}",
    );
    assert_eq!(runner.escape_presses(), 1, "one probe must not interrupt repeatedly");

    let written = String::from_utf8(runner.stdin().first().cloned().expect("text reached load-buffer")).unwrap();
    assert!(
        written.starts_with(INTERRUPT_NOTICE),
        "an interrupted worker must be told its last action was cut off: {written}",
    );
    assert!(written.contains("stop — you are building the wrong target"));
}

/// The driver that this change matters most for. Grok declares
/// `MidTurnPaneInput::Rejects`, so a mid-turn probe to it used to be accepted
/// with the `next_turn_boundary` expectation — the one the CLI has to warn is
/// best-effort and "may never arrive while the worker is still alive".
/// Interrupting makes the boundary happen, so the same probe now lands.
#[tokio::test]
async fn interrupting_probe_lands_on_a_driver_that_refuses_mid_turn_input() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 4, Some("grok"));
    let runner = tmux_hosted(&server_state, &run_id, 4);
    let parker = park_once_interrupted(&server_state, &runner, 4);
    let confirmer = confirm_write_when_it_lands(&server_state, &runner, &run_id);

    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        probe_request(&run_id, "the operator changed the plan; re-read the description"),
    )
    .await;
    parker.await.expect("parker task");
    confirmer.await.expect("confirmer task");

    match sole_response(&sink).await {
        FrontendEvent::ProbeDelivered { state, interrupt, .. } => {
            assert_eq!(state, ProbeDeliveryState::Consumed);
            assert_eq!(interrupt, ProbeInterruptOutcome::Interrupted);
        }
        other => panic!("expected ProbeDelivered, got {other:?}"),
    }
}

/// An idle worker has no turn to interrupt, so none is attempted. Interrupting
/// costs the worker whatever it was doing; spending that when it was doing
/// nothing is pure loss — and on Claude Code a second Escape at the prompt
/// opens the rewind picker, a modal that would swallow the text.
#[tokio::test]
async fn an_already_idle_worker_is_written_to_without_a_pointless_interrupt() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 5, None);
    park_slot(&server_state, 5);
    let runner = tmux_hosted(&server_state, &run_id, 5);
    let confirmer = confirm_write_when_it_lands(&server_state, &runner, &run_id);

    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        probe_request(&run_id, "when you resume, start with the failing test"),
    )
    .await;
    confirmer.await.expect("confirmer task");

    match sole_response(&sink).await {
        FrontendEvent::ProbeDelivered {
            state,
            interrupt,
            interrupt_attempts,
            ..
        } => {
            assert_eq!(state, ProbeDeliveryState::Consumed);
            assert_eq!(interrupt, ProbeInterruptOutcome::NotNeeded);
            assert_eq!(interrupt_attempts, 0);
        }
        other => panic!("expected ProbeDelivered, got {other:?}"),
    }
    assert_eq!(
        runner.escape_presses(),
        0,
        "no interrupt may be sent to a worker that is already at its prompt",
    );
    let written = String::from_utf8(runner.stdin().first().cloned().expect("text reached the pane")).unwrap();
    assert!(
        !written.contains("cut your current turn"),
        "a worker whose turn was not cut short must not be told it was: {written}",
    );
}

// ── Failing loudly ──────────────────────────────────────────────────────────

/// The interrupt does not take. The probe must reach a terminal, visibly
/// failed state, **nothing may be typed into the pane**, and an operator must
/// be told without having to run `probe-status`.
///
/// Paused clock so the driver's full attempt budget elapses instantly; the
/// engine's sleeps are the only thing being skipped, and every gesture it
/// sends is still recorded.
#[tokio::test(start_paused = true)]
async fn an_interrupt_that_never_takes_fails_terminally_and_writes_nothing() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 6, None);
    let runner = tmux_hosted(&server_state, &run_id, 6);
    // No parker: the slot stays `Working` for the whole call, exactly as a
    // wedged worker would.

    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        probe_request(&run_id, "abandon the refactor"),
    )
    .await;

    let probe_id = match sole_response(&sink).await {
        FrontendEvent::ProbeDelivered {
            probe_id,
            state,
            interrupt,
            interrupt_attempts,
            detail,
            ..
        } => {
            assert_eq!(
                state,
                ProbeDeliveryState::InterruptFailed,
                "an undelivered probe must not look like a delivered one",
            );
            assert!(state.is_terminal() && state.is_undeliverable());
            assert_eq!(interrupt, ProbeInterruptOutcome::Failed);
            assert_eq!(
                interrupt_attempts, 2,
                "the driver's declared attempt budget is spent, and no more",
            );
            let detail = detail.expect("a failure must say why");
            assert!(
                detail.contains("nothing was written into the pane"),
                "the caller must be told no text landed: {detail}",
            );
            probe_id
        }
        other => panic!("expected ProbeDelivered, got {other:?}"),
    };

    assert!(
        !runner.wrote_text(),
        "typing into a turn that never ended is the corruption this path exists to prevent; \
         calls were {:?}",
        runner.calls(),
    );
    assert_eq!(runner.escape_presses(), 2, "bounded attempts, not an interrupt loop");
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::InterruptFailed),
        "probe-status must report the same verdict the caller was given",
    );
    assert!(
        !server_state.has_pending_probe(&run_id),
        "a terminally failed probe must not be left in the queue for a later boundary to deliver \
         text the caller was told never arrived",
    );
    assert!(
        !server_state.has_in_flight_probe(&run_id),
        "the run's single delivery slot must not stay pinned by a probe nothing will deliver",
    );

    // Surfaced where an operator will see it without being asked.
    let attentions = server_state
        .work_db
        .list_attention_items(&run_id)
        .expect("attention items readable");
    assert!(
        attentions
            .iter()
            .any(|item| item.kind == crate::app::probes::PROBE_UNDELIVERED_ATTENTION_KIND),
        "a probe the engine gave up on must file an attention item, not just a log line",
    );
}

/// A driver that declares no interrupt mechanism is reported as such — the
/// probe fails visibly rather than being silently downgraded to the boundary
/// delivery the caller did not ask for and was not told about.
///
/// Reached here through a run whose driver cannot be resolved at all, which is
/// the same declared answer (`interrupt_plan() == None`) arrived at by the
/// fail-closed route.
#[tokio::test(start_paused = true)]
async fn a_driver_with_no_interrupt_mechanism_is_reported_not_silently_downgraded() {
    let (server_state, _dir) = test_server_state();
    // A registered, mid-turn slot with no execution row behind it: no driver
    // resolves, so no interrupt plan exists.
    register_working_worker(&server_state, "run-unresolvable-driver", 7);
    let runner = tmux_hosted(&server_state, "run-unresolvable-driver", 7);

    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        probe_request("run-unresolvable-driver", "course-correct now"),
    )
    .await;

    match sole_response(&sink).await {
        FrontendEvent::ProbeDelivered {
            state,
            interrupt,
            interrupt_attempts,
            detail,
            ..
        } => {
            assert_eq!(state, ProbeDeliveryState::InterruptFailed);
            assert_eq!(interrupt, ProbeInterruptOutcome::DriverCannotInterrupt);
            assert_eq!(
                interrupt_attempts, 0,
                "no gesture may be sent for a driver that has none"
            );
            assert!(
                detail
                    .expect("a failure must say why")
                    .contains("no interrupt mechanism"),
                "the caller must be told which capability is missing",
            );
        }
        other => panic!("expected ProbeDelivered, got {other:?}"),
    }
    assert_eq!(runner.escape_presses(), 0);
    assert!(!runner.wrote_text());
}

// ── The tty truncation regression ───────────────────────────────────────────

/// Long, multi-line probe text must reach the pane **whole**.
///
/// Text typed into a pane was previously truncated at the tty's canonical
/// line-length limit, which silently dropped the tail of a worker's spawn
/// command. Probe text is routinely long and multi-line, so this path must go
/// through `Tmux::send_keys` — which brackets multi-line text through
/// `load-buffer`/`paste-buffer` rather than typing it — and never a
/// hand-rolled pty write of its own.
#[tokio::test]
async fn a_long_multi_line_probe_reaches_the_pane_intact() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 8, None);
    let runner = tmux_hosted(&server_state, &run_id, 8);
    let parker = park_once_interrupted(&server_state, &runner, 8);
    let confirmer = confirm_write_when_it_lands(&server_state, &runner, &run_id);

    // Comfortably past both the tty canonical limit (typically 1024 bytes)
    // and tmux's own literal-chunk size, and multi-line, which is the shape
    // that used to lose its tail.
    let body: String = (0..40)
        .map(|i| format!("line {i:02}: the operator's decision, restated at length so the payload is long\n"))
        .collect();
    let probe_text = format!("Revised plan:\n\n{body}\nAct on this before continuing.");
    assert!(probe_text.len() > 3000, "fixture must exceed the canonical line limit");

    let sink = make_session_sink();
    executions::handle_probe_run(dispatch_for(&server_state, &sink), probe_request(&run_id, &probe_text)).await;
    parker.await.expect("parker task");
    confirmer.await.expect("confirmer task");

    match sole_response(&sink).await {
        FrontendEvent::ProbeDelivered { state, .. } => assert_eq!(state, ProbeDeliveryState::Consumed),
        other => panic!("expected ProbeDelivered, got {other:?}"),
    }

    let written = String::from_utf8(runner.stdin().first().cloned().expect("text reached load-buffer")).unwrap();
    assert!(written.starts_with(INTERRUPT_NOTICE));
    for line in probe_text.lines().filter(|line| !line.trim().is_empty()) {
        assert!(written.contains(line), "probe text lost a line in transit: {line:?}");
    }
    assert!(
        written.ends_with("Act on this before continuing."),
        "the tail is exactly what canonical-mode truncation used to eat",
    );
    assert!(
        runner
            .calls()
            .iter()
            .any(|call| call.contains(&"paste-buffer".to_owned())),
        "multi-line text must go through the bracketed-paste path, not literal keystrokes",
    );
}

// ── Opting out, and the boundaries of the new path ──────────────────────────

/// `--no-interrupt` still gets the old behaviour: the probe is queued and the
/// caller is told which boundary the engine committed to. The opt-out has to
/// keep working, because interrupting has a real cost on the worker's side and
/// a message that can wait should let it finish.
#[tokio::test(start_paused = true)]
async fn opting_out_keeps_boundary_delivery_and_reports_an_expectation() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 9, Some("grok"));
    let runner = tmux_hosted(&server_state, &run_id, 9);

    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text: "no rush — read this at your next boundary".into(),
            urgent: false,
            interrupt: false,
        },
    )
    .await;

    match sole_response(&sink).await {
        FrontendEvent::ProbeQueued { expected_delivery, .. } => {
            assert_eq!(expected_delivery, Some(ProbeDeliveryExpectation::NextTurnBoundary));
        }
        other => panic!("expected ProbeQueued, got {other:?}"),
    }
    assert_eq!(
        runner.escape_presses(),
        0,
        "opting out of interrupting must not interrupt",
    );
}

/// A worker that has not started its first turn has nothing to interrupt and
/// no prompt to write to, so the interrupting request falls through to the
/// ordinary queue and says so. Inventing an interrupt outcome for a turn that
/// never existed would be a fabricated answer.
#[tokio::test(start_paused = true)]
async fn a_spawning_worker_falls_through_to_boundary_delivery() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_spawning_worker_with_driver(&server_state, 10, None);
    let runner = tmux_hosted(&server_state, &run_id, 10);

    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        probe_request(&run_id, "as soon as you are up: the spec changed"),
    )
    .await;

    match sole_response(&sink).await {
        FrontendEvent::ProbeQueued { expected_delivery, .. } => {
            assert_eq!(expected_delivery, Some(ProbeDeliveryExpectation::NextToolBoundary));
        }
        other => panic!("expected ProbeQueued, got {other:?}"),
    }
    assert_eq!(runner.escape_presses(), 0);
}

/// A run with no live pane is still refused before a probe id is minted —
/// interrupting changes where a deliverable probe lands, not what counts as
/// deliverable.
#[tokio::test]
async fn an_interrupting_probe_to_a_dead_run_is_still_refused_up_front() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    executions::handle_probe_run(
        dispatch_for(&server_state, &sink),
        probe_request("run-with-no-pane", "anyone there?"),
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ProbeRefused { reason, .. } => {
            assert!(reason.contains("no live worker pane"), "unexpected reason: {reason}");
        }
        other => panic!("expected ProbeRefused, got {other:?}"),
    }
}

// ── The claim held across the interrupt ─────────────────────────────────────

/// The interrupting path holds the run's delivery claim across the whole
/// interrupt, precisely so the boundary its own Escape produces cannot let a
/// second dispatcher deliver the same probe. The reply path must therefore
/// leave a claim alone while its probe is still `Queued` — taking it would
/// free the slot mid-delivery and let exactly that double-delivery happen.
#[tokio::test]
async fn a_turn_boundary_does_not_steal_a_claim_whose_probe_has_not_been_written() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, 11, None);
    let probe_id = server_state.queue_probe(run_id.clone(), "held across the interrupt".into(), false);
    let claimed = server_state
        .try_reserve_probe_for_delivery(&run_id, Some("/tmp/t.jsonl".to_owned()), 0)
        .expect("claim succeeds");
    assert_eq!(claimed.probe_id, probe_id);
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Queued),
        "a claim taken before the write leaves the probe at Queued",
    );

    super::super::worker_events::dispatch_probe_reply_on_stop(
        &server_state,
        &crate::events_socket::IncomingHookEvent::for_test(
            WorkerEvent::Stop {
                session_id: "test-sess".into(),
                stop_hook_active: false,
                stop_reason: crate::protocol::StopReason::Interrupted,
            },
            Some(run_id.clone()),
            None,
        ),
    )
    .await;

    assert!(
        server_state.has_in_flight_probe(&run_id),
        "the boundary the interrupt itself produced must not free the claim that interrupt is \
         holding",
    );
}

/// Re-snapshotting the in-flight transcript offset must keep the claim. The
/// alternative — release and re-claim — would reopen the very race the early
/// claim closes.
#[test]
fn updating_the_in_flight_snapshot_keeps_the_claim() {
    let (server_state, _dir) = test_server_state();
    server_state.worker_registry.register_run_slot("run-snap", 1);
    let probe_id = server_state.queue_probe("run-snap".into(), "text".into(), false);
    let claimed = server_state
        .try_reserve_probe_for_delivery("run-snap", Some("/tmp/before.jsonl".to_owned()), 10)
        .expect("claim succeeds");
    assert_eq!(claimed.probe_id, probe_id);

    server_state.update_in_flight_probe_snapshot("run-snap", Some("/tmp/after.jsonl".to_owned()), 4096);
    assert!(server_state.has_in_flight_probe("run-snap"));
    assert_eq!(
        server_state.in_flight_probe_id("run-snap").as_deref(),
        Some(probe_id.as_str())
    );

    let taken = server_state.take_in_flight_probe("run-snap").expect("entry present");
    assert_eq!(taken.transcript_path.as_deref(), Some("/tmp/after.jsonl"));
    assert_eq!(
        taken.offset_bytes, 4096,
        "the reply must be read from after the cancelled turn's own output, not before it",
    );
}

/// A no-op on a run holding no claim, rather than inventing one.
#[test]
fn updating_the_snapshot_for_a_run_with_no_claim_does_nothing() {
    let (server_state, _dir) = test_server_state();
    server_state.update_in_flight_probe_snapshot("run-none", Some("/tmp/x.jsonl".to_owned()), 7);
    assert!(!server_state.has_in_flight_probe("run-none"));
    assert_eq!(server_state.in_flight_probe_id("run-none"), None);
}
