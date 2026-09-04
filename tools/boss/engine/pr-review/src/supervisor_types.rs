//! Data model for the supervisor worker: the bounded structured verdict a
//! consolidating supervisor submits after reading all reported leaf reviews
//! for one review batch.
//!
//! Unlike a leaf's [`crate::types::ReviewerReport`], which reports raw
//! observations only, a [`SupervisorVerdict`] is the batch's one consolidated
//! outcome: semantically-deduplicated findings with source attribution across
//! the (up to three) leaves that raised them, plus an explicit record of any
//! contradictions between leaves and how the supervisor resolved them.

use serde::{Deserialize, Serialize};

use crate::types::{ReviewFindingCategory, ReviewFindingConfidence, ReviewFindingSeverity};

/// Which leaf reviewer produced a piece of evidence the supervisor is
/// attributing. Deliberately narrower than
/// [`boss_protocol::ReviewBatchMemberRole`] (which also has `Supervisor` and
/// `PostMergeReviewer` variants): a verdict can only ever attribute a claim to
/// one of the three leaves that actually produce evidence, so this schema
/// makes the invalid values unrepresentable rather than validating them away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorSourceRole {
    Claude,
    Codex,
    Grok,
}

impl SupervisorSourceRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }
}

impl std::fmt::Display for SupervisorSourceRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One consolidated finding in a [`SupervisorVerdict`].
///
/// `sources` names every leaf that independently raised this finding (after
/// the supervisor's semantic dedup collapsed near-duplicate reports of the
/// same underlying issue into one entry) — never empty, since a finding with
/// no source is unattributed evidence the supervisor may not have invented.
/// `sources.len() > 1` is itself the corroboration signal: a defect two or
/// three independent reviewers converged on, without being told to.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
#[builder(on(String, into))]
pub struct ConsolidatedFinding {
    pub severity: ReviewFindingSeverity,
    pub category: ReviewFindingCategory,
    pub confidence: ReviewFindingConfidence,
    /// File path relative to the repo root.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Short, scannable title (≤ 80 chars).
    pub title: String,
    /// The supervisor's own consolidated description of the defect and what
    /// to change — reconciled from the contributing leaves' reports, not a
    /// copy-paste of one of them.
    pub detail: String,
    /// Every leaf that independently raised this finding. Must be non-empty;
    /// validated at the proposal-validation layer since serde alone cannot
    /// express a non-empty-vec constraint.
    pub sources: Vec<SupervisorSourceRole>,
}

/// One leaf's claim about a contested point, as recorded in a
/// [`SupervisorContradiction`].
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
#[builder(on(String, into))]
pub struct ReviewerPosition {
    pub role: SupervisorSourceRole,
    /// What this leaf claimed, in its own terms (paraphrase is fine — this
    /// is not required to be a verbatim quote from the leaf's report).
    pub claim: String,
}

/// A recorded disagreement between two or more leaves about the same file or
/// claim, and how the supervisor resolved it.
///
/// Existing to make contradiction-handling an explicit, reviewable act rather
/// than a silent pick-one: every entry names both (or all) positions before
/// stating the resolution, so a human reading the verdict can see what was
/// actually in dispute.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
#[builder(on(String, into))]
pub struct SupervisorContradiction {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// What the leaves disagree about.
    pub description: String,
    /// At least two independent positions — validated at the
    /// proposal-validation layer, same as `ConsolidatedFinding::sources`.
    pub positions: Vec<ReviewerPosition>,
    /// How the supervisor resolved the disagreement and why — e.g. it
    /// independently re-read the cited file and confirmed one side, or
    /// neither claim held up under inspection and the point was dropped.
    pub resolution: String,
    /// The leaf whose position the supervisor adopted, when the resolution
    /// sided with one of them. `None` when the resolution is "neither claim
    /// held up" / "genuinely inconclusive".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_in_favor_of: Option<SupervisorSourceRole>,
}

/// Structured verdict submitted by the supervisor of a persisted review
/// batch, via `boss propose review-verdict`.
///
/// Verdict *application* (deciding what happens to the reviewed work item —
/// creating a revision, advancing to human review, etc.) is deliberately out
/// of scope for this type and for the engine code that accepts it: accepting
/// this schema only records the batch's consolidated outcome durably. What a
/// `revision_warranted = true` verdict actually triggers is a later, separate
/// piece of work.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
#[builder(on(String, into))]
pub struct SupervisorVerdict {
    pub batch_id: String,
    pub pr_url: String,
    pub target_sha: String,
    pub phase: boss_protocol::ReviewBatchPhase,
    /// One-paragraph overall assessment, written for a human who will not
    /// read the three leaf reports individually.
    pub summary: String,
    /// Whether the consolidated findings warrant a revision. Mirrors
    /// [`crate::types::ReviewResult::revision_warranted`]'s semantics: set to
    /// `true` when at least one finding is `critical`/`high` severity, or any
    /// finding is `category: "regression"`, `"duplication"`,
    /// `"deferred_scope"`, or `"agent_isms"`, regardless of severity.
    pub revision_warranted: bool,
    /// Consolidated, deduplicated, source-attributed findings — never a raw
    /// concatenation of the leaves' own finding lists.
    pub findings: Vec<ConsolidatedFinding>,
    /// Disagreements between leaves and how each was resolved. Empty when
    /// the leaves' reports did not conflict.
    #[serde(default)]
    pub contradictions: Vec<SupervisorContradiction>,
}

impl SupervisorVerdict {
    /// Parse a verdict from the JSON body handed to `boss propose`.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Project this consolidated verdict into the legacy [`crate::types::ReviewResult`]
    /// shape so the existing severity gate and revision-instruction renderer
    /// can run without a parallel implementation.
    pub fn to_review_result(&self) -> crate::types::ReviewResult {
        let findings = self
            .findings
            .iter()
            .map(|finding| crate::types::ReviewFinding {
                severity: finding.severity.clone(),
                category: finding.category.clone(),
                file: finding.file.clone(),
                location: finding.location.clone(),
                title: finding.title.clone(),
                detail: finding.detail.clone(),
                confidence: finding.confidence.clone(),
            })
            .collect::<Vec<_>>();
        let suspected_deletions = findings
            .iter()
            .filter(|finding| matches!(finding.category, crate::types::ReviewFindingCategory::Regression))
            .cloned()
            .collect();
        crate::types::ReviewResult {
            pr_url: self.pr_url.clone(),
            head_sha: self.target_sha.clone(),
            summary: self.summary.clone(),
            revision_warranted: self.revision_warranted,
            findings,
            regression_check: crate::types::RegressionCheck {
                performed: true,
                suspected_deletions,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_verdict_json() -> serde_json::Value {
        serde_json::json!({
            "batch_id": "rvb_1",
            "pr_url": "https://github.com/org/repo/pull/7",
            "target_sha": "head_7",
            "phase": "pre_merge",
            "summary": "One corroborated correctness defect; a claimed regression did not hold up.",
            "revision_warranted": true,
            "findings": [{
                "severity": "high",
                "category": "correctness",
                "confidence": "high",
                "file": "src/lib.rs",
                "location": "fn handle, ~L42",
                "title": "Unchecked index into changed_files",
                "detail": "Both claude and codex independently flagged the same out-of-bounds read.",
                "sources": ["claude", "codex"]
            }],
            "contradictions": [{
                "file": "src/auth.rs",
                "description": "grok claimed a regression; codex disputed it",
                "positions": [
                    {"role": "grok", "claim": "the timeout check was removed"},
                    {"role": "codex", "claim": "the check moved into the caller, not removed"}
                ],
                "resolution": "Re-read src/auth.rs at the PR head: the check is present in the caller. No regression.",
                "resolved_in_favor_of": "codex"
            }]
        })
    }

    #[test]
    fn supervisor_verdict_projects_into_a_review_result_for_the_severity_gate() {
        let parsed = SupervisorVerdict::from_json(&sample_verdict_json().to_string()).expect("valid verdict");
        let result = parsed.to_review_result();
        assert_eq!(result.pr_url, parsed.pr_url);
        assert_eq!(result.head_sha, parsed.target_sha);
        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].title, "Unchecked index into changed_files");
        assert!(crate::passes_severity_gate(&result));
    }

    #[test]
    fn supervisor_verdict_round_trips_through_json() {
        let json = sample_verdict_json().to_string();
        let parsed = SupervisorVerdict::from_json(&json).expect("valid verdict");
        assert_eq!(parsed.batch_id, "rvb_1");
        assert!(parsed.revision_warranted);
        assert_eq!(parsed.findings.len(), 1);
        assert_eq!(parsed.findings[0].sources.len(), 2);
        assert_eq!(parsed.contradictions.len(), 1);
        assert_eq!(parsed.contradictions[0].positions.len(), 2);
        assert_eq!(
            parsed.contradictions[0].resolved_in_favor_of,
            Some(SupervisorSourceRole::Codex)
        );
    }

    #[test]
    fn supervisor_verdict_rejects_unknown_fields() {
        let mut value = sample_verdict_json();
        value
            .as_object_mut()
            .unwrap()
            .insert("extra_field".to_owned(), serde_json::json!("nope"));
        assert!(SupervisorVerdict::from_json(&value.to_string()).is_err());
    }

    #[test]
    fn supervisor_verdict_contradictions_default_to_empty() {
        let mut value = sample_verdict_json();
        value.as_object_mut().unwrap().remove("contradictions");
        let parsed = SupervisorVerdict::from_json(&value.to_string()).expect("valid verdict");
        assert!(parsed.contradictions.is_empty());
    }

    #[test]
    fn source_role_round_trips_as_snake_case() {
        for (role, expected) in [
            (SupervisorSourceRole::Claude, "\"claude\""),
            (SupervisorSourceRole::Codex, "\"codex\""),
            (SupervisorSourceRole::Grok, "\"grok\""),
        ] {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, expected);
        }
    }
}
