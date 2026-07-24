//! GitHub-authoritative verification that a merge-conflict revision's
//! conflict is actually gone before the engine accepts a "nothing left to
//! do" Stop.
//!
//! ## Why this exists
//!
//! A merge-conflict revision worker (`created_via = merge-conflict:*`)
//! stopped after 26 seconds without pushing anything, asserting the
//! conflict had "already been resolved and pushed in a prior attempt".
//! GitHub said `mergeable: CONFLICTING` / `mergeStateStatus: DIRTY` — the
//! worker had never queried the field, and never ran the mandatory
//! `cube workspace rebase` either. Divergent jj change ids in the shared
//! cube object store made two local revsets resolve to two *different*
//! commits, so `conflicts=false` and `descendants(main@origin)` were each
//! true of a different copy and neither of both.
//!
//! The only guard on that Stop was the generic SHA-delta gate, whose
//! nudge text (`probe_push_to_existing_pr`) literally offers the escape
//! hatch the worker took: *"if there is nothing left to do, say so"*. For
//! a conflict revision that is the wrong contract — "is there still a
//! conflict" is objectively checkable and the engine already holds the
//! bound PR URL, so it must check rather than take the worker's word.
//!
//! ## Contract
//!
//! Only `mergeable == MERGEABLE` validates an "already resolved" claim.
//! `CONFLICTING` refuses it outright. `UNKNOWN` — GitHub recomputing
//! mergeability asynchronously — is **never** read as mergeable; it is
//! retried with backoff and, if it never settles, still refuses the
//! claim. Mapping `UNKNOWN` to "clean" is exactly the bug that let a
//! `succeeded` attempt land at an un-advanced head in mono#1398/#1764.

use std::time::Duration;

use crate::merge_poller::{MergeProbe, OpenPrMergeability, PrLifecycleProbe, PrLifecycleState};

/// How many extra probes to spend waiting out a `mergeable=UNKNOWN`
/// before giving up and refusing the claim anyway. GitHub's async
/// mergeability recompute normally settles within a few seconds of the
/// base or head moving, so a couple of re-probes converts almost every
/// transient `UNKNOWN` into a definitive answer.
pub const UNKNOWN_RETRY_ATTEMPTS: u32 = 2;

/// Base delay between `UNKNOWN` re-probes; doubled on each retry. Only
/// ever paid on the rare Stop of a conflict revision that pushed
/// nothing, never on a hot path.
///
/// Worst case, a persistent `UNKNOWN` blocks the `on_stop` handler for
/// `DEFAULT_UNKNOWN_RETRY_BACKOFF * (2^UNKNOWN_RETRY_ATTEMPTS - 1)` = 3s +
/// 6s = 9s before returning [`ConflictClearance::Indeterminate`]. A
/// caller that already holds a fresh probe result should pass it via
/// [`verify_conflict_cleared_from`] rather than call [`verify_conflict_cleared`]
/// blind — that skips the first (redundant) probe but not the sleeps
/// past it.
pub const DEFAULT_UNKNOWN_RETRY_BACKOFF: Duration = Duration::from_secs(3);

/// What GitHub says about a merge-conflict revision's bound PR at the
/// Stop boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictClearance {
    /// GitHub reports the PR genuinely mergeable. The worker's
    /// "already resolved" claim is corroborated — accept it.
    Cleared,
    /// GitHub still reports the PR conflicting. The claim is false
    /// regardless of what local jj state suggested; refuse it and quote
    /// the live values back at the worker.
    StillConflicting {
        raw_mergeable: String,
        raw_merge_state_status: String,
    },
    /// GitHub's mergeability was still `UNKNOWN` after the bounded
    /// retries. Not evidence of anything — in particular NOT evidence
    /// the conflict cleared — so the claim is still refused, but with
    /// text that says why.
    Indeterminate,
    /// The PR is merged/closed, or the probe itself failed. This gate
    /// has no opinion; the caller falls back to its normal handling.
    Unavailable,
}

/// Ask GitHub whether `pr_url` still conflicts, retrying past a
/// transient `mergeable=UNKNOWN`.
///
/// `unknown_backoff` is the base delay between `UNKNOWN` re-probes
/// (doubled each retry); pass [`Duration::ZERO`] in tests. A probe error
/// is reported as [`ConflictClearance::Unavailable`] rather than
/// retried — a broken `gh` invocation will not fix itself in three
/// seconds, and the caller's fallback path is safe.
pub async fn verify_conflict_cleared(
    probe: &dyn MergeProbe,
    pr_url: &str,
    unknown_backoff: Duration,
) -> ConflictClearance {
    verify_conflict_cleared_from(probe, pr_url, unknown_backoff, None).await
}

/// Same contract as [`verify_conflict_cleared`], but lets the caller pass
/// an already-fetched `initial` probe result to stand in for attempt 0 —
/// skipping the redundant re-probe when the caller (e.g.
/// `completion::try_retire_cleared_blocking_signal`) already probed this
/// PR microseconds earlier and observed it was not yet `Clean`. Every
/// subsequent `UNKNOWN` retry still probes for real.
pub async fn verify_conflict_cleared_from(
    probe: &dyn MergeProbe,
    pr_url: &str,
    unknown_backoff: Duration,
    initial: Option<PrLifecycleProbe>,
) -> ConflictClearance {
    let mut initial = initial;
    let mut delay = unknown_backoff;
    for attempt in 0..=UNKNOWN_RETRY_ATTEMPTS {
        let result = if let Some(cached) = initial.take() {
            cached
        } else {
            match probe.probe(pr_url).await {
                Ok(p) => p,
                Err(err) => {
                    tracing::warn!(
                        pr_url,
                        ?err,
                        "conflict stop gate: PR probe failed; cannot verify the conflict claim",
                    );
                    return ConflictClearance::Unavailable;
                }
            }
        };
        let open = match result.state {
            PrLifecycleState::Open(ref open) => open,
            // Merged or closed: the conflict question is moot and the
            // caller's own merged/closed handling is the right one.
            _ => return ConflictClearance::Unavailable,
        };
        match open.mergeability {
            OpenPrMergeability::Clean => return ConflictClearance::Cleared,
            OpenPrMergeability::Conflict => {
                return ConflictClearance::StillConflicting {
                    raw_mergeable: non_empty_or(&result.raw_mergeable, "CONFLICTING"),
                    raw_merge_state_status: non_empty_or(&result.raw_merge_state_status, "DIRTY"),
                };
            }
            OpenPrMergeability::Unknown => {
                if attempt == UNKNOWN_RETRY_ATTEMPTS {
                    tracing::info!(
                        pr_url,
                        probes = attempt + 1,
                        "conflict stop gate: mergeability still UNKNOWN after retries; \
                         refusing the 'already resolved' claim rather than assuming mergeable",
                    );
                    return ConflictClearance::Indeterminate;
                }
                tracing::debug!(
                    pr_url,
                    attempt,
                    backoff_ms = delay.as_millis(),
                    "conflict stop gate: mergeability UNKNOWN (GitHub recomputing); re-probing",
                );
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
        }
    }
    // Unreachable: the loop returns on every path of its final iteration.
    ConflictClearance::Indeterminate
}

/// GitHub omits the raw strings on some probe transports (and test
/// doubles default them to `""`); fall back to the canonical value the
/// derived [`OpenPrMergeability`] variant implies so the probe text
/// never renders an empty quote.
fn non_empty_or(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

/// Probe text for a merge-conflict revision that stopped without pushing
/// while GitHub still reports the PR conflicting.
///
/// Deliberately does NOT offer the "or say there is nothing left to do"
/// escape hatch that [`crate::completion::probe_push_to_existing_pr`]
/// carries: for this execution kind the question is settled, and the
/// answer is that there IS something left to do. It quotes the live
/// GitHub values and names the one command whose output can contradict
/// them.
pub fn probe_conflict_still_present(pr_url: &str, raw_mergeable: &str, raw_merge_state_status: &str) -> String {
    format!(
        "GitHub still reports `mergeable: {raw_mergeable}` / `mergeStateStatus: \
{raw_merge_state_status}` on {pr_url}, and this run pushed nothing. The conflict is NOT resolved \
— whatever your local `jj` state showed, GitHub is the authority here and it disagrees. Do NOT \
claim \"already resolved\" or \"nothing left to do\": run `cube workspace rebase` and paste its \
output before drawing any conclusion. If it reports `REBASED_WITH_CONFLICTS`, resolve every \
conflicted commit and push with `cube pr update --branch <bookmark>`. If any `jj` output shows a \
`??` suffix on a change id (e.g. `qtltpmoy??`), that change is DIVERGENT — change-id revsets \
resolve to an arbitrary copy, so every `conflicts=` / `descendants()` answer you got is unsound; \
re-check using full commit ids. If you genuinely cannot resolve it, run `boss engine conflicts \
mark-failed <attempt-id> --reason <reason>` — do not just stop."
    )
}

/// Probe text for a merge-conflict revision that stopped without pushing
/// while GitHub's mergeability is still `UNKNOWN` after retries.
///
/// `UNKNOWN` is not permission to conclude the conflict cleared — it is
/// an unanswered question, and the worker holds the one tool that can
/// answer it locally.
pub fn probe_conflict_mergeability_unknown(pr_url: &str) -> String {
    format!(
        "This run pushed nothing, and GitHub's `mergeable` field for {pr_url} is still `UNKNOWN` \
(mergeability recompute in flight) — that is NOT evidence the conflict cleared, so \"already \
resolved\" is not a conclusion you may draw yet. Run `cube workspace rebase` and paste its output: \
`REBASED_CLEAN` settles it in your favour, `REBASED_WITH_CONFLICTS` means resolve each conflicted \
commit and push with `cube pr update --branch <bookmark>`. Then re-check with `gh pr view {pr_url} \
--json mergeable,mergeStateStatus`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge_poller::{OpenPrStatus, PrLifecycleProbe, PrReviewState};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Returns a scripted sequence of probe results, one per call; the
    /// last entry repeats once exhausted so a caller that probes more
    /// times than scripted still gets a definite answer.
    struct ScriptedProbe {
        states: Mutex<Vec<PrLifecycleState>>,
        calls: Mutex<u32>,
        raw: (String, String),
    }

    impl ScriptedProbe {
        fn new(states: Vec<PrLifecycleState>) -> Self {
            Self {
                states: Mutex::new(states),
                calls: Mutex::new(0),
                raw: (String::new(), String::new()),
            }
        }

        fn with_raw(mut self, mergeable: &str, merge_state_status: &str) -> Self {
            self.raw = (mergeable.to_owned(), merge_state_status.to_owned());
            self
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl MergeProbe for ScriptedProbe {
        async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
            *self.calls.lock().unwrap() += 1;
            let state = {
                let mut states = self.states.lock().unwrap();
                if states.len() > 1 {
                    states.remove(0)
                } else {
                    states[0].clone()
                }
            };
            Ok(PrLifecycleProbe::builder()
                .url(url.to_owned())
                .state(state)
                .labels(Vec::new())
                .review(PrReviewState::Unknown)
                .raw_mergeable(self.raw.0.clone())
                .raw_merge_state_status(self.raw.1.clone())
                .build())
        }
    }

    struct FailingProbe;

    #[async_trait]
    impl MergeProbe for FailingProbe {
        async fn probe(&self, _url: &str) -> anyhow::Result<PrLifecycleProbe> {
            Err(anyhow::anyhow!("gh: connection reset"))
        }
    }

    const PR: &str = "https://github.com/spinyfin/mono/pull/2070";

    #[tokio::test]
    async fn mergeable_pr_validates_the_already_resolved_claim() {
        let probe = ScriptedProbe::new(vec![PrLifecycleState::Open(OpenPrStatus::clean())]);
        assert_eq!(
            verify_conflict_cleared(&probe, PR, Duration::ZERO).await,
            ConflictClearance::Cleared,
        );
        assert_eq!(probe.calls(), 1, "a definitive answer must not be re-probed");
    }

    #[tokio::test]
    async fn conflicting_pr_refuses_the_claim_and_carries_the_live_values() {
        let probe = ScriptedProbe::new(vec![PrLifecycleState::Open(OpenPrStatus::conflict_only())])
            .with_raw("CONFLICTING", "DIRTY");
        assert_eq!(
            verify_conflict_cleared(&probe, PR, Duration::ZERO).await,
            ConflictClearance::StillConflicting {
                raw_mergeable: "CONFLICTING".into(),
                raw_merge_state_status: "DIRTY".into(),
            },
        );
    }

    #[tokio::test]
    async fn unknown_mergeability_is_retried_and_settles_to_the_definitive_answer() {
        // GitHub is mid-recompute on the first probe and settles to
        // CONFLICTING on the second — the retry, not a coin flip, is what
        // produces the correct refusal.
        let probe = ScriptedProbe::new(vec![
            PrLifecycleState::Open(OpenPrStatus::unknown_mergeability()),
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
        ])
        .with_raw("CONFLICTING", "DIRTY");
        assert_eq!(
            verify_conflict_cleared(&probe, PR, Duration::ZERO).await,
            ConflictClearance::StillConflicting {
                raw_mergeable: "CONFLICTING".into(),
                raw_merge_state_status: "DIRTY".into(),
            },
        );
        assert_eq!(probe.calls(), 2, "UNKNOWN must be re-probed, not accepted");
    }

    #[tokio::test]
    async fn persistent_unknown_is_never_reported_as_cleared() {
        // The load-bearing invariant: UNKNOWN must never be laundered
        // into "mergeable". It exhausts the retries and refuses.
        let probe = ScriptedProbe::new(vec![PrLifecycleState::Open(OpenPrStatus::unknown_mergeability())]);
        assert_eq!(
            verify_conflict_cleared(&probe, PR, Duration::ZERO).await,
            ConflictClearance::Indeterminate,
        );
        assert_eq!(
            probe.calls(),
            UNKNOWN_RETRY_ATTEMPTS + 1,
            "every retry budgeted for UNKNOWN must be spent before refusing",
        );
    }

    #[tokio::test]
    async fn merged_pr_yields_no_opinion() {
        let probe = ScriptedProbe::new(vec![PrLifecycleState::Merged]);
        assert_eq!(
            verify_conflict_cleared(&probe, PR, Duration::ZERO).await,
            ConflictClearance::Unavailable,
        );
    }

    #[tokio::test]
    async fn probe_failure_yields_no_opinion_without_retrying() {
        assert_eq!(
            verify_conflict_cleared(&FailingProbe, PR, Duration::ZERO).await,
            ConflictClearance::Unavailable,
        );
    }

    #[tokio::test]
    async fn missing_raw_strings_fall_back_to_the_canonical_values() {
        let probe = ScriptedProbe::new(vec![PrLifecycleState::Open(OpenPrStatus::conflict_only())]);
        assert_eq!(
            verify_conflict_cleared(&probe, PR, Duration::ZERO).await,
            ConflictClearance::StillConflicting {
                raw_mergeable: "CONFLICTING".into(),
                raw_merge_state_status: "DIRTY".into(),
            },
            "an empty raw string must not render as an empty quote in the probe text",
        );
    }

    #[test]
    fn conflict_probe_text_quotes_github_and_withholds_the_escape_hatch() {
        let text = probe_conflict_still_present(PR, "CONFLICTING", "DIRTY");
        assert!(
            text.contains("CONFLICTING"),
            "must quote the live mergeable value: {text}"
        );
        assert!(text.contains("DIRTY"), "must quote the live mergeStateStatus: {text}");
        assert!(text.contains(PR), "must name the PR: {text}");
        assert!(
            text.contains("cube workspace rebase"),
            "must name the command that can contradict GitHub: {text}",
        );
        assert!(
            text.contains("??"),
            "must teach the jj divergence hazard that produced the false claim: {text}",
        );
        assert!(
            !text.contains("or there is nothing left to do"),
            "must NOT repeat the generic probe's escape hatch: {text}",
        );
    }

    #[test]
    fn unknown_probe_text_refuses_to_read_unknown_as_resolved() {
        let text = probe_conflict_mergeability_unknown(PR);
        assert!(text.contains("UNKNOWN"), "must name the indeterminate value: {text}");
        assert!(
            text.contains("cube workspace rebase"),
            "must name the settling command: {text}"
        );
        assert!(
            text.contains("not a conclusion you may draw"),
            "must explicitly deny the 'already resolved' conclusion: {text}",
        );
    }
}
