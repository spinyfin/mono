use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};

use boss_tmux::{CommandOutput, CommandRunner};

use super::*;

/// Scripted tmux server. `clients` is the `list-clients` reply body, replaced
/// as the test advances; every `detach-client` is recorded so a test can
/// assert *which* tty was detached (and that nothing else was ever run).
#[derive(Default)]
struct FakeServer {
    clients: Mutex<String>,
    detach_result: Mutex<Option<CommandOutput>>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl FakeServer {
    fn with_clients(rows: &str) -> Arc<Self> {
        Arc::new(Self {
            clients: Mutex::new(rows.to_owned()),
            ..Default::default()
        })
    }

    fn set_clients(&self, rows: &str) {
        *self.clients.lock().unwrap() = rows.to_owned();
    }

    fn detaches(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.get(2).map(String::as_str) == Some("detach-client"))
            .filter_map(|call| call.get(4).cloned())
            .collect()
    }
}

#[async_trait::async_trait]
impl CommandRunner for FakeServer {
    async fn run(&self, _: &Path, args: &[OsString], _: Option<&Path>) -> std::io::Result<CommandOutput> {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
        let verb = args.get(2).cloned().unwrap_or_default();
        self.calls.lock().unwrap().push(args);
        Ok(match verb.as_str() {
            "list-clients" => CommandOutput {
                success: true,
                code: Some(0),
                stdout: self.clients.lock().unwrap().clone(),
                stderr: String::new(),
            },
            "detach-client" => self.detach_result.lock().unwrap().clone().unwrap_or(CommandOutput {
                success: true,
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            }),
            other => panic!("unexpected tmux verb: {other:?}"),
        })
    }
}

fn fixture(server: &Arc<FakeServer>) -> Tmux {
    Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", server.clone(), boss_tmux::TEST_SOCKET_PATH).unwrap()
}

fn client(tty: &str, pid: i32, activity_epoch: i64) -> TmuxClient {
    TmuxClient {
        tty: tty.to_owned(),
        pid,
        activity_epoch,
    }
}

const SESSION: &str = "boss-coordinator";
const NOW: i64 = 1_787_149_500;

// --- the detection rule ----------------------------------------------------

/// The false positive this whole design exists to avoid. A coordinator
/// streaming output with nobody typing has a frozen `client_activity` — the
/// exact server-side picture as the wedge. What separates them is that a
/// healthy client's last observed input is never *behind* what the app says
/// it delivered.
#[test]
fn a_healthy_client_whose_operator_typed_a_while_ago_is_delivered() {
    let report = InputReport {
        client_pid: 13371,
        last_input_epoch: NOW - 120,
    };
    // Frozen at the same instant the operator last typed, two minutes of
    // pane output since. Nothing is wrong here.
    let clients = [client("/dev/ttys002", 13371, NOW - 120)];
    assert_eq!(classify(report, &clients, NOW), Verdict::Delivered);
}

#[test]
fn input_the_server_never_saw_is_undelivered() {
    let report = InputReport {
        client_pid: 13371,
        last_input_epoch: NOW - 5,
    };
    let clients = [client("/dev/ttys002", 13371, NOW - 400)];
    assert_eq!(
        classify(report, &clients, NOW),
        Verdict::Undelivered {
            tty: "/dev/ttys002".to_owned(),
            client_pid: 13371,
            activity_epoch: NOW - 400,
        }
    );
}

#[test]
fn a_second_of_clock_skew_between_the_app_and_tmux_is_not_a_wedge() {
    let report = InputReport {
        client_pid: 13371,
        last_input_epoch: NOW,
    };
    let clients = [client("/dev/ttys002", 13371, NOW - ACTIVITY_GRACE_SECS)];
    assert_eq!(classify(report, &clients, NOW), Verdict::Delivered);
}

/// A report whose pid no longer names an attached client describes a viewer
/// the app is already rebuilding. There is nothing to detach — and detaching
/// on a pid miss would evict whichever client happened to be there.
#[test]
fn a_report_for_a_departed_client_is_indeterminate() {
    let report = InputReport {
        client_pid: 13371,
        last_input_epoch: NOW - 5,
    };
    let clients = [client("/dev/ttys007", 999, NOW - 400)];
    assert_eq!(classify(report, &clients, NOW), Verdict::ClientGone);
    assert_eq!(classify(report, &[], NOW), Verdict::ClientGone);
}

#[test]
fn an_ancient_report_stops_arming_recovery() {
    let report = InputReport {
        client_pid: 13371,
        last_input_epoch: NOW - STALE_REPORT_SECS - 1,
    };
    let clients = [client("/dev/ttys002", 13371, NOW - 10_000)];
    assert_eq!(classify(report, &clients, NOW), Verdict::Stale);
}

/// A stale report is also dropped, so a pane nobody has touched in minutes
/// stops costing a `list-clients` every tick.
#[tokio::test]
async fn a_stale_report_is_dropped_so_the_session_stops_being_sampled() {
    let server = FakeServer::with_clients(&format!("/dev/ttys002\t13371\t{}\n", NOW - 10_000));
    let tmux = fixture(&server);
    let reports = PaneInputReports::default();
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - STALE_REPORT_SECS - 1,
        },
    );
    let mut watch = InputWatch::default();
    assert!(watch.tick(&tmux, &reports, Instant::now(), NOW).await.is_empty());
    assert!(reports.snapshot().is_empty());
    assert!(server.detaches().is_empty());
}

// --- the loop --------------------------------------------------------------

#[tokio::test]
async fn recovery_needs_a_sustained_streak_and_detaches_only_the_apps_own_tty() {
    // The app's client (pid 13371) is wedged; an operator terminal
    // (pid 40272) is attached to the same session and typing normally.
    let server = FakeServer::with_clients(&format!(
        "/dev/ttys002\t13371\t{}\n/dev/ttys007\t40272\t{}\n",
        NOW - 400,
        NOW - 1
    ));
    let tmux = fixture(&server);
    let reports = PaneInputReports::default();
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 5,
        },
    );
    let mut watch = InputWatch::default();
    let start = Instant::now();

    // A second unacknowledged attempt is required before the streak can
    // confirm: one key may have been a libghostty-local binding.
    assert!(watch.tick(&tmux, &reports, start, NOW).await.is_empty());
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 1,
        },
    );
    for pass in 2..CONFIRM_TICKS {
        let outcomes = watch.tick(&tmux, &reports, start, NOW).await;
        assert!(outcomes.is_empty(), "recovered on pass {pass}");
        assert!(server.detaches().is_empty());
    }

    let outcomes = watch.tick(&tmux, &reports, start, NOW).await;
    assert_eq!(
        outcomes,
        vec![WatchOutcome::Recovered {
            session: SESSION.to_owned(),
            tty: "/dev/ttys002".to_owned(),
            client_pid: 13371,
            recovery_count: 1,
        }]
    );
    // Only the app's tty — the operator's terminal is never evicted.
    assert_eq!(server.detaches(), vec!["/dev/ttys002".to_owned()]);
    // The outgoing client's report must not be judged against its
    // replacement, which will report its own pid.
    assert!(reports.snapshot().is_empty());
}

/// One unacknowledged attempt may be a local terminal binding rather than a
/// pty write, so it cannot detach an otherwise healthy client by itself.
#[tokio::test]
async fn one_unacknowledged_attempt_never_recovers_no_matter_how_long_it_runs() {
    let server = FakeServer::with_clients(&format!("/dev/ttys002\t13371\t{}\n", NOW - 900));
    let tmux = fixture(&server);
    let reports = PaneInputReports::default();
    // tmux never sees this key, but the app does not report another attempt.
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 5,
        },
    );
    let mut watch = InputWatch::default();
    let start = Instant::now();
    for tick in 0..20 {
        let outcomes = watch
            .tick(&tmux, &reports, start + TICK * tick, NOW + i64::from(tick) * 5)
            .await;
        assert!(outcomes.is_empty(), "idle session recovered on tick {tick}");
    }
    assert!(server.detaches().is_empty());
}

#[tokio::test]
async fn one_delivered_pass_clears_the_streak() {
    let server = FakeServer::with_clients(&format!("/dev/ttys002\t13371\t{}\n", NOW - 400));
    let tmux = fixture(&server);
    let reports = PaneInputReports::default();
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 5,
        },
    );
    let mut watch = InputWatch::default();
    let start = Instant::now();
    watch.tick(&tmux, &reports, start, NOW).await;
    watch.tick(&tmux, &reports, start, NOW).await;

    // The keystroke lands late; the server catches up.
    server.set_clients(&format!("/dev/ttys002\t13371\t{}\n", NOW - 4));
    assert!(watch.tick(&tmux, &reports, start, NOW).await.is_empty());
    // Streak restarted from zero, so the next two passes still recover
    // nothing even though the client goes back to looking wedged.
    server.set_clients(&format!("/dev/ttys002\t13371\t{}\n", NOW - 400));
    assert!(watch.tick(&tmux, &reports, start, NOW).await.is_empty());
    assert!(watch.tick(&tmux, &reports, start, NOW).await.is_empty());
    assert!(server.detaches().is_empty());
}

#[tokio::test]
async fn a_tmux_read_failure_is_not_treated_as_a_wedge() {
    #[derive(Default)]
    struct Broken;
    #[async_trait::async_trait]
    impl CommandRunner for Broken {
        async fn run(&self, _: &Path, _: &[OsString], _: Option<&Path>) -> std::io::Result<CommandOutput> {
            Ok(CommandOutput {
                success: false,
                code: Some(1),
                stdout: String::new(),
                stderr: "server exited unexpectedly".to_owned(),
            })
        }
    }
    let tmux =
        Tmux::with_runner_and_socket("/opt/homebrew/bin/tmux", Arc::new(Broken), boss_tmux::TEST_SOCKET_PATH).unwrap();
    let reports = PaneInputReports::default();
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 5,
        },
    );
    let mut watch = InputWatch::default();
    let start = Instant::now();
    for _ in 0..(CONFIRM_TICKS + 2) {
        assert!(watch.tick(&tmux, &reports, start, NOW).await.is_empty());
    }
}

#[tokio::test]
async fn repeated_wedges_latch_and_escalate_once_instead_of_churning() {
    let server = FakeServer::with_clients(&format!(
        "/dev/ttys002\t13371\t{}\n/dev/ttys007\t40272\t{}\n",
        NOW - 400,
        NOW
    ));
    let tmux = fixture(&server);
    let reports = PaneInputReports::default();
    let mut watch = InputWatch::default();
    let start = Instant::now();

    /// Drive one full wedge: re-arm the report, then run enough passes to
    /// cross the confirmation streak. Returns everything the round produced
    /// — a round that recovers on its first pass then spends the rest inside
    /// the settle window, so only the accumulation sees the outcome.
    async fn wedge(watch: &mut InputWatch, tmux: &Tmux, reports: &PaneInputReports, at: Instant) -> Vec<WatchOutcome> {
        reports.record(
            SESSION,
            InputReport {
                client_pid: 13371,
                last_input_epoch: NOW - 5,
            },
        );
        let mut all = Vec::new();
        all.extend(watch.tick(tmux, reports, at, NOW).await);
        reports.record(
            SESSION,
            InputReport {
                client_pid: 13371,
                last_input_epoch: NOW - 1,
            },
        );
        for _ in 1..CONFIRM_TICKS {
            all.extend(watch.tick(tmux, reports, at, NOW).await);
        }
        all
    }

    // SETTLE is honoured between wedges, so each round is advanced past it.
    let mut at = start;
    for expected in 1..=MAX_RECOVERIES {
        let outcomes = wedge(&mut watch, &tmux, &reports, at).await;
        assert!(
            matches!(outcomes.as_slice(), [WatchOutcome::Recovered { recovery_count, .. }] if *recovery_count == expected),
            "round {expected}: {outcomes:?}"
        );
        at += SETTLE + TICK;
        // Keep a second pane reported through the outgoing viewer's report
        // gap. This makes tick run after `reports.forget(SESSION)`, proving
        // the recovery ledger survives independently of report presence.
        reports.record(
            "boss-worker",
            InputReport {
                client_pid: 40272,
                last_input_epoch: NOW,
            },
        );
        assert!(watch.tick(&tmux, &reports, at, NOW).await.is_empty());
    }

    let outcomes = wedge(&mut watch, &tmux, &reports, at).await;
    assert_eq!(
        outcomes,
        vec![WatchOutcome::Escalated {
            session: SESSION.to_owned(),
            client_pid: 13371,
        }]
    );
    assert_eq!(server.detaches().len(), MAX_RECOVERIES as usize);

    // Latched: further passes stay silent rather than re-raising.
    at += SETTLE + TICK;
    assert!(wedge(&mut watch, &tmux, &reports, at).await.is_empty());
    assert_eq!(server.detaches().len(), MAX_RECOVERIES as usize);

    // Once the window clears, recovery is available again.
    at += RECOVERY_WINDOW + TICK;
    let outcomes = wedge(&mut watch, &tmux, &reports, at).await;
    assert!(
        matches!(outcomes.as_slice(), [WatchOutcome::Recovered { recovery_count: 1, .. }]),
        "{outcomes:?}"
    );
}

#[tokio::test]
async fn the_settle_period_spares_the_replacement_viewer() {
    let server = FakeServer::with_clients(&format!("/dev/ttys002\t13371\t{}\n", NOW - 400));
    let tmux = fixture(&server);
    let reports = PaneInputReports::default();
    let mut watch = InputWatch::default();
    let start = Instant::now();
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 5,
        },
    );
    watch.tick(&tmux, &reports, start, NOW).await;
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 1,
        },
    );
    for _ in 1..CONFIRM_TICKS {
        watch.tick(&tmux, &reports, start, NOW).await;
    }
    assert_eq!(server.detaches().len(), 1);

    // A fresh viewer comes up immediately and looks wedged on paper.
    server.set_clients(&format!("/dev/ttys002\t99999\t{}\n", NOW - 400));
    reports.record(
        SESSION,
        InputReport {
            client_pid: 99999,
            last_input_epoch: NOW - 1,
        },
    );
    for tick in 1..=4_u32 {
        assert!(watch.tick(&tmux, &reports, start + TICK * tick, NOW).await.is_empty());
    }
    assert_eq!(server.detaches().len(), 1, "detached during the settle period");
}

#[tokio::test]
async fn a_client_that_left_before_the_detach_is_benign() {
    let server = FakeServer::with_clients(&format!("/dev/ttys002\t13371\t{}\n", NOW - 400));
    *server.detach_result.lock().unwrap() = Some(CommandOutput {
        success: false,
        code: Some(1),
        stdout: String::new(),
        stderr: "can't find client: /dev/ttys002".to_owned(),
    });
    let tmux = fixture(&server);
    let reports = PaneInputReports::default();
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 5,
        },
    );
    let mut watch = InputWatch::default();
    let start = Instant::now();
    let mut outcomes = Vec::new();
    assert!(watch.tick(&tmux, &reports, start, NOW).await.is_empty());
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 1,
        },
    );
    for _ in 1..CONFIRM_TICKS {
        outcomes = watch.tick(&tmux, &reports, start, NOW).await;
    }
    assert_eq!(
        outcomes,
        vec![WatchOutcome::AlreadyGone {
            session: SESSION.to_owned(),
            tty: "/dev/ttys002".to_owned(),
        }]
    );
}

#[tokio::test]
async fn repeated_detach_failures_escalate_instead_of_retrying_forever() {
    let server = FakeServer::with_clients(&format!("/dev/ttys002\t13371\t{}\n", NOW - 400));
    *server.detach_result.lock().unwrap() = Some(CommandOutput {
        success: false,
        code: Some(1),
        stdout: String::new(),
        stderr: "permission denied".to_owned(),
    });
    let tmux = fixture(&server);
    let reports = PaneInputReports::default();
    let mut watch = InputWatch::default();
    let start = Instant::now();

    for attempt in 1..=MAX_RECOVERIES {
        let at = start + (SETTLE + TICK) * (attempt - 1);
        reports.record(
            SESSION,
            InputReport {
                client_pid: 13371,
                last_input_epoch: NOW - 5,
            },
        );
        assert!(watch.tick(&tmux, &reports, at, NOW).await.is_empty());
        reports.record(
            SESSION,
            InputReport {
                client_pid: 13371,
                last_input_epoch: NOW - 1,
            },
        );
        let mut outcomes = Vec::new();
        for _ in 1..CONFIRM_TICKS {
            outcomes = watch.tick(&tmux, &reports, at, NOW).await;
        }
        if attempt < MAX_RECOVERIES {
            assert!(matches!(outcomes.as_slice(), [WatchOutcome::Failed { .. }]));
        } else {
            assert!(matches!(
                outcomes.as_slice(),
                [WatchOutcome::Failed { .. }, WatchOutcome::Escalated { .. }]
            ));
        }
    }
    assert_eq!(server.detaches().len(), MAX_RECOVERIES as usize);
}

#[tokio::test]
async fn dropping_a_report_discards_that_sessions_streak() {
    let server = FakeServer::with_clients(&format!("/dev/ttys002\t13371\t{}\n", NOW - 400));
    let tmux = fixture(&server);
    let reports = PaneInputReports::default();
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 5,
        },
    );
    let mut watch = InputWatch::default();
    let start = Instant::now();
    for _ in 1..CONFIRM_TICKS {
        watch.tick(&tmux, &reports, start, NOW).await;
    }

    // The app disconnects; its reports go with it.
    reports.clear();
    assert!(watch.tick(&tmux, &reports, start, NOW).await.is_empty());

    // On reconnect the streak starts over rather than firing immediately.
    reports.record(
        SESSION,
        InputReport {
            client_pid: 13371,
            last_input_epoch: NOW - 5,
        },
    );
    assert!(watch.tick(&tmux, &reports, start, NOW).await.is_empty());
    assert!(server.detaches().is_empty());
}
