//! Data model for the reviewer worker: PR context, review scope, findings,
//! and the `ReviewResult` structured output (design §2/§3 of P992). See
//! [`super`] for the module role and output contract.

use serde::{Deserialize, Serialize};

/// Authoritative PR metadata fetched from GitHub at reviewer-spawn time.
///
/// Pre-fetched before the reviewer worker starts so the reviewer prompt
/// contains the correct base/head SHAs and changed-file list upfront —
/// orientation context the reviewer receives before touching any files.
/// The reviewer workspace is already checked out to the PR head, so file
/// reads go directly against the working tree.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct PrReviewContext {
    /// GitHub PR number (the integer, e.g. `42` for `.../pull/42`).
    pub pr_number: u64,
    /// Full SHA of the base commit the PR is diffed against.
    pub base_sha: String,
    /// Full SHA of the PR's current HEAD commit. The reviewer workspace is
    /// checked out to this SHA, so `Read`/`cat`/`grep` on workspace files
    /// reflect the PR head directly.
    pub head_sha: String,
    /// Paths of every file changed by the PR, relative to the repo root.
    pub changed_files: Vec<String>,
    /// Full output of `gh pr diff` when the diff fits within the configured
    /// line threshold. `None` when the diff was too large to embed or could
    /// not be fetched. When `Some`, the reviewer prompt embeds it directly
    /// so the reviewer skips the `gh pr diff` tool call.
    pub diff_content: Option<String>,
    /// HEAD SHA this PR was at the end of the most recent completed
    /// reviewer pass, or `None` on a PR's first review (2026-07-01
    /// revision-review experiment). Lets the reviewer prioritise the delta
    /// since that pass — content it already accepted doesn't need
    /// re-litigating — while still reviewing the PR as a whole; see the
    /// prompt's "Reviewing a revision" section, which explicitly overrides
    /// diff-only scoping for whole-PR-state defects (e.g. a duplicated
    /// module tree spread across a diff's unchanged and changed hunks).
    pub last_reviewed_sha: Option<String>,
    /// Deterministic supersession-language hits found in the PR body, commit
    /// messages, or comments (incident-002 P3). Each entry is a rendered
    /// `term — snippet` line. When non-empty the reviewer prompt carries an
    /// authoritative block requiring a verified design-doc citation for each
    /// flagged claim. Populated by the caller (the spawn path scans the PR
    /// narrative); empty here by default.
    #[builder(default)]
    pub supersession_flags: Vec<String>,
    /// Deterministic both-parents deletion-tripwire result (incident-002 P2):
    /// files a merged parent added and this forward-port/merge resolution
    /// removed, rename/move-aware. Each entry is a rendered description line.
    /// When non-empty the reviewer prompt carries an authoritative,
    /// rationale-independent block: each removed surface must be raised as a
    /// gating `regression` finding unless a design-doc citation authorises it.
    /// Populated by the caller for conflict-resolution reviews; empty here by
    /// default.
    #[builder(default)]
    pub merged_parent_deletions: Vec<String>,
    /// Deterministic bare Boss work-item id (`T<n>`/`P<n>`) sweep hits from
    /// the PR's title, description, and added diff lines — a mechanical
    /// assist for the agent-isms "Boss-construct references" sub-rule. Each
    /// entry is a rendered `` `id` at location — "snippet" `` line. When
    /// non-empty the reviewer prompt carries a forced-disposition block
    /// requiring each hit be flagged or explicitly dismissed. Populated by
    /// the caller; empty here by default.
    #[builder(default)]
    pub boss_construct_refs: Vec<String>,
}

/// Which review rubric to apply to a PR.
///
/// The reviewer renders a scope-specific initial prompt so the rubric is
/// always appropriate to the PR's content — the code rubric (correctness,
/// regressions, architecture, tests) does not apply to a pure docs delivery.
///
/// Callers should use [`classify_changed_files`] to derive this from the
/// list of files in the PR diff, falling back to [`ReviewScope::Code`] when
/// the file list is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewScope {
    /// Standard code PR — apply the full code rubric.
    Code,
    /// Docs-only PR (every changed file is a documentation file) — apply
    /// the light rubric (structure, completeness, required-sections) and
    /// skip the code rubric entirely.
    DocsOnly,
}

/// Severity of a review finding — drives the engine's revision-warrant
/// decision (design §3: any `critical`/`high`, or any `regression`,
/// creates a revision regardless of `revision_warranted`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingSeverity {
    Critical,
    High,
    Medium,
    Low,
}

impl ReviewFindingSeverity {
    /// Return the string label used in the prompt schema description.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// What kind of issue a finding represents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingCategory {
    /// Logic error, broken invariant, race condition, mishandled error.
    Correctness,
    /// Inadvertent deletion/regression — code dropped that was not the
    /// purpose of the PR (the T793 check class).
    Regression,
    /// Wrong layer, missed reuse, abstraction fights codebase conventions.
    Architecture,
    /// Style, naming, dead/confusing code, fails to match surroundings.
    Readability,
    /// Untested new behaviour, missing edge-case test.
    Tests,
    /// Boundary condition, nullability, concurrency, failure mode.
    #[serde(rename = "edgecase")]
    EdgeCase,
    /// New hand-rolled infrastructure (HTTP/API client, external-service
    /// wiring — endpoints, auth headers, version constants —, serialization
    /// helper, retry/backoff logic, utility module) that reimplements an
    /// equivalent already present elsewhere in the repo, instead of reusing
    /// or extracting a shared module. Forces a revision regardless of
    /// assigned severity (see [`passes_severity_gate`]) — the same
    /// treatment as [`Self::Regression`].
    Duplication,
    /// Undeclared deferral (brief asked for scope the diff doesn't deliver
    /// and no `[deferred-scope]` marker covers it), misdeclared deferral
    /// (prose "## Deferred" / "out of scope" language with no matching
    /// marker), or a malformed `[deferred-scope]` marker. Forces a revision
    /// regardless of assigned severity (see [`passes_severity_gate`]) — the
    /// same treatment as [`Self::Regression`] and [`Self::Duplication`].
    ///
    /// Does NOT apply to a deferred item that is manual, interactive, or
    /// display-requiring verification a headless worker cannot perform
    /// (live GUI runs, "spawn real workers and watch the app",
    /// screenshot-based checks, physical-device tests) — see the "Exception"
    /// paragraph in the code rubric ([`render_rubric_section`]). That carve-out
    /// is narrow: it covers infeasibility-for-a-headless-agent, not deferrals
    /// of work an agent could actually do.
    #[serde(rename = "deferred_scope")]
    DeferredScope,
    /// A code comment, PR title, or PR description that only makes sense to
    /// the agent that wrote it. In a code comment: narrates the historical
    /// lineage of a change ("we used to do X, but removed it because Y")
    /// instead of describing the current state of the code, references a
    /// Boss construct (a work item id, phase, chore, brief, or effort level
    /// — e.g. "implements T234 phase 7"), or refers to the human directing
    /// Boss as "the operator" or to actors in general instead of stating the
    /// underlying reason directly. In a PR title/description: references a
    /// Boss construct or "the operator"/actor framing, same as above —
    /// **except** historical/narrative context ("previously X, this PR makes
    /// it Y") is expected and exempt in PR descriptions, since that is a
    /// description's normal job; the lineage restriction applies to code
    /// comments only. Forces a revision regardless of assigned severity (see
    /// [`passes_severity_gate`]) — the same treatment as [`Self::Regression`],
    /// [`Self::Duplication`], and [`Self::DeferredScope`].
    #[serde(rename = "agent_isms")]
    AgentIsms,
}

/// Reviewer's confidence in a finding.
///
/// `Low` means "suggestion — apply at the revising worker's discretion;
/// this alone does not warrant a revision cycle".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingConfidence {
    High,
    Medium,
    Low,
}

/// A single actionable review finding.
///
/// Every finding must name a file and state concretely what to change.
/// Vague findings ("consider improving error handling") are not acceptable.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewFinding {
    pub severity: ReviewFindingSeverity,
    pub category: ReviewFindingCategory,
    /// File path relative to the repo root.
    pub file: String,
    /// Best-effort location within the file (function name, ~line, hunk).
    /// `None` when the finding applies to the file as a whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Short, scannable title (≤ 80 chars).
    pub title: String,
    /// Concrete description of the problem **and** what to change. Must be
    /// specific enough that the revising worker can act without guessing.
    pub detail: String,
    pub confidence: ReviewFindingConfidence,
}

/// First-class regression/deletion check — always present in `ReviewResult`.
///
/// `performed` must be `true`; the reviewer cannot skip the deletion check.
/// If it finds no regressions, it returns `suspected_deletions: []` with
/// `performed: true`.
///
/// `Default` (performed=false, suspected_deletions=[]) exists solely for
/// `#[serde(default)]` on `ReviewResult::regression_check`; it is not a
/// meaningful state for normal use.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RegressionCheck {
    /// Always `true`. The reviewer must always perform the deletion check.
    pub performed: bool,
    /// All `category = "regression"` findings extracted from `findings`.
    ///
    /// This field is **derived** by [`ReviewResult::from_json`] from
    /// `findings` entries where `category == Regression` and is never read
    /// from the JSON supplied by the reviewer (the reviewer always writes
    /// `suspected_deletions: []`). Skipping deserialization prevents a
    /// type-mismatch serde error when the reviewer fills the field with
    /// descriptive strings instead of `ReviewFinding` objects.
    #[serde(skip_deserializing, default)]
    pub suspected_deletions: Vec<ReviewFinding>,
}

/// Structured output emitted by the reviewer worker in a ```json fenced block
/// in its final message (design §3).
///
/// The engine completion handler extracts this JSON from the final message,
/// parses it, and applies the severity gate to decide whether to create a
/// revision on the producing task. `revision_warranted = false` with no
/// qualifying findings → PR proceeds to human Review unimpeded.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewResult {
    /// URL of the PR that was reviewed.
    pub pr_url: String,
    /// HEAD SHA of the PR at review time — used for no-op detection (design
    /// §8) and to guard against racing updates. Best-effort; the engine
    /// tolerates an empty string if `gh pr view` did not supply one.
    pub head_sha: String,
    /// One-paragraph overall assessment of the PR.
    pub summary: String,
    /// Whether the reviewer believes a revision is warranted. The engine
    /// additionally gates on its own severity threshold (any critical/high
    /// or any regression finding forces a revision regardless of this flag).
    pub revision_warranted: bool,
    /// All findings, ordered by severity (critical first, low last).
    pub findings: Vec<ReviewFinding>,
    /// First-class regression/deletion check — always present in well-formed
    /// reviewer output. Defaults to `{performed:false, suspected_deletions:[]}`
    /// if the field is absent or uses a different key (e.g. an older model
    /// using `deletion_check`) so the overall `ReviewResult` can still be
    /// extracted and the `findings`/`revision_warranted` fields honoured.
    #[serde(default)]
    pub regression_check: RegressionCheck,
}

impl ReviewResult {
    /// Parse a `ReviewResult` from a JSON string.
    ///
    /// After deserialization, `regression_check.suspected_deletions` is
    /// populated from `findings` entries where `category == Regression` so
    /// the field is always consistent with `findings` regardless of what the
    /// reviewer wrote in the JSON (the reviewer always emits `suspected_deletions: []`).
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let mut result: Self = serde_json::from_str(json)?;
        result.regression_check.suspected_deletions = result
            .findings
            .iter()
            .filter(|f| matches!(f.category, ReviewFindingCategory::Regression))
            .cloned()
            .collect();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn review_result_roundtrips_through_json() {
        let result = ReviewResult {
            pr_url: "https://github.com/org/repo/pull/42".to_owned(),
            head_sha: "abc123def456".to_owned(),
            summary: "Overall the PR looks good with one regression.".to_owned(),
            revision_warranted: true,
            findings: vec![ReviewFinding {
                severity: ReviewFindingSeverity::High,
                category: ReviewFindingCategory::Regression,
                file: "tools/boss/engine/src/lib.rs".to_owned(),
                location: Some("fn init, ~L10".to_owned()),
                title: "Forward-port dropped the autostart feature".to_owned(),
                detail: "The autostart flag was removed in the conflict resolution; \
                         restore it."
                    .to_owned(),
                confidence: ReviewFindingConfidence::High,
            }],
            regression_check: RegressionCheck {
                performed: true,
                suspected_deletions: vec![],
            },
        };
        let json = serde_json::to_string(&result).expect("serializes");
        let parsed = ReviewResult::from_json(&json).expect("deserializes");
        assert_eq!(parsed.pr_url, result.pr_url);
        assert_eq!(parsed.head_sha, result.head_sha);
        assert!(parsed.revision_warranted);
        assert_eq!(parsed.findings.len(), 1);
        assert_eq!(parsed.findings[0].severity, ReviewFindingSeverity::High);
        assert_eq!(parsed.findings[0].category, ReviewFindingCategory::Regression);
        assert!(parsed.regression_check.performed);
    }

    #[test]
    fn review_result_empty_findings_deserializes() {
        let json = serde_json::json!({
            "pr_url": "https://github.com/org/repo/pull/7",
            "head_sha": "",
            "summary": "LGTM, no issues found.",
            "revision_warranted": false,
            "findings": [],
            "regression_check": {
                "performed": true,
                "suspected_deletions": []
            }
        });
        let result = ReviewResult::from_json(&json.to_string()).expect("deserializes");
        assert!(!result.revision_warranted);
        assert!(result.findings.is_empty());
        assert!(result.regression_check.performed);
    }

    #[test]
    fn review_finding_severity_roundtrips_as_snake_case() {
        for (sev, expected) in [
            (ReviewFindingSeverity::Critical, "\"critical\""),
            (ReviewFindingSeverity::High, "\"high\""),
            (ReviewFindingSeverity::Medium, "\"medium\""),
            (ReviewFindingSeverity::Low, "\"low\""),
        ] {
            let json = serde_json::to_string(&sev).unwrap();
            assert_eq!(json, expected);
            let back: ReviewFindingSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(back, sev);
        }
    }

    #[test]
    fn review_finding_category_roundtrips_as_snake_case() {
        for (cat, expected) in [
            (ReviewFindingCategory::Correctness, "\"correctness\""),
            (ReviewFindingCategory::Regression, "\"regression\""),
            (ReviewFindingCategory::Architecture, "\"architecture\""),
            (ReviewFindingCategory::Readability, "\"readability\""),
            (ReviewFindingCategory::Tests, "\"tests\""),
            (ReviewFindingCategory::EdgeCase, "\"edgecase\""),
            (ReviewFindingCategory::Duplication, "\"duplication\""),
            (ReviewFindingCategory::DeferredScope, "\"deferred_scope\""),
            (ReviewFindingCategory::AgentIsms, "\"agent_isms\""),
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            assert_eq!(json, expected);
            let back: ReviewFindingCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cat);
        }
    }

    /// A `ReviewResult` that omits `regression_check` entirely (e.g., a model
    /// that used the old `deletion_check` key) must still parse successfully.
    /// The `findings`/`revision_warranted` fields carry all the information the
    /// engine needs; silently discarding the whole review for a missing optional
    /// field is the T1359 failure mode.
    #[test]
    fn review_result_parses_without_regression_check_field() {
        let json = serde_json::json!({
            "pr_url": "https://github.com/org/repo/pull/1",
            "head_sha": "abc",
            "summary": "Found a bug.",
            "revision_warranted": true,
            "findings": [
                {
                    "severity": "high",
                    "category": "correctness",
                    "file": "src/lib.rs",
                    "title": "Orphan tag leak",
                    "detail": "The tag is created before the push check.",
                    "confidence": "high"
                }
            ]
            // regression_check intentionally omitted — T1359 failure mode
        });
        let result = ReviewResult::from_json(&json.to_string())
            .expect("ReviewResult must parse even without regression_check (T1359 robustness)");
        assert!(result.revision_warranted, "revision_warranted must be preserved");
        assert_eq!(result.findings.len(), 1, "findings must be preserved");
        assert!(
            !result.regression_check.performed,
            "missing regression_check defaults to performed=false"
        );
    }

    #[test]
    fn regression_check_performed_must_be_true_in_valid_result() {
        let json = serde_json::json!({
            "pr_url": "https://github.com/org/repo/pull/1",
            "head_sha": "abc",
            "summary": "ok",
            "revision_warranted": false,
            "findings": [],
            "regression_check": {
                "performed": true,
                "suspected_deletions": []
            }
        });
        let result = ReviewResult::from_json(&json.to_string()).unwrap();
        assert!(result.regression_check.performed);
    }

    /// Regression fixture for T1687/PR#1497: reviewer correctly identifies a
    /// regression but fills `suspected_deletions` with descriptive strings
    /// instead of `ReviewFinding` objects (because the prompt schema never
    /// showed the element shape). Previously `serde_json::from_str` rejected
    /// the ENTIRE `ReviewResult` with "invalid type: string, expected struct
    /// ReviewFinding", silently dropping the finding.
    ///
    /// After the fix (`#[serde(skip_deserializing)]` on `suspected_deletions`
    /// plus derivation from `findings` in `from_json`) the JSON must parse and
    /// `passes_severity_gate` must fire.
    #[test]
    fn suspected_deletions_string_array_accepted_and_derived_from_findings() {
        let json = serde_json::json!({
            "pr_url": "https://github.com/org/repo/pull/42",
            "head_sha": "abc123",
            "summary": "Found a regression — config exclude rule removed.",
            "revision_warranted": true,
            "findings": [
                {
                    "severity": "high",
                    "category": "regression",
                    "file": "CHECKS.yaml",
                    "title": "Config exclude rule dropped without replacement",
                    "detail": "The config_dir-scoped exclude_files rule was removed.",
                    "confidence": "high"
                }
            ],
            "regression_check": {
                "performed": true,
                // Reviewer emitted a string array — the T1687 shape that
                // previously caused a serde type-mismatch rejection.
                "suspected_deletions": [
                    "config_dir-scoped exclude_files matching removed without replacement"
                ]
            }
        })
        .to_string();

        let result = ReviewResult::from_json(&json)
            .expect("ReviewResult with string-array suspected_deletions must parse (T1687 fix)");
        assert!(result.revision_warranted, "revision_warranted must be preserved");
        assert_eq!(result.findings.len(), 1, "finding must be preserved");
        assert!(
            passes_severity_gate(&result),
            "high-severity regression must pass the severity gate",
        );
        // Engine derives suspected_deletions from the regression finding.
        assert_eq!(
            result.regression_check.suspected_deletions.len(),
            1,
            "engine must derive one suspected_deletion from the regression finding",
        );
        assert_eq!(
            result.regression_check.suspected_deletions[0].title,
            "Config exclude rule dropped without replacement",
        );
    }

    /// Deriving `suspected_deletions` from `findings` must work when there are
    /// no regression-category findings — the field stays empty.
    #[test]
    fn suspected_deletions_empty_when_no_regression_findings() {
        let json = serde_json::json!({
            "pr_url": "https://github.com/org/repo/pull/1",
            "head_sha": "abc",
            "summary": "ok",
            "revision_warranted": false,
            "findings": [
                {
                    "severity": "medium",
                    "category": "readability",
                    "file": "a.rs",
                    "title": "style nit",
                    "detail": "consider renaming",
                    "confidence": "low"
                }
            ],
            "regression_check": {"performed": true, "suspected_deletions": []}
        })
        .to_string();

        let result = ReviewResult::from_json(&json).expect("parses");
        assert!(
            result.regression_check.suspected_deletions.is_empty(),
            "no regression findings → suspected_deletions must be empty",
        );
    }
}
