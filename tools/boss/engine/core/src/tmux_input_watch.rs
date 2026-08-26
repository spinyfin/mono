//! Detects and recovers a tmux viewer that renders output but accepts no
//! input, and makes each recovery visible to the operator afterwards.
//!
//! # The signal, and why the obvious one does not work
//!
//! A Boss pane is a libghostty surface whose pty runs `tmux attach-session`.
//! The observed failure is one-directional: the surface keeps rendering
//! everything the server sends, but nothing the operator types reaches the
//! session. On the tmux server the wedged client's `#{client_activity}` is
//! frozen while its pane keeps producing output.
//!
//! That pairing is *not*, on its own, evidence of anything. Measured against
//! tmux 3.6a (`tools/boss/docs/tmux-client-input-wedge.md`):
//!
//! - `#{client_activity}` advances **only when that client sends the server
//!   something** — i.e. on input. Twelve seconds of pane output moved it by
//!   zero; one byte written into the client's pty moved it immediately.
//! - `#{window_activity}` advances on **pane output**, with or without any
//!   client attached at all.
//!
//! So "frozen client_activity while output flows" describes a perfectly
//! healthy client whose operator simply is not typing — which is the
//! coordinator's normal state, since it emits output for minutes unattended.
//! The server cannot distinguish "no input was sent" from "input was sent
//! and lost", because both look identical from its side.
//!
//! The missing half only exists in the app: whether input was *attempted*.
//! So detection is a correlation across the two sides —
//! [`boss_protocol::FrontendRequest::ReportPaneClientInput`] carries the
//! app's "I delivered a keystroke into this pane at T", and a wedge is
//! `T` newer than the server's record of that client's last input, sustained
//! across several passes. An idle session cannot trip it: with nothing typed
//! there is no report, and with a report the app's stamp is never ahead of
//! what the server saw.
//!
//! # Recovery
//!
//! `detach-client -t <tty>` on the app's own client, and nothing else. The
//! client process exits 0, the app observes its child exit and rebuilds a
//! fresh surface plus a fresh client, and the session, its pane and the
//! process inside it are untouched — this is the manual fix that was
//! confirmed to work on the live wedge, reproduced under test. It is
//! addressed by tty, and only for the tty whose `#{client_pid}` matches the
//! pid the app reported, so an operator's terminal attached to the same
//! session is never evicted.
//!
//! Recovery is bounded ([`MAX_RECOVERIES`] per [`RECOVERY_WINDOW`]). Past
//! that the watch latches and escalates instead of detaching again: a viewer
//! that re-wedges immediately is a defect to look at, not one to keep
//! papering over.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use boss_tmux::{Tmux, TmuxClient};

/// How often the watch samples the tmux server. Only sessions the app has
/// actually reported input for are sampled, so an idle Boss costs nothing.
pub(crate) const TICK: Duration = Duration::from_secs(5);

/// Slack between the app's epoch-second stamp and tmux's. Both come from the
/// same wall clock on the same host, and a keystroke reaches the server in
/// milliseconds, so this only has to cover second-boundary rounding and the
/// app's one-per-second report coalescing.
const ACTIVITY_GRACE_SECS: i64 = 3;

/// Consecutive undelivered verdicts before recovery. At [`TICK`] this is
/// ~15s of "keys went in, the server saw nothing" — long enough that no
/// scheduling hiccup or clock rounding can manufacture it, short enough that
/// an operator is still sitting there when the pane comes back.
const CONFIRM_TICKS: u32 = 3;

/// A report older than this is no longer judged. The wedge confirms within
/// seconds, so anything this stale is a viewer nobody has touched in a long
/// time — keeping it would let one ancient un-acked keystroke arm recovery
/// indefinitely.
const STALE_REPORT_SECS: i64 = 300;

/// Recoveries allowed inside [`RECOVERY_WINDOW`] before the watch latches.
pub(crate) const MAX_RECOVERIES: u32 = 3;
const RECOVERY_WINDOW: Duration = Duration::from_secs(600);

/// Quiet period after a detach. The app has to notice its child exit, back
/// off, and build a new surface and client; judging the replacement against
/// the outgoing client's report would confirm a wedge that no longer exists.
const SETTLE: Duration = Duration::from_secs(30);

/// The app's latest observation for one tmux-hosted pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputReport {
    /// Pid of the `tmux attach-session` process backing the app's viewer.
    pub(crate) client_pid: i32,
    /// Unix seconds of the most recent keystroke the app delivered into it.
    pub(crate) last_input_epoch: i64,
}

/// App-reported input observations, keyed by tmux session name.
///
/// Held for the lifetime of an app session and cleared with it: a report
/// from a departed app describes a client that no longer exists, and acting
/// on one would detach whatever inherited its session name.
#[derive(Debug, Default)]
pub(crate) struct PaneInputReports(StdMutex<HashMap<String, InputReport>>);

impl PaneInputReports {
    pub(crate) fn record(&self, session: &str, report: InputReport) {
        self.lock().insert(session.to_owned(), report);
    }

    pub(crate) fn snapshot(&self) -> Vec<(String, InputReport)> {
        self.lock()
            .iter()
            .map(|(session, report)| (session.clone(), *report))
            .collect()
    }

    pub(crate) fn forget(&self, session: &str) {
        self.lock().remove(session);
    }

    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, InputReport>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// What one report says about one session's client, given the server's view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The server recorded input from that client at or after the app's
    /// stamp. Whatever the operator typed arrived.
    Delivered,
    /// The app delivered input the server never saw from that client.
    Undelivered {
        tty: String,
        client_pid: i32,
        activity_epoch: i64,
    },
    /// Nobody has typed into this pane in a long time. Not a wedge, and no
    /// longer worth sampling for — the watch drops the report, which is what
    /// keeps an idle Boss from making any tmux calls at all.
    Stale,
    /// No attached client answers to the reported pid: the viewer is being
    /// rebuilt, or its surface was torn down. There is nothing to detach,
    /// and detaching on a pid miss would evict whichever client is there.
    ClientGone,
}

/// Correlate one app report against the server's client list.
///
/// Pure: the whole detection rule lives here so it can be exercised across
/// the idle, healthy, wedged and rebuilt cases without a tmux server.
pub(crate) fn classify(report: InputReport, clients: &[TmuxClient], now_epoch: i64) -> Verdict {
    if now_epoch.saturating_sub(report.last_input_epoch) > STALE_REPORT_SECS {
        return Verdict::Stale;
    }
    let Some(client) = clients.iter().find(|client| client.pid == report.client_pid) else {
        return Verdict::ClientGone;
    };
    if client.activity_epoch.saturating_add(ACTIVITY_GRACE_SECS) >= report.last_input_epoch {
        return Verdict::Delivered;
    }
    Verdict::Undelivered {
        tty: client.tty.clone(),
        client_pid: client.pid,
        activity_epoch: client.activity_epoch,
    }
}

/// What a [`InputWatch::tick`] pass did, for the caller to log and surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchOutcome {
    /// The app's client was detached after a confirmed wedge. The app
    /// rebuilds its viewer; the session and pane are untouched.
    Recovered {
        session: String,
        tty: String,
        client_pid: i32,
        /// How many recoveries this session has needed inside the current
        /// window, this one included. A rising count is the thing an
        /// operator most needs to see: it means the underlying defect is
        /// still there and self-healing is carrying it.
        recovery_count: u32,
    },
    /// The client left between the listing and the detach. Benign.
    AlreadyGone { session: String, tty: String },
    /// Recovery budget exhausted; no further detach until the window
    /// expires. Emitted once per latch, not once per tick.
    Escalated { session: String, client_pid: i32 },
    /// The detach itself failed.
    Failed {
        session: String,
        tty: String,
        error: String,
    },
}

#[derive(Debug, Default, bon::Builder)]
#[builder(on(String, into))]
struct SessionWatch {
    consecutive_undelivered: u32,
    /// The first unacknowledged input attempt and tmux activity that armed
    /// this streak. A second attempt while activity remains frozen is needed
    /// before a report can confirm a wedge: libghostty may consume one key
    /// for a local binding without writing it to the pty.
    armed_epoch: Option<i64>,
    armed_activity_epoch: Option<i64>,
    repeated_attempt_observed: bool,
    recoveries: VecDeque<Instant>,
    settle_until: Option<Instant>,
    escalated: bool,
}

impl SessionWatch {
    fn reset_streak(&mut self) {
        self.consecutive_undelivered = 0;
        self.armed_epoch = None;
        self.armed_activity_epoch = None;
        self.repeated_attempt_observed = false;
    }

    /// Drop recoveries that have aged out, then report how many remain.
    fn recoveries_in_window(&mut self, now: Instant) -> u32 {
        while self
            .recoveries
            .front()
            .is_some_and(|at| now.duration_since(*at) > RECOVERY_WINDOW)
        {
            self.recoveries.pop_front();
        }
        if self.recoveries.is_empty() {
            self.escalated = false;
        }
        u32::try_from(self.recoveries.len()).unwrap_or(u32::MAX)
    }
}

/// Per-session wedge state across ticks.
#[derive(Debug, Default)]
pub(crate) struct InputWatch {
    sessions: HashMap<String, SessionWatch>,
}

impl InputWatch {
    /// Sample every session the app has reported input for, and recover the
    /// ones whose viewer has stopped delivering it.
    ///
    /// `now` / `now_epoch` are passed in rather than read here so tests can
    /// drive the streak, the recovery window and the settle period without
    /// sleeping.
    pub(crate) async fn tick(
        &mut self,
        tmux: &Tmux,
        reports: &PaneInputReports,
        now: Instant,
        now_epoch: i64,
    ) -> Vec<WatchOutcome> {
        let mut outcomes = Vec::new();
        let reported = reports.snapshot();
        // A consumed report must clear its streak, but it must not discard a
        // recovery ledger or settle window. Recovery deliberately forgets the
        // outgoing report, and another reporting pane can keep this loop
        // alive during that gap.
        for (session, watch) in &mut self.sessions {
            if !reported.iter().any(|(name, _)| name == session) {
                watch.reset_streak();
            }
            watch.recoveries_in_window(now);
        }
        self.sessions.retain(|_, watch| {
            !watch.recoveries.is_empty()
                || watch.settle_until.is_some_and(|until| now < until)
                || watch.consecutive_undelivered > 0
        });

        for (session, report) in reported {
            let watch = self.sessions.entry(session.clone()).or_default();
            if watch.settle_until.is_some_and(|until| now < until) {
                continue;
            }
            watch.settle_until = None;

            let clients = match tmux.list_clients(&session).await {
                Ok(clients) => clients,
                Err(error) => {
                    // A tmux read failure is not evidence of a wedge, and
                    // treating it as one would detach a healthy client every
                    // time the server hiccuped.
                    tracing::debug!(
                        %session,
                        error = %format!("{error:#}"),
                        "tmux input watch: could not list clients"
                    );
                    watch.reset_streak();
                    continue;
                }
            };

            let (tty, client_pid, activity_epoch) = match classify(report, &clients, now_epoch) {
                Verdict::Delivered => {
                    watch.reset_streak();
                    continue;
                }
                Verdict::Stale => {
                    // Drop it so this session stops being sampled until
                    // somebody types into it again.
                    reports.forget(&session);
                    watch.reset_streak();
                    continue;
                }
                Verdict::ClientGone => {
                    // Keep the report: the replacement viewer overwrites it
                    // with its own pid as soon as it takes a keystroke.
                    watch.reset_streak();
                    continue;
                }
                Verdict::Undelivered {
                    tty,
                    client_pid,
                    activity_epoch,
                } => (tty, client_pid, activity_epoch),
            };

            if watch.consecutive_undelivered == 0 {
                watch.consecutive_undelivered = 1;
                watch.armed_epoch = Some(report.last_input_epoch);
                watch.armed_activity_epoch = Some(activity_epoch);
            } else if Some(activity_epoch) != watch.armed_activity_epoch {
                // tmux observed intervening input, so this is a new attempt
                // rather than the same frozen client state.
                watch.reset_streak();
                watch.consecutive_undelivered = 1;
                watch.armed_epoch = Some(report.last_input_epoch);
                watch.armed_activity_epoch = Some(activity_epoch);
            } else if !watch.repeated_attempt_observed {
                if report.last_input_epoch > watch.armed_epoch.unwrap_or(report.last_input_epoch)
                    && Some(activity_epoch) == watch.armed_activity_epoch
                {
                    watch.repeated_attempt_observed = true;
                    watch.consecutive_undelivered += 1;
                }
            } else {
                watch.consecutive_undelivered += 1;
            }
            if watch.consecutive_undelivered < CONFIRM_TICKS {
                continue;
            }

            if watch.recoveries_in_window(now) >= MAX_RECOVERIES {
                if !watch.escalated {
                    watch.escalated = true;
                    outcomes.push(WatchOutcome::Escalated {
                        session: session.clone(),
                        client_pid,
                    });
                }
                continue;
            }

            match tmux.detach_client(&tty).await {
                Ok(true) => {
                    watch.recoveries.push_back(now);
                    let recovery_count = u32::try_from(watch.recoveries.len()).unwrap_or(u32::MAX);
                    outcomes.push(WatchOutcome::Recovered {
                        session: session.clone(),
                        tty,
                        client_pid,
                        recovery_count,
                    });
                }
                Ok(false) => outcomes.push(WatchOutcome::AlreadyGone {
                    session: session.clone(),
                    tty,
                }),
                Err(error) => outcomes.push(WatchOutcome::Failed {
                    session: session.clone(),
                    tty,
                    error: format!("{error:#}"),
                }),
            }
            if matches!(outcomes.last(), Some(WatchOutcome::Failed { .. })) {
                watch.recoveries.push_back(now);
                if watch.recoveries_in_window(now) >= MAX_RECOVERIES && !watch.escalated {
                    watch.escalated = true;
                    outcomes.push(WatchOutcome::Escalated {
                        session: session.clone(),
                        client_pid,
                    });
                }
            }
            watch.reset_streak();
            watch.settle_until = Some(now + SETTLE);
            // The replacement viewer reports its own pid; keeping the
            // outgoing client's report would judge the new client against
            // the old one's stamp.
            reports.forget(&session);
        }
        outcomes
    }
}

#[cfg(test)]
mod tests;
