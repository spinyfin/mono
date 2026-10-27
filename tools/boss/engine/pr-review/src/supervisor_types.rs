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

fn join_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        "(none: no leaf reported any finding)".to_owned()
    } else {
        ids.iter().map(|id| format!("`{id}`")).collect::<Vec<_>>().join(", ")
    }
}

/// Which leaf reviewer produced a piece of evidence the supervisor is
/// attributing. Deliberately narrower than
/// [`boss_protocol::ReviewBatchMemberRole`] (which also has `Supervisor` and
/// `PostMergeReviewer` variants): a verdict can only ever attribute a claim to
/// a leaf that actually produced evidence. Grok remains deserializable for
/// historical batches; the applier validates citations against batch reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorSourceRole {
    Claude,
    Codex,
    /// Historical three-leaf batches only; never configured for a new batch.
    Grok,
}

impl SupervisorSourceRole {
    /// Parse a role name as written by [`Self::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "grok" => Some(Self::Grok),
            _ => None,
        }
    }

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
/// `sources` lists the ids of the leaf findings (`<role>-<n>`, see
/// [`crate::types::leaf_finding_id`]) the supervisor merged into this entry
/// after its semantic dedup — never empty, since a finding with no source is
/// unattributed evidence the supervisor may not have invented. Distinct
/// reviewers among the cited ids (`leaf_finding_role`) are the corroboration
/// signal: a defect two or three independent reviewers converged on, without
/// being told to.
///
/// A post-merge verdict has no leaf findings to cite; its `sources` is the
/// producing reviewer's role name instead.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
#[builder(on(String, into))]
pub struct ConsolidatedFinding {
    /// Stable id of this consolidated finding, unique within the verdict and
    /// carried into the rendered revision description. `None` only for a
    /// legacy verdict stored before ids existed; new submissions must set it
    /// (enforced by [`SupervisorVerdict::structural_errors`], since serde
    /// alone cannot require it without breaking legacy rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
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
    /// Leaf finding ids merged into this finding. Must be non-empty;
    /// validated at the proposal-validation layer since serde alone cannot
    /// express a non-empty-vec constraint. A legacy verdict instead lists
    /// bare role names here.
    pub sources: Vec<String>,
}

/// A leaf finding the supervisor considered and deliberately did not carry
/// into any consolidated finding. Every leaf finding must be either cited in
/// some [`ConsolidatedFinding::sources`] or listed here — an omission is a
/// validation failure, never an implicit rejection.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
#[builder(on(String, into))]
pub struct RejectedFinding {
    /// Id of the dropped leaf finding (`<role>-<n>`).
    pub leaf_finding_id: String,
    /// Why the supervisor dropped it — e.g. it re-read the cited file and the
    /// claim did not hold, or it was subsumed with no residual value.
    pub reason: String,
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
    /// Leaf findings the supervisor dropped, each with its reason. Together
    /// with the leaf ids cited in `findings[].sources` this must account for
    /// every reported leaf finding. Absent in a legacy verdict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub rejected: Vec<RejectedFinding>,
}

/// One failed id/disposition rule: the JSON path (relative to the verdict,
/// e.g. `findings[0].sources`) and a human-readable message the submitting
/// worker can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictIdError {
    pub path: String,
    pub message: String,
}

impl VerdictIdError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

const MAX_FINDING_ID_CHARS: usize = 64;

fn finding_id_is_well_formed(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_FINDING_ID_CHARS
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl SupervisorVerdict {
    /// Parse a verdict from the JSON body handed to `boss propose`.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Whether this verdict contains legacy findings: none carries an id
    /// and nothing is recorded as rejected. An empty verdict alone cannot
    /// establish its age. Legacy findings are never backfilled.
    pub fn is_legacy(&self) -> bool {
        !self.findings.is_empty()
            && self.rejected.is_empty()
            && self.findings.iter().all(|finding| finding.id.is_none())
    }

    /// Rules that need only the verdict itself, applied to every new
    /// submission: each consolidated finding has a well-formed id, ids are
    /// unique, `sources` is non-empty without repeats, and `rejected`
    /// entries are well-formed and not repeated.
    ///
    /// A post-merge verdict has no leaf reports, so its `sources` are
    /// producing-role names and `rejected` must be empty. For every other
    /// phase the cited ids are checked against real leaf findings by
    /// [`Self::accounting_errors`], which needs the batch's reports.
    pub fn structural_errors(&self) -> Vec<VerdictIdError> {
        let post_merge = self.phase == boss_protocol::ReviewBatchPhase::PostMerge;
        let mut errors = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        for (index, finding) in self.findings.iter().enumerate() {
            match finding.id.as_deref() {
                None => errors.push(VerdictIdError::new(
                    format!("findings[{index}].id"),
                    "is required: give every consolidated finding a unique id (e.g. `C1`)",
                )),
                Some(id) if !finding_id_is_well_formed(id) => errors.push(VerdictIdError::new(
                    format!("findings[{index}].id"),
                    format!(
                        "`{id}` is not a valid id: use 1-{MAX_FINDING_ID_CHARS} characters from letters, digits, `-`, `_`, `.`"
                    ),
                )),
                Some(id) if !seen_ids.insert(id) => errors.push(VerdictIdError::new(
                    format!("findings[{index}].id"),
                    format!("duplicate consolidated finding id `{id}`; ids must be unique within the verdict"),
                )),
                Some(_) => {}
            }
            if finding.sources.is_empty() {
                errors.push(VerdictIdError::new(
                    format!("findings[{index}].sources"),
                    "must cite at least one leaf finding id; a finding cannot be unattributed",
                ));
            }
            let mut seen_sources = std::collections::HashSet::new();
            for source in &finding.sources {
                if !seen_sources.insert(source.as_str()) {
                    errors.push(VerdictIdError::new(
                        format!("findings[{index}].sources"),
                        format!("cites `{source}` more than once"),
                    ));
                }
                if post_merge && SupervisorSourceRole::parse(source).is_none() {
                    errors.push(VerdictIdError::new(
                        format!("findings[{index}].sources"),
                        format!(
                            "`{source}` is not a reviewer role; a post-merge verdict cites its own role, e.g. `claude`"
                        ),
                    ));
                }
            }
        }
        let mut seen_rejected = std::collections::HashSet::new();
        for (index, rejected) in self.rejected.iter().enumerate() {
            if post_merge {
                errors.push(VerdictIdError::new(
                    format!("rejected[{index}]"),
                    "a post-merge verdict has no leaf findings to reject; `rejected` must be empty",
                ));
                continue;
            }
            if rejected.reason.trim().is_empty() {
                errors.push(VerdictIdError::new(
                    format!("rejected[{index}].reason"),
                    "must explain why the leaf finding was dropped",
                ));
            }
            if !seen_rejected.insert(rejected.leaf_finding_id.as_str()) {
                errors.push(VerdictIdError::new(
                    format!("rejected[{index}].leaf_finding_id"),
                    format!("`{}` is rejected more than once", rejected.leaf_finding_id),
                ));
            }
        }
        errors
    }

    /// Check this verdict's citations and dispositions against the leaf
    /// finding ids actually reported for the batch (`leaf_ids`).
    ///
    /// - every cited source and every rejected id must be a reported leaf id —
    ///   an unknown id is an error, never ignored;
    /// - a leaf finding must not be both cited and rejected;
    /// - every reported leaf finding must be cited by some consolidated
    ///   finding or listed in `rejected`. A finding the verdict simply omits is
    ///   an error the supervisor must correct — it is not auto-rejected.
    ///
    /// Not applicable to post-merge verdicts (no leaves); callers skip them.
    pub fn accounting_errors(&self, leaf_ids: &[String]) -> Vec<VerdictIdError> {
        let known: std::collections::HashSet<&str> = leaf_ids.iter().map(String::as_str).collect();
        let mut errors = Vec::new();
        let mut cited = std::collections::HashSet::new();
        for (index, finding) in self.findings.iter().enumerate() {
            for source in &finding.sources {
                if known.contains(source.as_str()) {
                    cited.insert(source.as_str());
                } else {
                    errors.push(VerdictIdError::new(
                        format!("findings[{index}].sources"),
                        format!(
                            "cites unknown leaf finding id `{source}`; reported leaf finding ids are: {}",
                            join_ids(leaf_ids)
                        ),
                    ));
                }
            }
        }
        let mut rejected_ids = std::collections::HashSet::new();
        for (index, rejected) in self.rejected.iter().enumerate() {
            let id = rejected.leaf_finding_id.as_str();
            if !known.contains(id) {
                errors.push(VerdictIdError::new(
                    format!("rejected[{index}].leaf_finding_id"),
                    format!(
                        "unknown leaf finding id `{id}`; reported leaf finding ids are: {}",
                        join_ids(leaf_ids)
                    ),
                ));
                continue;
            }
            if cited.contains(id) {
                errors.push(VerdictIdError::new(
                    format!("rejected[{index}].leaf_finding_id"),
                    format!("`{id}` is both cited in a consolidated finding's `sources` and listed in `rejected`"),
                ));
            }
            rejected_ids.insert(id);
        }
        let unaccounted: Vec<&str> = leaf_ids
            .iter()
            .map(String::as_str)
            .filter(|id| !cited.contains(id) && !rejected_ids.contains(id))
            .collect();
        if !unaccounted.is_empty() {
            errors.push(VerdictIdError::new(
                "rejected",
                format!(
                    "leaf finding(s) {} are neither cited in any consolidated finding's `sources` nor listed in \
                     `rejected`; account for every leaf finding by merging it into a finding or rejecting it with a reason",
                    unaccounted.iter().map(|id| format!("`{id}`")).collect::<Vec<_>>().join(", "),
                ),
            ));
        }
        errors
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
                id: finding.id.clone(),
                sources: finding.sources.clone(),
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
                "id": "C1",
                "severity": "high",
                "category": "correctness",
                "confidence": "high",
                "file": "src/lib.rs",
                "location": "fn handle, ~L42",
                "title": "Unchecked index into changed_files",
                "detail": "Both claude and codex independently flagged the same out-of-bounds read.",
                "sources": ["claude-1", "codex-1"]
            }],
            "rejected": [{"leaf_finding_id": "codex-2", "reason": "File re-read; the claim did not hold."}],
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

    fn leaf_ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn parsed_sample() -> SupervisorVerdict {
        SupervisorVerdict::from_json(&sample_verdict_json().to_string()).expect("valid verdict")
    }

    fn paths(errors: &[VerdictIdError]) -> Vec<String> {
        errors.iter().map(|error| error.path.clone()).collect()
    }

    #[test]
    fn a_fully_accounted_verdict_has_no_id_errors() {
        let verdict = parsed_sample();
        assert!(!verdict.is_legacy());
        assert_eq!(verdict.findings[0].id.as_deref(), Some("C1"));
        assert_eq!(verdict.rejected.len(), 1);
        assert!(verdict.structural_errors().is_empty());
        assert!(
            verdict
                .accounting_errors(&leaf_ids(&["claude-1", "codex-1", "codex-2"]))
                .is_empty()
        );
    }

    #[test]
    fn legacy_verdict_without_ids_or_rejected_still_loads() {
        let mut value = sample_verdict_json();
        value["findings"][0].as_object_mut().unwrap().remove("id");
        value["findings"][0]["sources"] = serde_json::json!(["claude", "codex"]);
        value.as_object_mut().unwrap().remove("rejected");
        let legacy = SupervisorVerdict::from_json(&value.to_string()).expect("legacy verdict loads");
        assert!(legacy.is_legacy());
        assert!(legacy.findings[0].id.is_none());
        assert!(legacy.rejected.is_empty());
        // Still projects for revision rendering; nothing is backfilled.
        let result = legacy.to_review_result();
        assert!(result.findings[0].id.is_none());
        // New submissions in the legacy shape fail structural validation.
        assert_eq!(paths(&legacy.structural_errors()), vec!["findings[0].id"]);
    }

    #[test]
    fn unknown_cited_leaf_id_is_rejected_loudly() {
        let errors = parsed_sample().accounting_errors(&leaf_ids(&["claude-1", "codex-2"]));
        assert!(errors.iter().any(|error| {
            error.path == "findings[0].sources" && error.message.contains("unknown leaf finding id `codex-1`")
        }));
    }

    #[test]
    fn unknown_rejected_leaf_id_is_rejected_loudly() {
        let errors = parsed_sample().accounting_errors(&leaf_ids(&["claude-1", "codex-1"]));
        assert!(errors.iter().any(|error| {
            error.path == "rejected[0].leaf_finding_id" && error.message.contains("unknown leaf finding id `codex-2`")
        }));
    }

    #[test]
    fn unaccounted_leaf_finding_is_a_validation_failure_not_an_implicit_rejection() {
        let errors = parsed_sample().accounting_errors(&leaf_ids(&["claude-1", "codex-1", "codex-2", "grok-1"]));
        let unaccounted = errors
            .iter()
            .find(|error| error.path == "rejected" && error.message.contains("`grok-1`"))
            .expect("unaccounted grok-1 must be reported");
        assert!(unaccounted.message.contains("neither cited"));
        assert_eq!(errors.len(), 1, "only the omission is wrong: {errors:?}");
    }

    #[test]
    fn a_leaf_finding_cannot_be_both_cited_and_rejected() {
        let mut verdict = parsed_sample();
        verdict.rejected[0].leaf_finding_id = "claude-1".to_owned();
        let errors = verdict.accounting_errors(&leaf_ids(&["claude-1", "codex-1"]));
        assert!(errors.iter().any(|error| error.message.contains("both cited")));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let mut verdict = parsed_sample();
        let mut second = verdict.findings[0].clone();
        second.title = "Another".to_owned();
        second.sources = vec!["claude-1".to_owned(), "claude-1".to_owned()];
        verdict.findings.push(second);
        verdict.rejected.push(verdict.rejected[0].clone());
        let errors = verdict.structural_errors();
        assert!(errors.iter().any(|error| {
            error.path == "findings[1].id" && error.message.contains("duplicate consolidated finding id `C1`")
        }));
        assert!(
            errors
                .iter()
                .any(|error| error.path == "findings[1].sources" && error.message.contains("more than once"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.path == "rejected[1].leaf_finding_id" && error.message.contains("more than once"))
        );
    }

    #[test]
    fn malformed_and_missing_structure_is_rejected() {
        let mut verdict = parsed_sample();
        verdict.findings[0].id = Some("has space".to_owned());
        verdict.findings[0].sources.clear();
        verdict.rejected[0].reason = "  ".to_owned();
        let errors = verdict.structural_errors();
        assert_eq!(
            paths(&errors),
            vec!["findings[0].id", "findings[0].sources", "rejected[0].reason"]
        );
    }

    #[test]
    fn post_merge_verdict_cites_roles_and_has_nothing_to_reject() {
        let mut value = sample_verdict_json();
        value["phase"] = serde_json::json!("post_merge");
        value["findings"][0]["sources"] = serde_json::json!(["claude"]);
        value.as_object_mut().unwrap().remove("rejected");
        let verdict = SupervisorVerdict::from_json(&value.to_string()).unwrap();
        assert!(verdict.structural_errors().is_empty());

        let mut leafy = verdict.clone();
        leafy.findings[0].sources = vec!["claude-1".to_owned()];
        leafy.rejected = parsed_sample().rejected;
        assert_eq!(
            paths(&leafy.structural_errors()),
            vec!["findings[0].sources", "rejected[0]"]
        );
    }

    #[test]
    fn consolidated_ids_and_sources_carry_into_the_rendered_revision_description() {
        let mut verdict = parsed_sample();
        let mut second = verdict.findings[0].clone();
        second.id = Some("C2".to_owned());
        second.title = "Second defect".to_owned();
        second.sources = vec!["codex-3".to_owned()];
        verdict.findings.push(second);
        let rendered = crate::render_revision_instructions(
            &verdict.to_review_result(),
            crate::ReviewOrigin { pr_number: Some(7) },
        );
        assert!(rendered.contains("**Finding id:** `C1`"), "{rendered}");
        assert!(rendered.contains("**Finding id:** `C2`"), "{rendered}");
        assert!(rendered.contains("**Sources:** `claude-1`, `codex-1`"), "{rendered}");
        assert!(rendered.contains("**Sources:** `codex-3`"), "{rendered}");
    }

    #[test]
    fn rendered_revision_description_for_a_legacy_verdict_has_no_id_lines() {
        let mut value = sample_verdict_json();
        value["findings"][0].as_object_mut().unwrap().remove("id");
        value["findings"][0]["sources"] = serde_json::json!([]);
        let legacy = SupervisorVerdict::from_json(&value.to_string()).unwrap();
        let rendered =
            crate::render_revision_instructions(&legacy.to_review_result(), crate::ReviewOrigin { pr_number: Some(7) });
        assert!(!rendered.contains("Finding id"));
        assert!(!rendered.contains("**Sources:**"));
    }
}
