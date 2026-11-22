//! Where one detection pass spent its time, and the health signal an
//! abandoned pass raises.
//!
//! A pass is one future on the poller's task. When it exceeds its budget the
//! loop drops it, and everything the pass knew about its own progress goes
//! with it. [`PassTrace`] lives outside the pass future, so the loop can
//! still read which phase was in flight, how far through its items it was,
//! and what every earlier phase cost.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use boss_protocol::EngineHealthIssue;
use tokio::time::Instant;

use crate::metrics::{GaugeHandle, Registry};

/// The phases of one full detection pass, in the order the pass runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PassPhase {
    /// The candidate-list queries (local DB).
    ListCandidates,
    /// Turning off GitHub auto-merge for merges conflict resolution stopped.
    MergeDisarm,
    /// The batched lifecycle probe (GitHub), plus its sequence allocation.
    Probe,
    /// Candidates whose probe reports a merged or closed PR.
    TerminalWalk,
    /// Upkeep of candidates whose PR is still open.
    OpenWalk,
    /// `pending` CI-remediation attempts with no live execution.
    StrandedCi,
    /// `blocked` parents with no blocking signal left.
    StrandedBlocked,
    /// The batched merge-queue dequeue query and its per-candidate checks.
    QueueRebounce,
    /// Tasks held for a reviewer pass that finished or timed out.
    StalledReviewer,
    /// Deferred pre-merge review admission.
    ReviewAdmission,
    /// Review-batch liveness, reaping and the pool gauges.
    ReviewBatchUpkeep,
    /// Per-execution PR-detection recheck for live workers with no PR.
    PrRecheck,
    /// Per-execution recheck of nudges held for background work.
    BackgroundNudges,
    /// Per-execution PR detection for recently terminal executions.
    LatePr,
}

impl PassPhase {
    /// Every phase a pass can run. Terminal merge observation always
    /// precedes the rest; open walk, local upkeep and the per-execution
    /// families rotate their starting order across passes.
    pub const ALL: [PassPhase; 14] = [
        PassPhase::ListCandidates,
        PassPhase::MergeDisarm,
        PassPhase::Probe,
        PassPhase::TerminalWalk,
        PassPhase::OpenWalk,
        PassPhase::StrandedCi,
        PassPhase::StrandedBlocked,
        PassPhase::QueueRebounce,
        PassPhase::StalledReviewer,
        PassPhase::ReviewAdmission,
        PassPhase::ReviewBatchUpkeep,
        PassPhase::PrRecheck,
        PassPhase::BackgroundNudges,
        PassPhase::LatePr,
    ];

    /// Stable snake_case name, used in log fields and metric names.
    pub fn as_str(self) -> &'static str {
        match self {
            PassPhase::ListCandidates => "list_candidates",
            PassPhase::MergeDisarm => "merge_disarm",
            PassPhase::Probe => "probe",
            PassPhase::TerminalWalk => "terminal_walk",
            PassPhase::OpenWalk => "open_walk",
            PassPhase::StrandedCi => "stranded_ci",
            PassPhase::StrandedBlocked => "stranded_blocked",
            PassPhase::QueueRebounce => "queue_rebounce",
            PassPhase::StalledReviewer => "stalled_reviewer",
            PassPhase::ReviewAdmission => "review_admission",
            PassPhase::ReviewBatchUpkeep => "review_batch_upkeep",
            PassPhase::PrRecheck => "pr_recheck",
            PassPhase::BackgroundNudges => "background_nudges",
            PassPhase::LatePr => "late_pr",
        }
    }

    fn gauge(self) -> &'static GaugeHandle {
        &PHASE_GAUGES[PassPhase::ALL
            .iter()
            .position(|phase| *phase == self)
            .expect("every phase is listed in PassPhase::ALL")]
    }
}

const PHASE_GAUGE_HELP: &str = "Wall-clock milliseconds the most recent full detection pass spent in this phase.";

/// One gauge per phase, indexed like [`PassPhase::ALL`].
static PHASE_GAUGES: [GaugeHandle; 14] = [
    GaugeHandle::new("merge_poller.phase_ms.list_candidates", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.merge_disarm", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.probe", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.terminal_walk", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.open_walk", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.stranded_ci", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.stranded_blocked", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.queue_rebounce", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.stalled_reviewer", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.review_admission", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.review_batch_upkeep", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.pr_recheck", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.background_nudges", PHASE_GAUGE_HELP),
    GaugeHandle::new("merge_poller.phase_ms.late_pr", PHASE_GAUGE_HELP),
];

crate::register_gauge!(
    PASS_DURATION_MS,
    "merge_poller.pass_duration_ms",
    "Wall-clock milliseconds of the most recent full detection pass (its budget, when the pass was abandoned)."
);
crate::register_gauge!(
    PASS_ITEMS,
    "merge_poller.items_per_pass",
    "Items the most recent full detection pass finished, summed over its phases."
);
crate::register_gauge!(
    PASS_ITEMS_UNFINISHED,
    "merge_poller.items_unfinished",
    "Items the most recent full detection pass listed but did not finish; they remain eligible for later passes."
);
crate::register_counter!(
    PASSES,
    "merge_poller.passes",
    "Full detection passes ended, completed or abandoned."
);

/// Register the pass-timing handles with `registry`.
pub(crate) fn register_pass_metrics(registry: &Registry) {
    registry.register_gauge(&PASS_DURATION_MS);
    registry.register_gauge(&PASS_ITEMS);
    registry.register_gauge(&PASS_ITEMS_UNFINISHED);
    registry.register_counter(&PASSES);
    for gauge in &PHASE_GAUGES {
        registry.register_gauge(gauge);
    }
}

/// What one phase cost and how far it got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseTiming {
    pub phase: PassPhase,
    pub elapsed: Duration,
    /// Items the phase set out to process. `0` for a phase that is one
    /// batched step rather than a walk.
    pub items_total: usize,
    /// Items the phase finished.
    pub items_done: usize,
    /// The item in flight when the timing was read — only ever set for a
    /// phase that was cut off.
    pub item: Option<String>,
}

impl PhaseTiming {
    fn render(&self, cut_off: bool) -> String {
        let mut out = format!("{}={}ms", self.phase.as_str(), self.elapsed.as_millis());
        if self.items_total > 0 || cut_off {
            out.push_str(&format!("({}/{})", self.items_done, self.items_total));
        }
        match (&self.item, cut_off) {
            (Some(item), true) => out.push_str(&format!("[cut off at {item}]")),
            (None, true) => out.push_str("[cut off]"),
            (_, false) => {}
        }
        out
    }
}

/// A read of [`PassTrace`]: every finished phase, plus the phase that was
/// still running if the pass was cut off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassTimings {
    /// Time since the pass began.
    pub total: Duration,
    /// Host sleep or a GitHub transport failure interrupted this pass.
    pub interrupted: bool,
    /// Finished phases, in the order they ran.
    pub phases: Vec<PhaseTiming>,
    /// The phase in flight. `None` once the pass has run to its end.
    pub in_flight: Option<PhaseTiming>,
    /// Cancellation-safe probe sub-work, overlapping the enclosing phase.
    pub probe_work: BTreeMap<String, ProbeWork>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeWork {
    pub elapsed: Duration,
    pub attempted: usize,
    pub completed: usize,
    pub deferred: usize,
}

impl PassTimings {
    /// One line naming each phase's cost, e.g.
    /// `list_candidates=14ms probe=910ms(1/1) open_walk=178000ms(12/46)[cut off at chore_x]`.
    pub fn breakdown(&self) -> String {
        self.phases
            .iter()
            .map(|phase| phase.render(false))
            .chain(self.in_flight.iter().map(|phase| phase.render(true)))
            .chain(self.probe_work.iter().map(|(kind, work)| {
                format!(
                    "{kind}={}ms(attempted={},completed={},deferred={})",
                    work.elapsed.as_millis(),
                    work.attempted,
                    work.completed,
                    work.deferred
                )
            }))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn all(&self) -> impl Iterator<Item = &PhaseTiming> {
        self.phases.iter().chain(self.in_flight.iter())
    }

    /// Items finished, summed over every phase.
    pub fn items_done(&self) -> usize {
        self.all().map(|phase| phase.items_done).sum()
    }

    /// Items listed but not finished, summed over every phase.
    pub fn items_unfinished(&self) -> usize {
        self.all()
            .map(|phase| phase.items_total.saturating_sub(phase.items_done))
            .sum()
    }

    /// The open walk listed more candidates than it finished: this pass
    /// yielded at the reserve or was dropped before covering every open PR.
    pub fn open_walk_truncated(&self) -> bool {
        self.all()
            .find(|phase| phase.phase == PassPhase::OpenWalk)
            .is_some_and(|phase| phase.items_done < phase.items_total)
    }

    /// Publish this pass's duration, item counts and per-phase cost. A phase
    /// the pass never reached reads `0`, so a stale value from an earlier
    /// pass is never mistaken for this one's.
    pub(crate) fn publish(&self, registry: &Registry, pass_duration: Duration) {
        PASS_DURATION_MS.set(registry, saturating_ms(pass_duration));
        PASS_ITEMS.set(registry, i64::try_from(self.items_done()).unwrap_or(i64::MAX));
        PASS_ITEMS_UNFINISHED.set(registry, i64::try_from(self.items_unfinished()).unwrap_or(i64::MAX));
        PASSES.inc(registry);
        for phase in PassPhase::ALL {
            let elapsed: Duration = self
                .all()
                .filter(|timing| timing.phase == phase)
                .map(|timing| timing.elapsed)
                .sum();
            phase.gauge().set(registry, saturating_ms(elapsed));
        }
    }
}

fn saturating_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[derive(Debug)]
struct OpenPhase {
    phase: PassPhase,
    started: Instant,
    items_total: usize,
    items_done: usize,
    item: Option<String>,
}

impl OpenPhase {
    fn timing(&self, now: Instant) -> PhaseTiming {
        PhaseTiming {
            phase: self.phase,
            elapsed: now.saturating_duration_since(self.started),
            items_total: self.items_total,
            items_done: self.items_done,
            item: self.item.clone(),
        }
    }
}

#[derive(Debug, Default)]
struct TraceState {
    interrupted: bool,
    started: Option<Instant>,
    finished: Vec<PhaseTiming>,
    open: Option<OpenPhase>,
    probe_work: BTreeMap<String, ProbeWork>,
}

impl TraceState {
    fn close_open(&mut self, now: Instant) {
        if let Some(open) = self.open.take() {
            let mut timing = open.timing(now);
            timing.item = None;
            self.finished.push(timing);
        }
    }
}

/// Phase timing for the pass currently running (or the one that just
/// ended). Shared by reference between the pass, which writes it, and the
/// loop, which reads it once the pass has returned or been dropped.
///
/// Timed on tokio's clock so it advances with a paused test runtime.
#[derive(Debug, Default)]
pub struct PassTrace {
    state: Mutex<TraceState>,
}

impl PassTrace {
    fn state(&self) -> MutexGuard<'_, TraceState> {
        // The state is plain bookkeeping with no invariant a panic could
        // break, so a poisoned lock is still safe to read and write.
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Forget the previous pass and start timing a new one.
    pub(crate) fn begin(&self) {
        *self.state() = TraceState {
            started: Some(Instant::now()),
            ..TraceState::default()
        };
    }

    /// End the phase in flight, if any, and start `phase` over `items_total`
    /// items.
    pub(crate) fn enter(&self, phase: PassPhase, items_total: usize) {
        let now = Instant::now();
        let mut state = self.state();
        state.close_open(now);
        state.open = Some(OpenPhase {
            phase,
            started: now,
            items_total,
            items_done: 0,
            item: None,
        });
    }

    /// Set the item count of the phase in flight, for a phase that only
    /// learns it after its own listing query.
    pub(crate) fn set_items_total(&self, items_total: usize) {
        if let Some(open) = self.state().open.as_mut() {
            open.items_total = items_total;
        }
    }

    /// Name the item the phase in flight is about to process.
    pub(crate) fn start_item(&self, item: &str) {
        if let Some(open) = self.state().open.as_mut() {
            open.item = Some(item.to_owned());
        }
    }

    /// The item named by [`Self::start_item`] is done.
    pub(crate) fn finish_item(&self) {
        if let Some(open) = self.state().open.as_mut() {
            open.items_done += 1;
            open.item = None;
        }
    }

    /// The pass ran to its end: close the phase in flight.
    pub(crate) fn end(&self) {
        let now = Instant::now();
        self.state().close_open(now);
    }

    pub(crate) fn record_probe_work(
        &self,
        kind: &str,
        elapsed: Duration,
        attempted: usize,
        completed: usize,
        deferred: usize,
    ) {
        let mut state = self.state();
        let work = state.probe_work.entry(kind.to_owned()).or_default();
        work.elapsed += elapsed;
        work.attempted += attempted;
        work.completed += completed;
        work.deferred += deferred;
    }

    /// What [`Self::record_probe_work`] has recorded under `kind` this pass.
    pub(crate) fn probe_work(&self, kind: &str) -> ProbeWork {
        self.state().probe_work.get(kind).cloned().unwrap_or_default()
    }

    pub(crate) fn interrupt(&self) {
        self.state().interrupted = true;
    }

    /// Read the trace. Safe at any point, including after the pass future
    /// was dropped mid-phase.
    pub fn timings(&self) -> PassTimings {
        let now = Instant::now();
        let state = self.state();
        PassTimings {
            interrupted: state.interrupted,
            total: state
                .started
                .map(|started| now.saturating_duration_since(started))
                .unwrap_or_default(),
            phases: state.finished.clone(),
            probe_work: state.probe_work.clone(),
            in_flight: state.open.as_ref().map(|open| open.timing(now)),
        }
    }
}

/// `EngineHealthIssue::kind` of the abandoned-pass notice in the engine
/// health report.
pub const DETECTION_STALL_ISSUE_KIND: &str = "merge_detection_stalled";

/// Full detection passes are not finishing their open-PR walk: the poller
/// ran out of budget, or it yielded at the open-walk reserve with candidates
/// still unwalked. Raised by the first such pass, updated in place by each
/// further one, and cleared by the next pass that walks every open candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectionStall {
    /// Abandoned passes since the last pass that ran to its end.
    pub consecutive_abandoned: u32,
    /// Unix epoch seconds of the first abandoned pass in this run.
    pub first_abandoned_epoch_s: i64,
    /// Unix epoch seconds of the most recent abandoned pass.
    pub latest_abandoned_epoch_s: i64,
    /// The budget the most recent pass exceeded.
    pub budget: Duration,
    /// Phase timings of the most recent abandoned pass.
    pub timings: PassTimings,
}

/// The poller's channel to the engine health report: `Some` while full
/// detection passes are being abandoned.
pub type DetectionStallSignal = tokio::sync::watch::Sender<Option<DetectionStall>>;

/// A [`DetectionStallSignal`] that reports no stall until a pass is abandoned.
pub fn detection_stall_signal() -> Arc<DetectionStallSignal> {
    Arc::new(tokio::sync::watch::channel(None).0)
}

/// Record that a full pass was abandoned after `budget`, extending the
/// current run of abandoned passes or starting one.
pub(crate) fn record_abandoned_pass(
    signal: &DetectionStallSignal,
    budget: Duration,
    timings: PassTimings,
    now_epoch_secs: i64,
) {
    signal.send_modify(|stall| {
        let (consecutive_abandoned, first_abandoned_epoch_s) = match stall.as_ref() {
            Some(previous) => (
                previous.consecutive_abandoned.saturating_add(1),
                previous.first_abandoned_epoch_s,
            ),
            None => (1, now_epoch_secs),
        };
        *stall = Some(DetectionStall {
            consecutive_abandoned,
            first_abandoned_epoch_s,
            latest_abandoned_epoch_s: now_epoch_secs,
            budget,
            timings,
        });
    });
}

/// A full pass ran to its end: withdraw the stall notice, if one is up.
pub(crate) fn record_completed_pass(signal: &DetectionStallSignal) {
    signal.send_if_modified(|stall| stall.take().is_some());
}

/// Record whether this pass finished its open-PR walk. A pass that returns
/// from its future but left open candidates unwalked is still a stall: the
/// health banner stays up until a pass covers every open PR.
pub(crate) fn record_pass_result(
    signal: &DetectionStallSignal,
    completed: bool,
    timings: &PassTimings,
    budget: Duration,
    now_epoch_secs: i64,
) {
    if timings.interrupted {
        return;
    }
    if completed && !timings.open_walk_truncated() {
        record_completed_pass(signal);
    } else {
        record_abandoned_pass(signal, budget, timings.clone(), now_epoch_secs);
    }
}

/// Render the stall as the engine-health issue the app banner and `bossctl`
/// show. The engine owns the wording; the app renders it verbatim.
///
/// A single abandoned pass is a warning; a second in a row is an error,
/// because by then PR state on the board is minutes behind GitHub.
pub fn detection_stall_health_issue(stall: &DetectionStall, now_epoch_secs: i64) -> EngineHealthIssue {
    let passes = match stall.consecutive_abandoned {
        1 => "the last detection pass".to_owned(),
        n => format!("the last {n} detection passes"),
    };
    let since = boss_engine_utils::iso8601::format_paused_since_phrase(stall.first_abandoned_epoch_s, now_epoch_secs);
    let since = if since.is_empty() { "just now".to_owned() } else { since };
    let cut_off = match stall.timings.in_flight.as_ref() {
        Some(phase) => format!(
            "The most recent one was cut off in its `{}` phase after finishing {} of {} item(s) there.",
            phase.phase.as_str(),
            phase.items_done,
            phase.items_total,
        ),
        None => match stall
            .timings
            .phases
            .iter()
            .find(|phase| phase.phase == PassPhase::OpenWalk)
            .filter(|phase| phase.items_done < phase.items_total)
        {
            Some(open) => format!(
                "The most recent one walked {} of {} open PR(s) and left the rest for the next pass.",
                open.items_done, open.items_total,
            ),
            None => "The most recent one was cut off between phases.".to_owned(),
        },
    };
    EngineHealthIssue {
        kind: DETECTION_STALL_ISSUE_KIND.to_owned(),
        severity: if stall.consecutive_abandoned > 1 {
            "error"
        } else {
            "warning"
        }
        .to_owned(),
        title: format!("PR merge detection is running out of time: {passes} did not finish"),
        body: format!(
            "Starting {since}, {passes} did not finish within the {budget}s budget. {cut_off}\n\n\
             Observed merged and closed PRs are recorded first. Probe fallback attempts rotate \
             by attendance, prioritizing previously observed terminal candidates; unfinished observations \
             are retried on later passes. Until a pass walks every \
             open PR, CI, review and merge-queue state on the board can be several minutes behind GitHub.\n\n\
             Phase timings of the most recent pass ({unfinished} item(s) left unfinished):\n{breakdown}\n\n\
             This notice clears itself once a pass walks every open candidate.",
            budget = stall.budget.as_secs(),
            unfinished = stall.timings.items_unfinished(),
            breakdown = stall.timings.breakdown(),
        ),
    }
}
