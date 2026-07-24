use super::*;

/// Review-gating state of a PR at probe time. Derived from
/// GitHub's `reviewDecision` field and the `reviews` array.
///
/// `Required` maps to `REVIEW_REQUIRED` — at least one approving
/// review is still needed. `Approved` means all required reviewers
/// have approved; the `reviewers` list carries their login names
/// for the tooltip. `ChangesRequested` means at least one reviewer
/// blocked the PR; `reviewers` lists who. `Unknown` is the
/// fallback when GitHub omitted the field or returned an
/// unrecognised value (e.g., no branch protection configured).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrReviewState {
    Required,
    Approved { reviewers: Vec<String> },
    ChangesRequested { reviewers: Vec<String> },
    Unknown,
}

impl PrReviewState {
    /// Stable DB string for the `review_required_state` column.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            PrReviewState::Required => "required",
            PrReviewState::Approved { .. } => "approved",
            PrReviewState::ChangesRequested { .. } => "changes_requested",
            PrReviewState::Unknown => "unknown",
        }
    }

    /// Reviewer login names for the tooltip, if available.
    pub fn reviewers(&self) -> &[String] {
        match self {
            PrReviewState::Approved { reviewers } | PrReviewState::ChangesRequested { reviewers } => reviewers,
            _ => &[],
        }
    }
}

/// Derive the [`PrReviewState`] from GitHub's `reviewDecision` string,
/// the `reviews` array, and an optional per-org review-signal verdict
/// produced from reclassified status checks (e.g. LinkedIn's
/// `Owner Approval`). Rules for the GitHub portion:
///
///   - `REVIEW_REQUIRED` → `Required` (no reviewers needed yet).
///   - `CHANGES_REQUESTED` → `ChangesRequested`; reviewers are the
///     latest CHANGES_REQUESTED submitters per author (de-duped).
///   - `APPROVED` → `Approved`; reviewers are the latest APPROVED
///     submitters per author (de-duped).
///   - Empty / `null` / unrecognised → `Unknown` (no branch
///     protection or first poll hasn't run). The UI hides the
///     indicator in this case rather than showing a misleading green.
///
/// `review_signal` then overlays per the dominance rule:
///   - `Pass` / `None` → no override; the GitHub verdict stands.
///   - `InFlight` → force `Required` unless the GitHub verdict is
///     `ChangesRequested` (a stronger negative signal we preserve).
///   - `Fail` → force `ChangesRequested { reviewers: [] }`. An ACL
///     rejection is conceptually "approval refused" but the rollup
///     leaf carries no reviewer identity, so we leave the list empty.
pub(crate) fn classify_review(
    review_decision: &str,
    reviews: &[serde_json::Value],
    review_signal: ReviewSignalVerdict,
) -> PrReviewState {
    // Collect the most-recent review state per author from the
    // `reviews` array. GitHub orders reviews oldest-to-newest so
    // iterating forward and overwriting gives us the latest per author.
    let mut by_author: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for review in reviews {
        let login = review
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let state = review
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        if !login.is_empty() && !state.is_empty() {
            by_author.insert(login, state);
        }
    }

    let base = match review_decision.to_ascii_uppercase().as_str() {
        "REVIEW_REQUIRED" => PrReviewState::Required,
        "CHANGES_REQUESTED" => {
            let reviewers = by_author
                .into_iter()
                .filter(|(_, state)| state == "CHANGES_REQUESTED")
                .map(|(login, _)| login)
                .collect();
            PrReviewState::ChangesRequested { reviewers }
        }
        "APPROVED" => {
            let reviewers = by_author
                .into_iter()
                .filter(|(_, state)| state == "APPROVED")
                .map(|(login, _)| login)
                .collect();
            PrReviewState::Approved { reviewers }
        }
        _ => PrReviewState::Unknown,
    };
    apply_review_signal(base, review_signal)
}

/// Apply a per-org review-signal verdict over the base GitHub review
/// state. `None` / `Pass` are no-ops; `InFlight` forces `Required`
/// unless the base already says `ChangesRequested`; `Fail` forces
/// `ChangesRequested { reviewers: [] }` (the leaf carries no identity).
pub(crate) fn apply_review_signal(base: PrReviewState, signal: ReviewSignalVerdict) -> PrReviewState {
    match signal {
        ReviewSignalVerdict::None | ReviewSignalVerdict::Pass => base,
        ReviewSignalVerdict::InFlight => match base {
            PrReviewState::ChangesRequested { .. } => base,
            _ => PrReviewState::Required,
        },
        ReviewSignalVerdict::Fail => PrReviewState::ChangesRequested { reviewers: Vec::new() },
    }
}

/// Verdict on a per-org "review signal" status check, after
/// [`normalize_leaf`]'s buckets are folded across all reclassified
/// leaves. `None` means no reclassified check is present on the PR
/// (the common case — non-LinkedIn org, or the check is absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewSignalVerdict {
    None,
    /// At least one reclassified check is still running.
    InFlight,
    /// All reclassified checks have completed successfully.
    Pass,
    /// At least one reclassified check has failed/errored.
    Fail,
}

/// Per-org table of status-check `context` names that are reclassified
/// from CI signals to review signals. Match is case-insensitive on
/// both axes. v1 hardcodes the two LinkedIn orgs known to ship the
/// `Owner Approval` (LI-ACL) check; the table shape is deliberately
/// extensible so adding more orgs (or more check names per org) later
/// is a one-line change rather than another aggregation-layer hook.
pub(crate) const REVIEW_SIGNAL_RULES: &[(&str, &[&str])] = &[
    ("linkedin-multiproduct", &["Owner Approval"]),
    ("linkedin-eng", &["Owner Approval"]),
];

/// The list of status-check `context` names to reclassify for `owner`.
/// Empty slice for unconfigured owners — the call site partitions on
/// that and the rollup is classified normally.
///
/// `pub(crate)` so the worker-prompt composer (`runner.rs`) can name the
/// same human-gated checks the CI classifier here reclassifies. That
/// single sourcing is the point of issue #899: the worker's
/// "don't wait on these checks" guidance and the engine's
/// "these checks don't block CI-clean" detection must not drift apart.
pub(crate) fn review_signal_checks_for_owner(owner: &str) -> &'static [&'static str] {
    for (org, names) in REVIEW_SIGNAL_RULES {
        if org.eq_ignore_ascii_case(owner) {
            return names;
        }
    }
    &[]
}

/// Extract just the `<owner>` segment from a GitHub PR URL of the
/// form `https://github.com/<owner>/<repo>/pull/<n>`. Returns `None`
/// when the URL does not match the GitHub PR shape.
pub(crate) fn owner_from_pr_url(pr_url: &str) -> Option<&str> {
    let repo = repo_from_pr_url(pr_url)?;
    Some(repo.split_once('/')?.0)
}

/// Whether a rollup leaf's check name (the `name` field on a CheckRun
/// or the `context` field on a StatusContext) matches any of `names`
/// case-insensitively. An empty `names` slice yields `false` without
/// inspecting the leaf, so the common no-reclassification path costs
/// one branch.
pub(crate) fn leaf_matches_check_name(leaf: &serde_json::Value, names: &[&str]) -> bool {
    if names.is_empty() {
        return false;
    }
    let leaf_name = leaf
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| leaf.get("context").and_then(|v| v.as_str()))
        .unwrap_or("");
    if leaf_name.is_empty() {
        return false;
    }
    names.iter().any(|n| n.eq_ignore_ascii_case(leaf_name))
}

/// Fold the partitioned review-signal leaves into one
/// [`ReviewSignalVerdict`] via [`normalize_leaf`]'s buckets.
/// Fail dominates InFlight which dominates Pass; an empty input
/// (the common case) → `None`.
pub(crate) fn classify_review_signal(leaves: &[serde_json::Value]) -> ReviewSignalVerdict {
    if leaves.is_empty() {
        return ReviewSignalVerdict::None;
    }
    let mut any_in_flight = false;
    let mut any_fail = false;
    for leaf in leaves {
        match normalize_leaf(leaf) {
            LeafVerdict::Fail { .. } => any_fail = true,
            LeafVerdict::InFlight => any_in_flight = true,
            LeafVerdict::Pass => {}
        }
    }
    if any_fail {
        ReviewSignalVerdict::Fail
    } else if any_in_flight {
        ReviewSignalVerdict::InFlight
    } else {
        ReviewSignalVerdict::Pass
    }
}

/// Verdict bucket a single rollup leaf contributes to. Produced by
/// [`normalize_leaf`] so the two GraphQL leaf shapes
/// (`CheckRun` and `StatusContext`) feed the same downstream branches
/// in [`classify_ci`].
pub(crate) enum LeafVerdict {
    /// Leaf is in a non-terminal state (queued / running / expected /
    /// briefly post-completion with empty conclusion).
    InFlight,
    /// Leaf reached a successful terminal state (`SUCCESS` /
    /// `NEUTRAL` / `SKIPPED`).
    Pass,
    /// Leaf reached a failing terminal state. `conclusion` is the
    /// uppercased token kept verbatim for the worker prompt /
    /// `ci_remediations.failed_checks` JSON.
    Fail { conclusion: String },
}

/// Normalize one rollup leaf into a [`LeafVerdict`]. `gh pr view
/// --json statusCheckRollup` returns a heterogeneous array containing
/// two GraphQL types:
///
///   - `CheckRun` — modern check-runs (GitHub Actions, most CI
///     integrations). Carries `name`, `status`, `conclusion`.
///   - `StatusContext` — the legacy commit-status API shape (Buildkite,
///     some self-hosted CI). Carries `context`, `state`. **No** `status`
///     or `conclusion` field.
///
/// Treating the two uniformly via `status`+`conclusion` (the pre-fix
/// behaviour) silently classifies every StatusContext leaf as InFlight
/// because both fields read empty, which is why a green Buildkite-only
/// PR stayed pinned on the yellow-clock badge indefinitely.
pub(crate) fn normalize_leaf(leaf: &serde_json::Value) -> LeafVerdict {
    let typename = leaf.get("__typename").and_then(|v| v.as_str()).unwrap_or("");

    // StatusContext: `state` carries the verdict. Values per GitHub's
    // commit-status API: SUCCESS / FAILURE / ERROR / PENDING / EXPECTED.
    // Dispatch on `__typename` when present; fall back to "has `state`
    // but no `conclusion`" so older fixtures (and any future leaf shape
    // that mirrors StatusContext) classify correctly.
    let has_status_context_shape = typename.eq_ignore_ascii_case("StatusContext")
        || (leaf.get("state").is_some() && leaf.get("conclusion").is_none());
    if has_status_context_shape {
        let state = leaf
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        return match state.as_str() {
            "SUCCESS" => LeafVerdict::Pass,
            "FAILURE" | "ERROR" => LeafVerdict::Fail { conclusion: state },
            // PENDING (running), EXPECTED (branch protection lists the
            // context but no run has reported yet), empty, or anything
            // else GitHub may add later → wait for a terminal verdict.
            _ => LeafVerdict::InFlight,
        };
    }

    // CheckRun (and unknown typenames that still carry CheckRun-shaped
    // fields): combine `status` and `conclusion`. A leaf is in-flight
    // when its status is one of GitHub's pending-shape values OR when
    // the conclusion is still empty (briefly, post-completion).
    let status = leaf
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    let conclusion = leaf
        .get("conclusion")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    let status_in_flight = matches!(
        status.as_str(),
        "IN_PROGRESS" | "QUEUED" | "PENDING" | "WAITING" | "REQUESTED" | ""
    );
    if conclusion.is_empty() {
        return LeafVerdict::InFlight;
    }
    if is_failure_conclusion(&conclusion) {
        return LeafVerdict::Fail { conclusion };
    }
    if is_pass_conclusion(&conclusion) {
        return LeafVerdict::Pass;
    }
    if status_in_flight {
        return LeafVerdict::InFlight;
    }
    // Unknown conclusion shape — treat as in-flight rather than
    // misclassifying it as a failure.
    LeafVerdict::InFlight
}

/// Collapse the `statusCheckRollup` array into one [`OpenPrCiStatus`]
/// per the design's §Q1 predicate:
///
///   1. Drop leaves where `isRequired` is explicitly `false`. Leaves
///      that don't report `isRequired` (legacy status contexts,
///      providers that don't fill the field) default to `true` —
///      branch protection is the authority, and we'd rather over-trip
///      on a third-party check than ignore a real signal.
///   2. Group by check name; pick the latest leaf per name (we use the
///      last entry, which matches GitHub's natural ordering for
///      re-runs — the most recent run lands last in the rollup).
///   3. For each surviving leaf, run [`normalize_leaf`] to fold the
///      two leaf shapes (`CheckRun` and `StatusContext`) into a single
///      verdict bucket.
///   4. If any required leaf has terminally failed → `Failing` immediately,
///      even while other required checks are still running. `Fail` dominates
///      `InFlight` for terminal failures — hiding a real failure until the
///      slowest check finishes defeats fast detection. Else if any required
///      leaf is still InFlight (and no terminal failure) → `InFlight`. Else
///      if the rollup was empty, consult `combined_state` from the legacy
///      commit-status REST API:
///        - `"pending"` / `"failure"` / `"error"` → `InFlight`
///          (required contexts configured but not yet submitted).
///        - `"success"` or absent → `Clean` (no required checks).
///
/// `combined_state` is only consulted when `leaves` is empty; for a
/// non-empty rollup the leaf data is authoritative.
///
/// Fast-fail design: a terminal failure (e.g. `checkleft` in 4 s) surfaces
/// `Failing` and spawns remediation immediately, even while a slow check
/// (e.g. `bazel-test` still running) is in flight. Anti-phantom protection
/// lives in the reconcile/withdraw path (`on_ci_in_flight_supersedes_failure`,
/// commit 3): a remediation spawned on a terminal failure that a later CI run
/// then clears is auto-withdrawn and the badge is reset to the authoritative
/// state. Do NOT re-add a "wait for all checks terminal" gate here — that was
/// a prior regression. Phantom prevention belongs in withdraw,
/// not in detection delay.
///
/// Excludes Trunk's own bookkeeping check (`"Trunk Merge Queue (<branch>)"`,
/// posted by the `trunk-io` app on the PR head) from the required-failure
/// set unconditionally. Trunk flips that check to failure the moment a
/// queue episode is evicted — a check name that can only exist on a repo
/// with Trunk installed, whose failure is already the authoritative
/// `ci_watch::on_trunk_queue_eviction_detected` path's own signal (driven
/// by `TrunkQueueProbe`, not this rollup). Without this exclusion the same
/// eviction would ALSO read as a failing required check here, on the PR's
/// own head, and spawn a duplicate/misleading `pr_branch_ci` remediation
/// from a check that isn't a real CI run (see the buildkite-log-access
/// investigation's coordination note).
pub(crate) fn classify_ci(leaves: &[serde_json::Value], combined_state: Option<&str>) -> OpenPrCiStatus {
    use std::collections::BTreeMap;

    const TRUNK_QUEUE_CHECK_NAME_PREFIX: &str = "Trunk Merge Queue";

    // Group by name, keeping the most-recently-seen leaf per name.
    // The rollup is ordered oldest-to-newest for same-name re-runs.
    let mut by_name: BTreeMap<String, &serde_json::Value> = BTreeMap::new();
    for leaf in leaves {
        // `isRequired` defaults to `true` when missing; only filter
        // out the explicit `false`.
        let required = leaf.get("isRequired").and_then(|v| v.as_bool()).unwrap_or(true);
        if !required {
            continue;
        }
        let name = leaf
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| leaf.get("context").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_owned();
        if name.is_empty() || name.starts_with(TRUNK_QUEUE_CHECK_NAME_PREFIX) {
            continue;
        }
        by_name.insert(name, leaf);
    }

    let mut failures: Vec<RequiredCheckFailure> = Vec::new();
    let mut any_in_flight = false;
    for (name, leaf) in by_name {
        match normalize_leaf(leaf) {
            LeafVerdict::Pass => {}
            LeafVerdict::InFlight => {
                any_in_flight = true;
            }
            LeafVerdict::Fail { conclusion } => {
                let target_url = leaf
                    .get("targetUrl")
                    .and_then(|v| v.as_str())
                    .or_else(|| leaf.get("detailsUrl").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .to_owned();
                let provider = provider_for_url(&target_url);
                let provider_job_id = parse_provider_job_id(provider, &target_url);
                failures.push(RequiredCheckFailure {
                    name,
                    conclusion,
                    target_url,
                    provider,
                    provider_job_id,
                });
            }
        }
    }

    // Fast-fail: a terminal required-check failure surfaces `Failing`
    // immediately, even while other required checks are still running.
    // `Fail` dominates `InFlight` for terminal failures (restoring the
    // pre-regression-fix behaviour). Anti-phantom protection lives in the
    // reconcile/withdraw path, not here — see `on_ci_in_flight_supersedes_failure`.
    if !failures.is_empty() {
        return OpenPrCiStatus::Failing { failures };
    }
    // Only InFlight when no terminal failure has occurred yet.
    if any_in_flight {
        return OpenPrCiStatus::InFlight;
    }
    // No check-run data in the rollup. Consult the legacy commit-status
    // combined state when available: "pending" means required status
    // contexts are configured in branch protection but haven't been
    // submitted yet (GitHub's web UI labels this "Expected"). Treat any
    // non-success combined state as InFlight so the kanban card shows a
    // waiting indicator instead of a false-positive green checkmark.
    if leaves.is_empty() {
        match combined_state.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("pending") | Some("failure") | Some("error") => {
                return OpenPrCiStatus::InFlight;
            }
            _ => {}
        }
    }
    OpenPrCiStatus::Clean
}

/// Per-PR opt-out label that suppresses every auto-remediation flow
/// (conflict resolution, auto-rebase, CI fixing) for a single PR.
/// Mirrors the auto-rebase design's Q8 string; this design extends
/// the same label to the conflict-watch path (Q7 / Phase 6 #18).
pub const OPT_OUT_LABEL: &str = "boss/no-auto-rebase";

/// True iff `labels` contains the unified opt-out label
/// ([`OPT_OUT_LABEL`]). Match is case-insensitive — GitHub labels are
/// case-preserving but the engine should tolerate casing drift the
/// user introduces.
pub fn pr_labels_opt_out(labels: &[String]) -> bool {
    labels.iter().any(|l| l.eq_ignore_ascii_case(OPT_OUT_LABEL))
}

/// Classification rules (design Q1):
///   - `state=MERGED` or non-empty `mergedAt` → `Merged`.
///   - `state=CLOSED` (and not merged) → `ClosedUnmerged`.
///   - `state=OPEN` (or unknown / empty, treated as still-open):
///
/// The merged / closed / open decision from `(state, mergedAt)` is
/// delegated to the shared [`crate::work::classify_pr_merge_state`]
/// helper (also used by the revision gate's `map_gh_state`); this
/// function layers the open-PR mergeability / CI axes on top:
///       * `mergeable=CONFLICTING` AND `mergeStateStatus=DIRTY` → `Conflict`
///       * `mergeable=UNKNOWN` → `Unknown` (GitHub is recomputing; skip conflict transitions)
///       * everything else → `Clean`.
///
/// The `ci` axis is supplied by the caller from [`classify_ci`] — both axes
/// share the `Open` wrapper.
///
/// The two-field agreement on `CONFLICTING` + `DIRTY` is deliberate —
/// either alone is the precise signal, but requiring both protects
/// against `mergeStateStatus` lagging behind `mergeable` immediately
/// after a base move.
///
/// `UNKNOWN` maps to `OpenPrMergeability::Unknown` rather than `Clean`
/// so the conflict-watch retire path does not fire on transient
/// recomputation windows (root cause of the blocked↔in_review flap —
/// a PR left genuinely CONFLICTING would briefly read UNKNOWN during a
/// base-move, trigger `on_resolved`, and then re-detect CONFLICTING on
/// the next sweep).
pub(crate) fn classify_state(
    raw_state: &str,
    merged_at: &str,
    mergeable: &str,
    merge_state_status: &str,
    ci: OpenPrCiStatus,
) -> PrLifecycleState {
    match crate::work::classify_pr_merge_state(raw_state, merged_at) {
        crate::work::PrMergeClass::Merged => return PrLifecycleState::Merged,
        crate::work::PrMergeClass::ClosedUnmerged => return PrLifecycleState::ClosedUnmerged,
        crate::work::PrMergeClass::Open => {}
    }
    let conflicting = mergeable.eq_ignore_ascii_case("CONFLICTING") && merge_state_status.eq_ignore_ascii_case("DIRTY");
    let mergeability = if conflicting {
        OpenPrMergeability::Conflict
    } else if mergeable.eq_ignore_ascii_case("UNKNOWN") {
        OpenPrMergeability::Unknown
    } else {
        OpenPrMergeability::Clean
    };
    PrLifecycleState::Open(OpenPrStatus { mergeability, ci })
}
