//! Evidence a merge-conflict worker must supply before it may stop on
//! `architectural_mismatch`, and the fixed vocabulary of the question the
//! engine raises from it.
//!
//! The bar for stopping is deliberately very high. Porting a change onto a
//! refactored base (files moved or deleted, modules or crates split or
//! renamed, helpers that became private, migrations that must move to another
//! crate's sequence, every commit conflicting) is ordinary conflict work, not
//! grounds to stop. The one situation that qualifies is a *behavioural* one:
//! after a genuine attempt to port the change onto the new layout, main has no
//! faithful place for the PR's behaviour (for example, the PR extends a
//! subsystem that main deleted outright) or main's new behaviour contradicts
//! the PR's intent.

use serde::{Deserialize, Serialize};

/// Worker-facing `mark-failed` reason: the upstream change already did what
/// this PR was trying to do.
pub const CONFLICT_OBSOLESCENCE_SUSPECTED_REASON: &str = "obsolescence_suspected";
/// Worker-facing `mark-failed` reason: the conflict needs a human choice
/// between two valid resolutions.
pub const CONFLICT_PRODUCT_DECISION_REQUIRED_REASON: &str = "product_decision_required";
/// The `mark-failed` reason that raises the question.
pub const CONFLICT_ARCHITECTURAL_MISMATCH_REASON: &str = "architectural_mismatch";

/// The closed set of reasons a worker may pass to
/// `boss engine conflicts mark-failed`. Engine-internal reasons (for example
/// `no_push_no_stop_condition`) are not on this list: they go through
/// `WorkDb::mark_conflict_resolution_failed` directly.
pub const CONFLICT_MARK_FAILED_REASONS: [&str; 3] = [
    CONFLICT_OBSOLESCENCE_SUSPECTED_REASON,
    CONFLICT_PRODUCT_DECISION_REQUIRED_REASON,
    CONFLICT_ARCHITECTURAL_MISMATCH_REASON,
];

/// Whether `reason` is one of [`CONFLICT_MARK_FAILED_REASONS`]. The error
/// text is shown to the worker, so it names the allowed reasons and the
/// same porting guidance as [`ArchitecturalMismatchEvidence::validate`].
pub fn validate_conflict_mark_failed_reason(reason: &str) -> Result<(), String> {
    if CONFLICT_MARK_FAILED_REASONS.contains(&reason) {
        Ok(())
    } else {
        Err(format!(
            "`{reason}` is not an allowed mark-failed reason. Use `{CONFLICT_OBSOLESCENCE_SUSPECTED_REASON}`, \
             `{CONFLICT_PRODUCT_DECISION_REQUIRED_REASON}`, or `{CONFLICT_ARCHITECTURAL_MISMATCH_REASON}`. {GUIDANCE}"
        ))
    }
}

/// Answer that rewrites the PR onto the new base as a port-sized revision.
pub const CONFLICT_MISMATCH_CHOICE_REWRITE: &str = "Rewrite onto the new base";
/// Answer that re-dispatches the conflict revision to resolve in place.
pub const CONFLICT_MISMATCH_CHOICE_FORCE_MERGE: &str = "Force a merge";

/// Longest accepted free-text field, in characters.
const MAX_FIELD_CHARS: usize = 2000;
/// Longest accepted single-sentence field (`contradiction`, `recommendation`).
const MAX_SENTENCE_CHARS: usize = 400;
/// Fewest words a contradiction needs to be a sentence rather than a label.
const MIN_CONTRADICTION_WORDS: usize = 6;

/// Phrases that describe a refactor or an amount of work, never a behavioural
/// contradiction. A contradiction or recommendation that leans on one is
/// rejected: those cases are normal conflict work, and a larger-than-sized
/// port is handled by the effort-escalation marker.
const REFACTOR_ONLY_PHRASES: &[&str] = &[
    "too many conflict",
    "many conflicts",
    "lots of conflict",
    "every commit conflict",
    "all commits conflict",
    "all of the commits conflict",
    "files moved",
    "file moved",
    "moved files",
    "files were moved",
    "file was moved",
    "files have moved",
    "was moved",
    "were moved",
    "been moved",
    "renamed",
    "was split",
    "were split",
    "split into",
    "became private",
    "now private",
    "no longer public",
    "behind a new crate",
    "new crate boundary",
    "large rewrite",
    "big rewrite",
    "major rewrite",
    "too large",
    "too big",
    "too much work",
    "too much effort",
    "large refactor",
    "big refactor",
];

/// Why the worker believes main has no faithful place for the PR's behaviour.
/// A closed set: a subsystem that was moved, split or renamed is not on it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MismatchBasis {
    /// The PR adds or extends behaviour in a subsystem that main deleted.
    SubsystemDeleted,
    /// Main redesigned the concept the PR changes, so no faithful place for
    /// the PR's behaviour remains.
    ConceptRedesigned,
    /// Main's new behaviour directly contradicts the PR's intent, so a product
    /// decision is needed.
    BehaviourContradiction,
}

impl MismatchBasis {
    pub const ALL: [Self; 3] = [
        Self::SubsystemDeleted,
        Self::ConceptRedesigned,
        Self::BehaviourContradiction,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SubsystemDeleted => "subsystem_deleted",
            Self::ConceptRedesigned => "concept_redesigned",
            Self::BehaviourContradiction => "behaviour_contradiction",
        }
    }

    /// Plain-language label for the question card.
    pub fn describe(self) -> &'static str {
        match self {
            Self::SubsystemDeleted => "main deleted the subsystem this change extends",
            Self::ConceptRedesigned => "main redesigned the concept this change modifies",
            Self::BehaviourContradiction => "main's new behaviour contradicts this change's intent",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|basis| basis.as_str() == raw.trim())
    }
}

/// What the conflict worker must report for `architectural_mismatch`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
#[serde(deny_unknown_fields)]
pub struct ArchitecturalMismatchEvidence {
    pub basis: MismatchBasis,
    /// The commits of the PR that conflict.
    pub conflicting_commits: String,
    /// The files or symbols that moved or were deleted on main, and the PRs
    /// that changed them.
    pub main_changes: String,
    /// What the worker did to port the change onto main's new layout before
    /// concluding it could not be done faithfully.
    pub attempted_port: String,
    /// The specific behavioural contradiction, in one sentence.
    pub contradiction: String,
    /// One-line recommendation between rewriting and forcing a merge.
    pub recommendation: String,
}

impl ArchitecturalMismatchEvidence {
    /// Whether the evidence clears the bar. The error text is shown to the
    /// worker, so it names what to do instead.
    pub fn validate(&self) -> Result<(), String> {
        let long_fields = [
            ("conflicting commits", &self.conflicting_commits),
            ("changes on main", &self.main_changes),
            ("attempted port", &self.attempted_port),
        ];
        for (label, value) in long_fields {
            let value = value.trim();
            if value.is_empty() {
                return Err(format!("the {label} are required: {GUIDANCE}"));
            }
            if value.chars().count() > MAX_FIELD_CHARS {
                return Err(format!("the {label} exceed {MAX_FIELD_CHARS} characters"));
            }
        }
        let sentences = [
            ("contradiction", &self.contradiction),
            ("recommendation", &self.recommendation),
        ];
        for (label, value) in sentences {
            let value = value.trim();
            if value.is_empty() {
                return Err(format!("the {label} is required: {GUIDANCE}"));
            }
            if value.contains(['\n', '\r']) {
                return Err(format!("the {label} must be a single line"));
            }
            if value.chars().count() > MAX_SENTENCE_CHARS {
                return Err(format!("the {label} exceeds {MAX_SENTENCE_CHARS} characters"));
            }
        }
        if self.contradiction.split_whitespace().count() < MIN_CONTRADICTION_WORDS {
            return Err(format!(
                "the contradiction must be one full sentence naming the behaviour that cannot be preserved: {GUIDANCE}"
            ));
        }
        for (label, value) in sentences {
            let lowered = value.to_lowercase();
            if let Some(phrase) = REFACTOR_ONLY_PHRASES.iter().find(|phrase| lowered.contains(**phrase)) {
                return Err(format!(
                    "the {label} leans on `{phrase}`, which describes a refactor or an amount of work, not a \
                     behavioural contradiction. {GUIDANCE}"
                ));
            }
        }
        Ok(())
    }
}

const GUIDANCE: &str = "architectural_mismatch is only for a real behavioural conflict after a genuine attempt to port \
the change onto main's new layout (for example, the PR extends a subsystem main deleted outright). Moved or deleted \
files, split or renamed modules and crates, helpers that became private, migrations that must move, and every commit \
conflicting are normal conflict work: port the change's intent onto the new layout and push. If the port is bigger \
than the attempt's sizing, keep going and emit the effort-escalation marker";

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> ArchitecturalMismatchEvidence {
        ArchitecturalMismatchEvidence {
            basis: MismatchBasis::SubsystemDeleted,
            conflicting_commits: "abc123 add priority to the review queue".into(),
            main_changes: "the review-queue subsystem was deleted by the queue removal PR".into(),
            attempted_port:
                "Searched main for a replacement queue and found none; tried attaching the field to the dispatcher"
                    .into(),
            contradiction:
                "Main deleted the review queue entirely, so the PR's new queue priority has nowhere to live.".into(),
            recommendation:
                "Force a merge only if the priority should be dropped; otherwise rewrite around the dispatcher.".into(),
        }
    }

    #[test]
    fn a_deleted_subsystem_with_a_real_attempt_is_accepted() {
        evidence().validate().unwrap();
    }

    #[test]
    fn refactor_only_reasons_are_rejected() {
        for contradiction in [
            "Too many conflicts across every commit of the branch to resolve by hand.",
            "The files moved into the new dispatch crate so nothing lines up any more.",
            "Several helpers became private behind a new crate boundary in this refactor.",
            "This would be a large rewrite of the coordinator that is not worth attempting.",
        ] {
            let evidence = ArchitecturalMismatchEvidence {
                contradiction: contradiction.into(),
                ..evidence()
            };
            assert!(evidence.validate().is_err(), "{contradiction}");
        }
    }

    #[test]
    fn missing_or_malformed_fields_are_rejected() {
        for broken in [
            ArchitecturalMismatchEvidence {
                attempted_port: "  ".into(),
                ..evidence()
            },
            ArchitecturalMismatchEvidence {
                conflicting_commits: String::new(),
                ..evidence()
            },
            ArchitecturalMismatchEvidence {
                main_changes: String::new(),
                ..evidence()
            },
            ArchitecturalMismatchEvidence {
                contradiction: "It is deleted".into(),
                ..evidence()
            },
            ArchitecturalMismatchEvidence {
                recommendation: "line one\nline two".into(),
                ..evidence()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?}");
        }
    }

    #[test]
    fn basis_round_trips_through_its_wire_name() {
        for basis in MismatchBasis::ALL {
            assert_eq!(MismatchBasis::parse(basis.as_str()), Some(basis));
        }
        assert_eq!(MismatchBasis::parse("files_moved"), None);
    }

    #[test]
    fn non_canonical_mark_failed_reasons_are_rejected() {
        for reason in ["gave_up", "too_many_conflicts", "architecture_mismatch", ""] {
            let err = validate_conflict_mark_failed_reason(reason).unwrap_err();
            assert!(err.contains("not an allowed mark-failed reason"), "{err}");
            assert!(err.contains("normal conflict work"), "{err}");
        }
        for reason in CONFLICT_MARK_FAILED_REASONS {
            validate_conflict_mark_failed_reason(reason).unwrap();
        }
    }
}
