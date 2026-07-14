use super::*;

/// Trait for checking the live state of a GitHub PR URL.
///
/// Injected into `create_revision` so the gate can distinguish "open"
/// from "closed without merging" without hardcoding a `gh` call, which
/// would make unit tests depend on GitHub access. Production wires in
/// [`GhPrStateChecker`]; tests pass [`FakePrStateChecker`].
pub trait PrStateChecker: Send + Sync {
    /// Return the live lifecycle state of the given PR URL.
    fn check(&self, pr_url: &str) -> Result<PrOpenState>;
}

/// Lifecycle state returned by [`PrStateChecker::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrOpenState {
    Open,
    Merged,
    ClosedUnmerged,
}

/// Coarse GitHub PR lifecycle classification derivable purely from the
/// `(state, mergedAt)` pair returned by `gh pr view`.
///
/// This is the single source of truth for the merged / closed-unmerged /
/// open tri-state. Both the merge poller
/// ([`crate::merge_poller::classify_state`]) and the revision gate
/// ([`map_gh_state`]) map from this shared classification into their own
/// richer enums ([`crate::merge_poller::PrLifecycleState`] /
/// [`PrOpenState`]), so the (state, mergedAt) decision lives in exactly
/// one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrMergeClass {
    /// `state == MERGED`, or a non-empty `mergedAt` timestamp is present.
    Merged,
    /// `state == CLOSED` and the PR was not merged.
    ClosedUnmerged,
    /// Still open, or an unknown / empty state treated as still-open.
    Open,
}

/// Classify a GitHub PR's `(state, mergedAt)` fields into the shared
/// [`PrMergeClass`] tri-state.
///
/// Rules:
///   - `state == MERGED` (case-insensitive) **or** a non-empty,
///     non-`"null"` `mergedAt` → [`PrMergeClass::Merged`].
///   - `state == CLOSED` (case-insensitive), not merged →
///     [`PrMergeClass::ClosedUnmerged`].
///   - anything else (including a missing / empty / unrecognized state)
///     → [`PrMergeClass::Open`].
///
/// Matching is case-insensitive so `gh`'s canonical uppercase values and
/// any lowercase drift map alike. `merged_at` is treated as absent when
/// it is empty or the literal string `"null"` — a JSON `null` surfaces
/// either as an absent field (empty here) or, when stringified, as the
/// text `null`.
pub fn classify_pr_merge_state(state: &str, merged_at: &str) -> PrMergeClass {
    let merged_at_present = !merged_at.is_empty() && !merged_at.eq_ignore_ascii_case("null");
    if state.eq_ignore_ascii_case("MERGED") || merged_at_present {
        PrMergeClass::Merged
    } else if state.eq_ignore_ascii_case("CLOSED") {
        PrMergeClass::ClosedUnmerged
    } else {
        PrMergeClass::Open
    }
}

/// Production implementation: shells out to `gh pr view`.
pub struct GhPrStateChecker;

impl PrStateChecker for GhPrStateChecker {
    fn check(&self, pr_url: &str) -> Result<PrOpenState> {
        let output = std::process::Command::new("gh")
            .args(["pr", "view", pr_url, "--json", "state,mergedAt"])
            .output()
            .with_context(|| format!("failed to run `gh pr view` for {pr_url}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("`gh pr view` failed for {pr_url}: {stderr}");
        }
        let body = String::from_utf8_lossy(&output.stdout);
        let v: serde_json::Value =
            serde_json::from_str(&body).with_context(|| format!("failed to parse `gh pr view` JSON for {pr_url}"))?;
        Ok(map_gh_state(&v))
    }
}

/// Pure mapping from the parsed `gh pr view --json state,mergedAt` JSON to a
/// [`PrOpenState`]. Extracted from [`GhPrStateChecker::check`] so the mapping
/// can be unit-tested without shelling out to `gh`.
///
/// Delegates the merged / closed / open decision to the shared
/// [`classify_pr_merge_state`] helper, so matching is case-insensitive
/// (`gh`'s canonical uppercase values and any lowercase drift map alike)
/// and a non-empty `mergedAt` counts as merged even when `state` is not
/// `MERGED`. A missing or unrecognized state falls through to
/// [`PrOpenState::Open`].
fn map_gh_state(v: &serde_json::Value) -> PrOpenState {
    let state = v["state"].as_str().unwrap_or("");
    let merged_at = v["mergedAt"].as_str().unwrap_or("");
    match classify_pr_merge_state(state, merged_at) {
        PrMergeClass::Merged => PrOpenState::Merged,
        PrMergeClass::ClosedUnmerged => PrOpenState::ClosedUnmerged,
        PrMergeClass::Open => PrOpenState::Open,
    }
}

/// A `PrStateChecker` that returns a fixed, already-observed state without
/// issuing a `gh` call. Used by the merge-conflict producer (Phase 3): the
/// poller has *just* probed the PR live and is acting on an
/// `OpenPrMergeability::Conflict` result, which by construction means the PR
/// is open. Feeding that observation straight into the `create_revision`
/// gate reuses the gate's parent-revisable invariant (R4) while avoiding a
/// redundant — and, in tests, non-hermetic — `gh pr view` round-trip.
pub struct StaticPrStateChecker(pub PrOpenState);

impl PrStateChecker for StaticPrStateChecker {
    fn check(&self, _pr_url: &str) -> Result<PrOpenState> {
        Ok(self.0.clone())
    }
}

/// Test double: returns a preset state for known PR URLs.
#[cfg(test)]
pub struct FakePrStateChecker {
    pub states: std::collections::HashMap<String, PrOpenState>,
    pub default: PrOpenState,
}

#[cfg(test)]
impl FakePrStateChecker {
    pub fn always(state: PrOpenState) -> Self {
        Self {
            states: Default::default(),
            default: state,
        }
    }
    pub fn with(mut self, url: &str, state: PrOpenState) -> Self {
        self.states.insert(url.to_owned(), state);
        self
    }
}

#[cfg(test)]
impl PrStateChecker for FakePrStateChecker {
    fn check(&self, pr_url: &str) -> Result<PrOpenState> {
        Ok(self.states.get(pr_url).cloned().unwrap_or(self.default.clone()))
    }
}

/// Errors produced by the create-time revision gate.
#[derive(Debug, thiserror::Error)]
pub enum RevisionGateError {
    #[error(
        "T{short_id} has no PR yet; a revision targets an existing open PR. \
         Wait for T{short_id} to reach review, or file a normal follow-up chore."
    )]
    NoPr { short_id: i64 },

    #[error(
        "T{short_id}'s PR (#{pr_number}) is already merged; revisions only apply to \
         open, unmerged PRs. File a new chore against main instead."
    )]
    Merged { short_id: i64, pr_number: i64 },

    #[error(
        "T{short_id}'s PR (#{pr_number}) is closed without merging; \
         there is no open PR to revise."
    )]
    ClosedUnmerged { short_id: i64, pr_number: i64 },
}

impl RevisionGateError {
    pub(crate) fn no_pr(task: &Task) -> Self {
        Self::NoPr {
            short_id: task.short_id.unwrap_or(0),
        }
    }
    pub(crate) fn merged(task: &Task, pr_url: &str) -> Self {
        use crate::merge_poller::parse_pr_number;
        Self::Merged {
            short_id: task.short_id.unwrap_or(0),
            pr_number: parse_pr_number(pr_url).unwrap_or(0),
        }
    }
    pub(crate) fn closed(task: &Task, pr_url: &str) -> Self {
        use crate::merge_poller::parse_pr_number;
        Self::ClosedUnmerged {
            short_id: task.short_id.unwrap_or(0),
            pr_number: parse_pr_number(pr_url).unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merged_state_maps_to_merged() {
        assert_eq!(
            map_gh_state(&json!({"state": "MERGED", "mergedAt": "2026-01-01T00:00:00Z"})),
            PrOpenState::Merged
        );
    }

    #[test]
    fn closed_state_maps_to_closed_unmerged() {
        assert_eq!(
            map_gh_state(&json!({"state": "CLOSED", "mergedAt": null})),
            PrOpenState::ClosedUnmerged
        );
    }

    #[test]
    fn open_state_maps_to_open() {
        assert_eq!(
            map_gh_state(&json!({"state": "OPEN", "mergedAt": null})),
            PrOpenState::Open
        );
    }

    #[test]
    fn lowercase_states_still_map_correctly() {
        // check() upper-cases before matching, so lowercase inputs map like their canonical forms.
        assert_eq!(map_gh_state(&json!({"state": "merged"})), PrOpenState::Merged);
        assert_eq!(map_gh_state(&json!({"state": "closed"})), PrOpenState::ClosedUnmerged);
        assert_eq!(map_gh_state(&json!({"state": "open"})), PrOpenState::Open);
    }

    #[test]
    fn unrecognized_state_maps_to_open() {
        assert_eq!(map_gh_state(&json!({"state": "SOMETHING_ELSE"})), PrOpenState::Open);
    }

    #[test]
    fn missing_state_maps_to_open() {
        assert_eq!(map_gh_state(&json!({"mergedAt": null})), PrOpenState::Open);
    }

    #[test]
    fn non_string_state_maps_to_open() {
        // A non-string `state` value can't be read as a str, so it falls through to Open.
        assert_eq!(map_gh_state(&json!({"state": 42})), PrOpenState::Open);
    }

    #[test]
    fn non_empty_merged_at_maps_to_merged_even_if_state_not_merged() {
        // Previously map_gh_state ignored `mergedAt`; now a non-empty
        // timestamp classifies as Merged regardless of `state`, matching
        // the merge poller's `classify_state`.
        assert_eq!(
            map_gh_state(&json!({"state": "OPEN", "mergedAt": "2026-01-01T00:00:00Z"})),
            PrOpenState::Merged
        );
    }

    #[test]
    fn classify_pr_merge_state_covers_the_tri_state() {
        // state == MERGED
        assert_eq!(classify_pr_merge_state("MERGED", ""), PrMergeClass::Merged);
        // non-empty mergedAt wins even when state isn't MERGED
        assert_eq!(
            classify_pr_merge_state("OPEN", "2026-01-01T00:00:00Z"),
            PrMergeClass::Merged
        );
        // closed and not merged
        assert_eq!(classify_pr_merge_state("CLOSED", ""), PrMergeClass::ClosedUnmerged);
        assert_eq!(classify_pr_merge_state("CLOSED", "null"), PrMergeClass::ClosedUnmerged);
        // open / unknown / empty
        assert_eq!(classify_pr_merge_state("OPEN", ""), PrMergeClass::Open);
        assert_eq!(classify_pr_merge_state("", ""), PrMergeClass::Open);
        assert_eq!(classify_pr_merge_state("SOMETHING_ELSE", ""), PrMergeClass::Open);
    }

    #[test]
    fn classify_pr_merge_state_is_case_insensitive() {
        assert_eq!(classify_pr_merge_state("merged", ""), PrMergeClass::Merged);
        assert_eq!(classify_pr_merge_state("closed", "null"), PrMergeClass::ClosedUnmerged);
        assert_eq!(classify_pr_merge_state("open", ""), PrMergeClass::Open);
        // "null" in any case is treated as absent mergedAt.
        assert_eq!(classify_pr_merge_state("OPEN", "NULL"), PrMergeClass::Open);
    }
}
