//! Probe delivery reporting types.
//!
//! A probe is text the coordinator injects into a live worker's pane.
//!
//! These two types are what make accepting a probe a commitment rather than a
//! receipt: the engine states up front *which boundary* it expects to deliver
//! at ([`ProbeDeliveryExpectation`]), refuses outright when it cannot deliver
//! at all, and exposes a per-probe-id [`ProbeDeliveryState`] that can be
//! queried afterwards. Without them, "arriving shortly" and "never going to
//! arrive" are indistinguishable from outside the engine.

use serde::{Deserialize, Serialize};

/// Where the engine expects a freshly accepted probe to be delivered.
///
/// Returned on `FrontendEvent::ProbeQueued` so the CLI can describe what was
/// actually accepted instead of guessing from the `urgent` flag. A probe the
/// engine cannot deliver *at all* is never accepted in the first place — it
/// comes back as `FrontendEvent::ProbeRefused`.
///
/// The variants are not all equally strong, though, and treating them as if
/// they were is what let an accepted probe be silently lost. Two of them are
/// promises the engine keeps unilaterally; the third names a boundary the
/// engine will use *if it arrives*, which for a worker whose next turn
/// boundary is its last it may not. [`Self::is_best_effort`] is that
/// distinction, and clients must surface it at issue time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeDeliveryExpectation {
    /// The engine wrote the text into the pane during this call, because the
    /// pane would take a write: either the worker is parked at its prompt
    /// (no boundary is coming on its own — the case a boundary-only design
    /// hangs on), or it is mid-turn on a driver that buffers pane input, so
    /// the text sits in the agent's composer and it picks the text up as it
    /// works. This is the ordinary expectation for a live Claude worker.
    Immediate,
    /// No write could be issued yet, but the worker's driver buffers mid-turn
    /// pane input, so the probe is injected at the next `PostToolUse`
    /// boundary — i.e. as soon as its first/next tool call returns. Reported
    /// for a worker that is still spawning.
    NextToolBoundary,
    /// The probe waits for the worker's next turn boundary (`Stop`). The
    /// *last-resort* contract, reported only for a worker whose driver has
    /// not measured its mid-turn stdin behaviour and so holds the `Rejects`
    /// trait default: for such a worker there is no earlier opportunity.
    ///
    /// This is the one variant that is **not** a promise the engine can keep
    /// on its own — see [`Self::is_best_effort`]. A worker mid-way through a
    /// long turn may simply take a while, but a worker whose next turn
    /// boundary is also its last reaches that boundary and is torn down
    /// together with it, and the probe settles [`ProbeDeliveryState::Abandoned`]
    /// or [`ProbeDeliveryState::Orphaned`] having steered nothing. The engine
    /// spends the boundary on the probe before completion can consume it
    /// (`dispatch_probe_on_stop` runs on both sides of the completion
    /// handler), which is what makes the common case work — but the caller
    /// still has to be told that a boundary is all they are getting.
    NextTurnBoundary,
}

impl ProbeDeliveryExpectation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::NextToolBoundary => "next_tool_boundary",
            Self::NextTurnBoundary => "next_turn_boundary",
        }
    }

    /// Human-readable description of when the probe is expected to land,
    /// phrased to be pasted into CLI output after "probe accepted; ".
    pub fn describe(self) -> &'static str {
        match self {
            Self::Immediate => "written into the worker's pane now (parked, or buffered mid-turn by the agent)",
            Self::NextToolBoundary => "will be injected when the worker's next tool call returns",
            Self::NextTurnBoundary => {
                "delivery is BEST-EFFORT: it will be injected at the worker's next turn boundary \
                 (its driver's mid-turn input handling is undeclared), and that boundary may never \
                 arrive while the worker is still alive"
            }
        }
    }

    /// Whether the named boundary is one the engine cannot guarantee will
    /// arrive while the worker is still there to read the text.
    ///
    /// [`Self::Immediate`] and [`Self::NextToolBoundary`] both rest on the
    /// driver consuming mid-turn pane input: the engine has either already
    /// written the text or gets its opportunity inside the running turn, and
    /// a turn that is running is a turn with tool calls left in it.
    /// [`Self::NextTurnBoundary`] rests on nothing of the kind. It is the
    /// answer for a driver whose mid-turn stdin behaviour is undeclared, so
    /// the *only* delivery opportunity is a turn boundary — and a worker
    /// whose next turn boundary is also its last (an autonomous run that ends
    /// by opening its PR) reaches that boundary and is torn down in the same
    /// breath. The probe is then honestly recorded undeliverable, but the
    /// caller has already acted on "accepted".
    ///
    /// This is a property of the *delivery contract*, not of any one driver:
    /// every driver that has not measured its mid-turn stdin behaviour holds
    /// the safe `Rejects` default and lands here, and any driver that later
    /// measures `Buffers` leaves without this predicate changing.
    pub fn is_best_effort(self) -> bool {
        matches!(self, Self::NextTurnBoundary)
    }

    /// The caveat a caller must be shown at issue time when
    /// [`Self::is_best_effort`] holds, or `None` when the engine's
    /// expectation is one it can keep unilaterally.
    ///
    /// Separate from [`Self::describe`] so a CLI can route it to stderr and
    /// keep `--json` stdout clean, and so the wording of the warning lives
    /// next to the predicate that decides whether to warn.
    pub fn caveat(self) -> Option<&'static str> {
        match self {
            Self::Immediate | Self::NextToolBoundary => None,
            Self::NextTurnBoundary => Some(
                "this worker's driver does not take mid-turn input, so the engine can only inject \
                 at a turn boundary. If the worker's next turn boundary is its last — an autonomous \
                 run that ends by opening its PR — the text will never be acted on and the probe \
                 settles `abandoned`. Do not rely on this channel for an instruction that must be \
                 obeyed; prefer one that survives the run (a work-item description edit, a review \
                 revision) when correctness depends on it.",
            ),
        }
    }
}

/// Observable delivery state of one probe, keyed by probe id.
///
/// Queried with `FrontendRequest::ProbeStatus`. The states are ordered by
/// progress but not all probes visit all of them: a delivery to a parked
/// worker goes `Queued → Injected → Consumed`, a mid-turn one goes
/// `Queued → Injected → Buffered`, and either can end at `Replied` once the
/// worker answers.
///
/// Not every probe is delivered, and the failure modes are deliberately
/// distinct states rather than a probe left sitting at [`Self::Queued`]
/// forever. `Queued` is a live promise: it means the engine still intends to
/// deliver. The moment that stops being true — the engine discards the probe
/// ([`Self::Dropped`]), the run it targeted goes away ([`Self::Abandoned`]),
/// or the bytes were written into a pane whose process had already exited
/// ([`Self::Orphaned`]) — the record must say so, because a caller cannot
/// otherwise tell "arriving shortly" from "never going to arrive". All three
/// answer [`Self::is_undeliverable`]; the first two are also
/// [`Self::is_terminal`], while `Orphaned` can still be corrected by a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeDeliveryState {
    /// Accepted and waiting for its delivery boundary. Nothing has been
    /// written to the pane yet.
    Queued,
    /// The pane write has been issued and the engine is waiting to see
    /// whether the worker's CLI took it as a prompt.
    Injected,
    /// Confirmed: the worker's CLI enqueued the injected text as a prompt
    /// (observed via its `UserPromptSubmit` hook, or by finding the text in
    /// the session transcript).
    Consumed,
    /// Written into a mid-turn agent that buffers pane input. The text sits
    /// in the agent's composer and becomes its prompt when the current turn
    /// ends. Not a failure state and not the same as [`Self::Unconfirmed`]:
    /// no prompt submission is expected yet.
    Buffered,
    /// The verification window elapsed with no confirming signal on a worker
    /// that was parked and should have submitted immediately. "Unproven",
    /// not "lost" — the engine deliberately does not auto-redeliver, because
    /// doing so risks handing the worker the same instruction twice.
    Unconfirmed,
    /// The worker replied and the engine published the reply on the probe
    /// topic.
    Replied,
    /// The engine deliberately discarded the probe before any pane write —
    /// e.g. a stale nudge cleared when the worker turned out to be
    /// `[blocked]`. Terminal: it will never be delivered, and the `detail`
    /// on the status record says why it was discarded.
    Dropped,
    /// The run the probe targeted went away before its delivery boundary
    /// arrived: the worker's pane was released or the run reached a terminal
    /// state with the probe still queued. Terminal, and never the caller's
    /// fault — the boundary the engine committed to simply never came.
    Abandoned,
    /// The pane write was issued, but nothing was left to act on it. Two
    /// paths reach this state, and they share the property that makes it
    /// distinct from [`Self::Unconfirmed`] (where the worker is alive and the
    /// text probably landed, just unobservably): here there was demonstrably
    /// nobody home, so it must not be reported as [`Self::Consumed`].
    ///
    /// * The worker's *recorded* process had already exited when the bytes
    ///   went in. That verdict rests on a `kill(pid, 0)` probe of a pid the
    ///   engine treats elsewhere as a fragile identity (a wrapper shell that
    ///   exec'd or exited leaves the agent alive under another pid).
    /// * The write landed, and then the run's pane was released before the
    ///   worker produced the boundary its reply would have arrived on. This
    ///   is the shape a probe delivered on a run's *final* turn boundary
    ///   takes: the pty took the bytes, and teardown reached the worker
    ///   first. Leaving such a probe at `Consumed` would report a delivery
    ///   nobody ever acted on as a success.
    ///
    /// Either way this is undeliverable-as-far-as-we-know rather than proof
    /// of loss: if the worker answers anyway, the reply path corrects the
    /// record to [`Self::Replied`]. That is why it is not terminal.
    Orphaned,
    /// The engine set out to interrupt the worker's in-flight turn so the
    /// probe could land immediately, and the interrupt did not take: the
    /// keystrokes were delivered, the driver's plan was given every attempt
    /// it declares, and the turn never ended. **Nothing was typed into the
    /// pane** — that is the whole point of confirming before injecting, since
    /// text written into a still-running turn is the input that gets
    /// swallowed or misread.
    ///
    /// Also the state for a worker whose driver declares no interrupt
    /// mechanism at all when interrupting delivery was required: the engine
    /// says so rather than silently falling back to a boundary it cannot
    /// promise.
    ///
    /// Terminal and undeliverable. Unlike [`Self::Orphaned`] there is no
    /// possibility of a late correction, because no bytes ever reached the
    /// pane: the worker cannot reply to text it was never sent. Re-issuing
    /// the same instruction is therefore safe — it cannot duplicate one that
    /// landed — but it will hit the same wall unless the worker's state has
    /// changed, so the honest advice is to use a channel that survives the
    /// run (a work-item description edit) instead.
    InterruptFailed,
}

impl ProbeDeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Injected => "injected",
            Self::Consumed => "consumed",
            Self::Buffered => "buffered",
            Self::Unconfirmed => "unconfirmed",
            Self::Replied => "replied",
            Self::Dropped => "dropped",
            Self::Abandoned => "abandoned",
            Self::Orphaned => "orphaned",
            Self::InterruptFailed => "interrupt_failed",
        }
    }

    /// True when the probe is known to have reached the worker (or its input
    /// buffer). `Queued`/`Injected` are still in progress, and `Unconfirmed`
    /// is by definition unproven.
    pub fn is_delivered(self) -> bool {
        matches!(self, Self::Consumed | Self::Buffered | Self::Replied)
    }

    /// True when the engine still intends to deliver and has not yet recorded
    /// an outcome. `Queued` is waiting for a claim or a boundary; `Injected`
    /// is waiting for confirmation of a write that has already gone out.
    ///
    /// A synchronous `bossctl probe` that reports either of these as
    /// "NOT delivered" is lying: the probe has not failed, it has not
    /// finished. Distinct from [`Self::Unconfirmed`], which *did* write.
    pub fn is_in_progress(self) -> bool {
        matches!(self, Self::Queued | Self::Injected)
    }

    /// True when no further transition is possible: the worker answered, or
    /// the probe definitively will not be delivered.
    ///
    /// `Consumed`/`Buffered`/`Unconfirmed`/`Orphaned` are *not* terminal —
    /// each can still become `Replied` when the worker's answer lands. Only
    /// `Dropped` and `Abandoned` are settled by construction: in both the
    /// engine discarded the probe before any pane write, so there is nothing
    /// out there that could produce a reply.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Replied | Self::Dropped | Self::Abandoned | Self::InterruptFailed
        )
    }

    /// True when the engine has given up on delivering this probe.
    ///
    /// The point of this predicate is the contrast with `Queued`. `Queued` and
    /// `Injected` are *live* — something is still expected to happen — so a
    /// probe reported in either state indefinitely is a bug in the engine, not
    /// a slow worker. Every path that removes a probe from the pending queue
    /// without delivering it must land on one of the states here.
    pub fn is_undeliverable(self) -> bool {
        matches!(
            self,
            Self::Dropped | Self::Abandoned | Self::Orphaned | Self::InterruptFailed
        )
    }

    /// True when nothing was ever written into the worker's pane for this
    /// probe, so re-issuing the same instruction cannot deliver it twice.
    ///
    /// This is the predicate a caller deciding "is it safe to send this
    /// again?" wants, and it is deliberately narrower than
    /// [`Self::is_undeliverable`]: [`Self::Orphaned`] is undeliverable but
    /// the bytes *did* reach the pty, so a second copy could compound with a
    /// first one that was quietly read. `Unconfirmed` is not here for the
    /// same reason.
    pub fn is_safe_to_reissue(self) -> bool {
        matches!(self, Self::Dropped | Self::Abandoned | Self::InterruptFailed)
    }
}

/// What the interrupting probe path did about the worker's in-flight turn.
///
/// Reported alongside the settled [`ProbeDeliveryState`] on
/// `FrontendEvent::ProbeDelivered`, because "the probe landed" and "the
/// engine had to destroy in-flight work to make it land" are different facts
/// and the caller needs both. Interrupting is not free — it aborts whatever
/// tool call was running — so a caller that sees [`Self::NotNeeded`] knows
/// nothing was discarded, and one that sees [`Self::Interrupted`] knows
/// something was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeInterruptOutcome {
    /// The worker was mid-turn, the engine delivered the driver's interrupt
    /// gesture, and the turn end was confirmed before anything was typed.
    /// In-flight work was discarded.
    Interrupted,
    /// The worker was already parked at its prompt, so there was no turn to
    /// interrupt and none was attempted. The text was written directly. No
    /// work was discarded — this is the cheap case, and the reason the engine
    /// checks posture before reaching for the keystroke.
    NotNeeded,
    /// The worker reached a turn boundary on its own between the engine
    /// deciding to interrupt and the gesture being delivered. Nothing was
    /// sent; the engine typed into the now-parked pane instead. Distinct from
    /// [`Self::NotNeeded`] so the race is visible rather than looking like
    /// the worker was idle all along.
    EndedNaturally,
    /// The run's driver declares no interrupt mechanism
    /// (`AgentDriver::interrupt_plan() == None`). Nothing was attempted, and
    /// the probe is *not* silently downgraded to boundary delivery — it
    /// settles [`ProbeDeliveryState::InterruptFailed`] and the caller is told
    /// which driver could not be interrupted.
    DriverCannotInterrupt,
    /// The gesture was delivered and the driver's whole attempt budget was
    /// spent without the turn ending. Nothing was typed into the pane.
    Failed,
    /// No interrupt could be attempted (or one was cut short mid-attempt)
    /// because the worker's own state disappeared out from under the
    /// interrupt path: it was pre-session or terminal when the check ran, or
    /// its slot mapping / live activity went away while an attempt was in
    /// flight. Distinct from [`Self::Failed`], whose `describe()` states that
    /// a gesture *was* delivered and a full attempt budget was spent — a
    /// claim that does not hold here, where either no gesture was ever sent
    /// or the reason it stopped was the worker vanishing, not the budget
    /// running out.
    WorkerGone,
    /// No interrupt was attempted because another delivery path had already
    /// claimed this probe (or the run's single in-flight slot) by the time
    /// the interrupting path reached it — a boundary fan-out that got there
    /// first. Deliberately distinct from [`Self::NotNeeded`] and
    /// [`Self::EndedNaturally`]: both of those describe the *worker's* state,
    /// and reporting either here would tell the operator something about the
    /// worker that was never observed. The accompanying
    /// [`ProbeDeliveryState`] is whatever that other path recorded.
    NotAttempted,
}

impl ProbeInterruptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::NotNeeded => "not_needed",
            Self::EndedNaturally => "ended_naturally",
            Self::DriverCannotInterrupt => "driver_cannot_interrupt",
            Self::Failed => "failed",
            Self::WorkerGone => "worker_gone",
            Self::NotAttempted => "not_attempted",
        }
    }

    /// True when the engine actually cancelled in-flight work to make room
    /// for this probe. Only [`Self::Interrupted`] destroys anything: the
    /// other outcomes either found the worker already parked, raced with a
    /// natural boundary, or never sent the gesture at all.
    pub fn discarded_in_flight_work(self) -> bool {
        matches!(self, Self::Interrupted)
    }

    /// One line for CLI output, phrased to follow "probe delivered; ".
    pub fn describe(self) -> &'static str {
        match self {
            Self::Interrupted => {
                "the worker's in-flight turn was interrupted and confirmed ended first, so whatever \
                 it was doing at that moment was abandoned mid-flight"
            }
            Self::NotNeeded => "the worker was already parked at its prompt, so no interrupt was needed",
            Self::EndedNaturally => {
                "the worker reached a turn boundary on its own before the interrupt was sent, so none was"
            }
            Self::DriverCannotInterrupt => {
                "this worker's driver declares no interrupt mechanism, so its turn could not be cut short"
            }
            Self::Failed => {
                "the interrupt was delivered but the turn never ended within the driver's declared \
                 attempt budget; nothing was typed into the pane"
            }
            Self::WorkerGone => {
                "the worker's own state disappeared before (or while) the interrupt was attempted, so \
                 no gesture landed or none could be confirmed; nothing was typed into the pane"
            }
            Self::NotAttempted => {
                "another delivery path already held this probe, so no interrupt was attempted and the \
                 state above is what that path recorded"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_state_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProbeDeliveryState::Unconfirmed).unwrap(),
            "\"unconfirmed\"",
        );
    }

    #[test]
    fn delivery_state_round_trips_through_serde() {
        for state in [
            ProbeDeliveryState::Queued,
            ProbeDeliveryState::Injected,
            ProbeDeliveryState::Consumed,
            ProbeDeliveryState::Buffered,
            ProbeDeliveryState::Unconfirmed,
            ProbeDeliveryState::Replied,
            ProbeDeliveryState::Dropped,
            ProbeDeliveryState::Abandoned,
            ProbeDeliveryState::Orphaned,
            ProbeDeliveryState::InterruptFailed,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let parsed: ProbeDeliveryState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, state);
            // `as_str` is what CLI output renders; it must agree with the
            // wire form so text and JSON output share one vocabulary.
            assert_eq!(json, format!("\"{}\"", state.as_str()));
        }
    }

    #[test]
    fn only_reached_the_worker_counts_as_delivered() {
        assert!(ProbeDeliveryState::Consumed.is_delivered());
        assert!(ProbeDeliveryState::Buffered.is_delivered());
        assert!(ProbeDeliveryState::Replied.is_delivered());
        assert!(!ProbeDeliveryState::Queued.is_delivered());
        assert!(!ProbeDeliveryState::Injected.is_delivered());
        assert!(!ProbeDeliveryState::Unconfirmed.is_delivered());
        assert!(!ProbeDeliveryState::Dropped.is_delivered());
        assert!(!ProbeDeliveryState::Abandoned.is_delivered());
        assert!(!ProbeDeliveryState::Orphaned.is_delivered());
        assert!(!ProbeDeliveryState::InterruptFailed.is_delivered());
    }

    /// `Queued`/`Injected` are live promises, not losses. Collapsing them into
    /// "not delivered" is the reporting bug that made a still-queued probe
    /// look like a failed one.
    #[test]
    fn only_queued_and_injected_are_in_progress() {
        assert!(ProbeDeliveryState::Queued.is_in_progress());
        assert!(ProbeDeliveryState::Injected.is_in_progress());
        for settled in [
            ProbeDeliveryState::Consumed,
            ProbeDeliveryState::Buffered,
            ProbeDeliveryState::Unconfirmed,
            ProbeDeliveryState::Replied,
            ProbeDeliveryState::Dropped,
            ProbeDeliveryState::Abandoned,
            ProbeDeliveryState::Orphaned,
            ProbeDeliveryState::InterruptFailed,
        ] {
            assert!(
                !settled.is_in_progress(),
                "{} is an outcome, not an in-progress state",
                settled.as_str()
            );
        }
    }

    #[test]
    fn live_states_are_neither_terminal_nor_undeliverable() {
        // The whole point of the terminal states: a probe that is no longer
        // going to be delivered must not read as one that is still on its
        // way. `Queued`/`Injected` are the only two "still coming" states.
        for live in [ProbeDeliveryState::Queued, ProbeDeliveryState::Injected] {
            assert!(!live.is_terminal(), "{} must stay live", live.as_str());
            assert!(!live.is_undeliverable(), "{} must stay live", live.as_str());
        }
        for gave_up in [
            ProbeDeliveryState::Dropped,
            ProbeDeliveryState::Abandoned,
            ProbeDeliveryState::Orphaned,
            ProbeDeliveryState::InterruptFailed,
        ] {
            assert!(gave_up.is_undeliverable(), "{} must be undeliverable", gave_up.as_str());
        }
        // Discarded before any pane write: nothing exists that could reply.
        assert!(ProbeDeliveryState::Dropped.is_terminal());
        assert!(ProbeDeliveryState::Abandoned.is_terminal());
        // The bytes did reach the pane, and the dead-pid verdict rests on a
        // fragile pid identity — a reply that arrives anyway must be able to
        // correct the record.
        assert!(!ProbeDeliveryState::Orphaned.is_terminal());
        // Delivered-but-unanswered states can still advance to `Replied`, so
        // they are not terminal even though they are not failures either.
        for pending_reply in [
            ProbeDeliveryState::Consumed,
            ProbeDeliveryState::Buffered,
            ProbeDeliveryState::Unconfirmed,
        ] {
            assert!(
                !pending_reply.is_terminal(),
                "{} can still be replied to",
                pending_reply.as_str()
            );
            assert!(!pending_reply.is_undeliverable());
        }
        assert!(ProbeDeliveryState::Replied.is_terminal());
        assert!(!ProbeDeliveryState::Replied.is_undeliverable());
    }

    /// The whole point of the predicate: "the engine will write this" and
    /// "the engine will write this if it ever gets the chance" must be
    /// distinguishable by a caller deciding whether to rely on the channel.
    #[test]
    fn only_the_turn_boundary_expectation_is_best_effort() {
        assert!(ProbeDeliveryExpectation::NextTurnBoundary.is_best_effort());
        assert!(!ProbeDeliveryExpectation::Immediate.is_best_effort());
        assert!(!ProbeDeliveryExpectation::NextToolBoundary.is_best_effort());
    }

    /// A failed interrupt must be as loud as any other give-up state, and
    /// louder in one respect: it is settled for good. `Orphaned` can be
    /// corrected by a late reply because bytes reached the pty; here nothing
    /// did, so no reply can ever arrive to soften it.
    #[test]
    fn interrupt_failure_is_terminal_undeliverable_and_safe_to_reissue() {
        let state = ProbeDeliveryState::InterruptFailed;
        assert!(state.is_terminal());
        assert!(state.is_undeliverable());
        assert!(!state.is_delivered());
        assert!(state.is_safe_to_reissue());
    }

    /// The reissue predicate must not collapse into `is_undeliverable`: the
    /// difference between them is precisely whether bytes reached the pane,
    /// which is the difference between a safe retry and a duplicated
    /// instruction.
    #[test]
    fn only_states_that_wrote_nothing_are_safe_to_reissue() {
        for wrote_nothing in [
            ProbeDeliveryState::Dropped,
            ProbeDeliveryState::Abandoned,
            ProbeDeliveryState::InterruptFailed,
        ] {
            assert!(wrote_nothing.is_safe_to_reissue(), "{}", wrote_nothing.as_str());
        }
        // Bytes reached the pty in both of these, so a second copy could
        // compound with a first one that was quietly read.
        assert!(!ProbeDeliveryState::Orphaned.is_safe_to_reissue());
        assert!(!ProbeDeliveryState::Unconfirmed.is_safe_to_reissue());
        for delivered in [
            ProbeDeliveryState::Consumed,
            ProbeDeliveryState::Buffered,
            ProbeDeliveryState::Replied,
        ] {
            assert!(!delivered.is_safe_to_reissue(), "{}", delivered.as_str());
        }
    }

    #[test]
    fn interrupt_outcome_round_trips_and_names_itself_consistently() {
        for outcome in [
            ProbeInterruptOutcome::Interrupted,
            ProbeInterruptOutcome::NotNeeded,
            ProbeInterruptOutcome::EndedNaturally,
            ProbeInterruptOutcome::DriverCannotInterrupt,
            ProbeInterruptOutcome::Failed,
            ProbeInterruptOutcome::WorkerGone,
            ProbeInterruptOutcome::NotAttempted,
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            assert_eq!(json, format!("\"{}\"", outcome.as_str()));
            let parsed: ProbeInterruptOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, outcome);
            assert!(!outcome.describe().is_empty());
        }
    }

    /// Only an actual interrupt destroys work. Getting this wrong in either
    /// direction misinforms the operator about what their probe cost: a false
    /// positive makes a free delivery look destructive, a false negative
    /// hides that a tool call was aborted.
    #[test]
    fn only_a_real_interrupt_reports_discarded_work() {
        assert!(ProbeInterruptOutcome::Interrupted.discarded_in_flight_work());
        for cheap in [
            ProbeInterruptOutcome::NotNeeded,
            ProbeInterruptOutcome::EndedNaturally,
            ProbeInterruptOutcome::DriverCannotInterrupt,
            ProbeInterruptOutcome::Failed,
            ProbeInterruptOutcome::WorkerGone,
            ProbeInterruptOutcome::NotAttempted,
        ] {
            assert!(!cheap.discarded_in_flight_work(), "{}", cheap.as_str());
        }
    }

    /// A best-effort acceptance must carry the explanation with it. Pairing
    /// the two here stops a future variant gaining `is_best_effort` without
    /// the text a caller needs in order to choose a different mechanism.
    #[test]
    fn every_best_effort_expectation_carries_a_caveat_and_no_other_does() {
        for expectation in [
            ProbeDeliveryExpectation::Immediate,
            ProbeDeliveryExpectation::NextToolBoundary,
            ProbeDeliveryExpectation::NextTurnBoundary,
        ] {
            assert_eq!(
                expectation.caveat().is_some(),
                expectation.is_best_effort(),
                "{} must carry a caveat exactly when it is best-effort",
                expectation.as_str(),
            );
        }
    }

    /// `describe()` is the line the CLI prints on the accept path, so the
    /// best-effort case must not read as a plain commitment there either —
    /// the caveat on stderr is an addition, not the only warning.
    #[test]
    fn best_effort_description_does_not_read_as_a_plain_commitment() {
        let described = ProbeDeliveryExpectation::NextTurnBoundary.describe();
        assert!(
            described.contains("BEST-EFFORT"),
            "best-effort acceptance must say so in its one-line description; got {described:?}",
        );
        assert!(
            described.contains("may never arrive"),
            "the description must name the failure mode, not just label it; got {described:?}",
        );
    }

    #[test]
    fn expectation_serializes_as_snake_case_matching_as_str() {
        for expectation in [
            ProbeDeliveryExpectation::Immediate,
            ProbeDeliveryExpectation::NextToolBoundary,
            ProbeDeliveryExpectation::NextTurnBoundary,
        ] {
            let json = serde_json::to_string(&expectation).unwrap();
            assert_eq!(json, format!("\"{}\"", expectation.as_str()));
            let parsed: ProbeDeliveryExpectation = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, expectation);
        }
    }
}
