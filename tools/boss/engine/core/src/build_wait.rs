//! Heuristic detection of a worker's Stop-boundary narration that it is
//! legitimately waiting on a backgrounded build/test gate, as distinct from
//! being idle or wedged.
//!
//! Background: incident 2026-07-14 (automation pool, T2608 / T2612,
//! `exec_18c21add1416b5e8_3b` and `exec_18c21ba9b3fd2ef8_9e`). A worker had
//! finished its edits and was waiting on a backgrounded `bazel build`/`test`
//! pre-push gate under heavy host contention, with an armed harness monitor
//! that would wake it once the build finished. Ending its turn to wait is
//! *correct* worker behaviour — but each such Stop, with no PR yet, was
//! indistinguishable to [`crate::completion::WorkerCompletionHandler`] from a
//! worker that is idle or wedged, so it queued a "produce a PR" probe. The
//! worker woke, replied "still building, waiting", and stopped again — the
//! probe itself manufactured the next Stop. Four such nudges in about two
//! minutes exhausted the auto-nudge circuit breaker
//! ([`crate::nudge_breaker`]) and the execution was parked/abandoned, its
//! validated-but-unpushed work discarded, even though the worker was healthy
//! and making real (if slow) progress.
//!
//! This module gives the completion handler a way to recognize that
//! narration so [`crate::completion::WorkerCompletionHandler::nudge_or_park`]
//! can suppress the nudge instead of dogging a worker that just explained it
//! is waiting on something outside its control. Detection is a small,
//! deliberately narrow set of phrases lifted from the real incident
//! transcript — mirroring [`crate::worker_escalation::detect_heuristic_blocker`]'s
//! low-confidence-net style — not a general sentiment classifier. A false
//! negative here just means the normal nudge/park path runs as it always
//! has; a false positive would suppress a nudge that should have fired, so
//! the phrase list stays conservative.
//!
//! Suppression from this signal alone is not indefinite: pairing with
//! [`crate::build_wait_tracker::BuildWaitTracker`] bounds how long a
//! continuously-reported wait is trusted before the normal nudge/park flow
//! resumes, so a worker that keeps narrating "waiting" without ever actually
//! finishing is still eventually surfaced (see that module for the
//! wedge-vs-waiting discriminator this pairs with).

/// One build-wait signal detected in a worker's Stop-boundary text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildWaitSignal {
    /// The phrase from [`PHRASES`] that matched, for logging.
    pub matched_phrase: &'static str,
}

/// Phrases pulled from the incident transcript (`exec_18c21add1416b5e8_3b`,
/// 08:38:07Z) and its sibling, all describing a worker deliberately waiting
/// on a backgrounded build/test gate rather than being idle. Matched
/// case-insensitively as a substring — prose that merely mentions "build" in
/// passing (e.g. "I'll build the feature") must not trip this.
const PHRASES: &[&str] = &[
    "the build gate requires",
    "monitor is armed",
    "i'm not going to push until",
    "im not going to push until",
    "still building, waiting",
    "still building. waiting",
    "waiting for the test run",
    "test run comes back green",
    "waiting for a green build",
    "waiting for the build to finish",
    "waiting for the build/test gate",
    "waiting on the backgrounded",
];

/// Scan `text` (a worker's Stop-boundary assistant prose) for a phrase
/// indicating it is legitimately waiting on a backgrounded build/test gate.
/// Returns the first match, or `None` if nothing in [`PHRASES`] is present.
pub fn detect_build_wait_signal(text: &str) -> Option<BuildWaitSignal> {
    let lower = text.to_lowercase();
    PHRASES
        .iter()
        .find(|phrase| lower.contains(*phrase))
        .map(|phrase| BuildWaitSignal { matched_phrase: phrase })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_incident_transcript_phrasing() {
        let text = "the build gate requires an actual green build before I push, so I must let it \
                     finish. The monitor (task b9qrwn8c7) is armed... I'm not going to push until \
                     the test run comes back green. Waiting.";
        let signal = detect_build_wait_signal(text).expect("must match incident phrasing");
        assert_eq!(signal.matched_phrase, "the build gate requires");
    }

    #[test]
    fn detects_the_woken_worker_reply() {
        let signal = detect_build_wait_signal("still building, waiting").expect("must match");
        assert_eq!(signal.matched_phrase, "still building, waiting");
    }

    #[test]
    fn is_case_insensitive() {
        assert!(detect_build_wait_signal("STILL BUILDING, WAITING").is_some());
    }

    #[test]
    fn does_not_match_unrelated_prose_mentioning_build() {
        assert!(detect_build_wait_signal("I'll build the feature and open a PR.").is_none());
        assert!(detect_build_wait_signal("## Summary\nOpened the PR.\n").is_none());
    }

    #[test]
    fn empty_text_does_not_match() {
        assert!(detect_build_wait_signal("").is_none());
    }
}
