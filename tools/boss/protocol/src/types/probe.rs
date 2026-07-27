//! Probe delivery reporting types.
//!
//! A probe is text the coordinator injects into a live worker's pane. Queueing
//! one used to be reported as unconditional success — `bossctl probe` printed
//! "queued" (or "will inject at next tool boundary") and exited 0 whether or
//! not delivery was possible. When delivery then never happened there was no
//! way to tell "arriving shortly" from "never going to arrive", which cost an
//! operator a worker restart and several minutes of in-flight work.
//!
//! These two types are what makes the reporting honest: the engine states up
//! front *which boundary* it expects to deliver at
//! ([`ProbeDeliveryExpectation`]), refuses outright when it cannot deliver at
//! all, and exposes a per-probe-id [`ProbeDeliveryState`] that can be queried
//! afterwards.

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
    /// The worker is parked at its prompt, so the engine wrote the text into
    /// the pane during this call. No boundary is needed (and none is coming
    /// on its own — this is the case a boundary-only design would hang on).
    Immediate,
    /// The worker is mid-turn on a driver that buffers pane input, and the
    /// probe is urgent: it will be injected at the next `PostToolUse`
    /// boundary, i.e. as soon as the in-flight tool call returns.
    NextToolBoundary,
    /// The probe waits for the worker's next turn boundary (`Stop`). This is
    /// the ordinary non-urgent contract; for a worker mid-way through a long
    /// turn it can be a while, which is exactly why the CLI now says so.
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
            Self::Immediate => "written into the worker's pane now (it was parked at its prompt)",
            Self::NextToolBoundary => "will be injected when the worker's in-flight tool call returns",
            Self::NextTurnBoundary => "will be injected at the worker's next turn boundary",
        }
    }
}

/// Observable delivery state of one probe, keyed by probe id.
///
/// Queried with `FrontendRequest::ProbeStatus`. The states are ordered by
/// progress but not all probes visit all of them: an immediate delivery goes
/// `Queued → Injected → Consumed`, a mid-turn urgent one goes
/// `Queued → Injected → Buffered`, and either can end at `Replied` once the
/// worker answers.
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
        }
    }

    /// True when the probe is known to have reached the worker (or its input
    /// buffer). `Queued`/`Injected` are still in progress, and `Unconfirmed`
    /// is by definition unproven.
    pub fn is_delivered(self) -> bool {
        matches!(self, Self::Consumed | Self::Buffered | Self::Replied)
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
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let parsed: ProbeDeliveryState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, state);
            // `as_str` is what CLI output renders; it must agree with the
            // wire form so operators and JSON consumers see one vocabulary.
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
