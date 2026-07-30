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
/// engine cannot deliver is never accepted in the first place — it comes back
/// as `FrontendEvent::ProbeRefused` — so every variant here is a promise the
/// engine believes it can keep at the time of the call.
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
    /// The probe waits for the worker's next turn boundary (`Stop`). This is
    /// now the *last-resort* contract, reported only for a worker whose
    /// driver does not read mid-turn stdin at all (`codex exec`): for such a
    /// worker there is no earlier opportunity, and for one mid-way through a
    /// long turn it can be a while, which is exactly why the CLI says so.
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
                "will be injected at the worker's next turn boundary (its driver takes no mid-turn input)"
            }
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
    /// The pane write was issued, but the worker's recorded process had
    /// already exited: the bytes went into a pane nothing was reading. Distinct
    /// from [`Self::Unconfirmed`] (where the worker is alive and the text
    /// probably landed, just unobservably) — here there was demonstrably
    /// nobody home, so this must not be reported as [`Self::Consumed`].
    ///
    /// The verdict rests on a `kill(pid, 0)` probe of the *recorded* shell
    /// pid, which the engine treats elsewhere as a fragile identity (a wrapper
    /// shell that exec'd or exited leaves the agent alive under another pid).
    /// So this is undeliverable-as-far-as-we-know rather than proof of loss:
    /// if the worker answers anyway, the reply path corrects the record to
    /// [`Self::Replied`]. That is why it is not terminal.
    Orphaned,
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
        }
    }

    /// True when the probe is known to have reached the worker (or its input
    /// buffer). `Queued`/`Injected` are still in progress, and `Unconfirmed`
    /// is by definition unproven.
    pub fn is_delivered(self) -> bool {
        matches!(self, Self::Consumed | Self::Buffered | Self::Replied)
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
        matches!(self, Self::Replied | Self::Dropped | Self::Abandoned)
    }

    /// True when the engine has given up on delivering this probe.
    ///
    /// The point of this predicate is the contrast with `Queued`. `Queued` and
    /// `Injected` are *live* — something is still expected to happen — so a
    /// probe reported in either state indefinitely is a bug in the engine, not
    /// a slow worker. Every path that removes a probe from the pending queue
    /// without delivering it must land on one of the states here.
    pub fn is_undeliverable(self) -> bool {
        matches!(self, Self::Dropped | Self::Abandoned | Self::Orphaned)
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
