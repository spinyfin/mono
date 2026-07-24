//! The fixed v1 catalogue of everything Boothby is allowed to do.
//!
//! Boothby is an autonomous agent with coordinator privileges, so "what may
//! it do, and how much of it" cannot live in a prompt — a prompt is advice,
//! and the whole safety argument rests on the limits being unbypassable. It
//! lives here instead, as static data the executor consults on every call,
//! and it is deliberately a *closed* set: a verb slug that is not in this
//! table is refused rather than attempted.
//!
//! Every row is transcribed from `tools/boss/docs/designs/boothby.md`
//! §"Action catalogue with reversibility classification", which is the
//! authority on the numbers. The `#` in each doc comment is that table's row
//! number, so the two can be diffed by eye.
//!
//! ## The three columns that carry the safety properties
//!
//! * [`Autonomy`] — may Boothby do this unattended, or must an operator
//!   approve it first? A `propose` verb stays propose-gated even when the
//!   install is in `auto` mode; the mode can loosen nothing.
//! * [`CapGroup`] — the blast radius per pass. Grouped rather than per-verb
//!   because the design shares one budget across related verbs (closing 5
//!   stale tasks *and* 5 duplicates is 10 closes, which is not what "cap 5"
//!   is trying to buy).
//! * [`Reversibility`] — whether an undo can exist at all. This is what
//!   decides if a verb needs two-pass confirmation before it fires.

use boss_protocol::{
    BOOTHBY_REVERSIBILITY_IRREVERSIBLE, BOOTHBY_REVERSIBILITY_REVERSIBLE, BOOTHBY_REVERSIBILITY_SEMI,
    BOOTHBY_TARGET_ATTENTION, BOOTHBY_TARGET_ATTENTION_ITEM, BOOTHBY_TARGET_EXECUTION, BOOTHBY_TARGET_FILE,
    BOOTHBY_TARGET_ISSUE, BOOTHBY_TARGET_LEASE, BOOTHBY_TARGET_PROJECT, BOOTHBY_TARGET_TASK, BOOTHBY_TARGET_WORKSPACE,
};

/// Whether a verb may fire unattended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autonomy {
    /// Boothby may act without asking, subject to every other rail.
    Auto,
    /// Always becomes a proposal for an operator to approve, even when the
    /// install is in `auto` mode. The design marks the verbs whose blast
    /// radius is too wide to hand over: PR-drift fixes, lease force-release,
    /// ghost-execution cancels.
    Propose,
}

/// Whether the effect can be taken back, and therefore what the journal has
/// to capture.
///
/// The string forms are the protocol's `BOOTHBY_REVERSIBILITY_*` constants —
/// this enum is the typed view of the same vocabulary, so the catalogue can
/// be matched on exhaustively instead of compared against string literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reversibility {
    /// **R** — a pre-image restore is a true inverse.
    Reversible,
    /// **S** — a compensating action exists but is not an exact inverse
    /// (closing a filed issue does not unsend it).
    Semi,
    /// **I** — audit only. These are the verbs that require two-pass
    /// confirmation before they fire, because there is no second chance.
    Irreversible,
}

impl Reversibility {
    /// The `boothby_actions.reversibility` value for this class.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reversible => BOOTHBY_REVERSIBILITY_REVERSIBLE,
            Self::Semi => BOOTHBY_REVERSIBILITY_SEMI,
            Self::Irreversible => BOOTHBY_REVERSIBILITY_IRREVERSIBLE,
        }
    }

    /// I-class verbs cannot be undone, so the executor makes them prove the
    /// target's state held across two consecutive passes before acting.
    pub fn needs_two_pass_confirmation(self) -> bool {
        matches!(self, Self::Irreversible)
    }
}

/// A per-pass blast-radius budget, shared by every verb that names it.
///
/// Sharing is the point. The design gives "close stale task" and "close
/// duplicate task" a *combined* cap of 5, because an operator who accepts
/// "at most 5 closes a pass" does not thereby accept 10; the same holds for
/// filing a chore vs filing an issue (3 filings total, however routed).
/// Modelling the budget as a named group rather than a per-verb number is
/// what makes that expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapGroup {
    /// #1 + #2 — closing tasks, stale or duplicate.
    CloseTask,
    /// #3 — dismissing attention groups/members.
    DismissAttention,
    /// #4 — folding duplicate attentions together.
    MergeAttention,
    /// #5 — resolving legacy `work_attention_items`.
    ResolveLegacyAttention,
    /// #6 — archiving empty projects.
    ArchiveProject,
    /// #7 — pruning abandoned revision rows.
    PruneRevision,
    /// #8 — re-running the effort heuristic on drifted rows.
    RerunEffort,
    /// #9 + #10 — filing a finding, as a chore or a GitHub issue.
    FileFinding,
    /// #11 — nudging a stalled review.
    NudgeReview,
    /// #12 — reconciling PR-vs-task drift.
    ReconcilePrDrift,
    /// #13 — reaping a dead-but-live execution.
    ReapExecution,
    /// #14 — redispatching / unparking stuck work.
    RedispatchWork,
    /// #15 — force-releasing a stuck cube lease.
    ForceReleaseLease,
    /// #16 — cube workspace reconcile/gc. Capped at one invocation, not one
    /// workspace: the cube verbs are themselves batch operations.
    GcWorkspaces,
    /// #17 — recovery-patch GC. Capped in files.
    GcRecoveryPatches,
    /// #18 — cancelling a ghost execution.
    CancelGhostExecution,
}

impl CapGroup {
    /// This group's per-pass ceiling, from the design's `Cap/pass` column.
    pub fn cap(self) -> u32 {
        match self {
            Self::CloseTask => 5,
            Self::DismissAttention => 5,
            Self::MergeAttention => 3,
            Self::ResolveLegacyAttention => 5,
            Self::ArchiveProject => 2,
            Self::PruneRevision => 5,
            Self::RerunEffort => 10,
            Self::FileFinding => 3,
            Self::NudgeReview => 3,
            Self::ReconcilePrDrift => 3,
            Self::ReapExecution => 3,
            Self::RedispatchWork => 3,
            Self::ForceReleaseLease => 2,
            Self::GcWorkspaces => 1,
            Self::GcRecoveryPatches => 20,
            Self::CancelGhostExecution => 2,
        }
    }
}

/// Who writes the `boothby_actions` row for a verb.
///
/// This is not a stylistic split — the two paths are mechanically different.
/// A taxonomy verb mutates a WorkDb row, and the capture layer in
/// `work::boothby` diffs the row before/after inside the write's own
/// transaction, which is the only way a pre-image can be atomic with the
/// mutation it describes. An operational verb has no row to diff (reaping a
/// process, deleting a patch file), so nothing would ever be journalled
/// unless the executor writes the row itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalMode {
    /// The verb mutates a WorkDb row as actor `boothby`; the capture layer
    /// journals the column delta in-transaction. The executor arms the
    /// context and gets out of the way.
    WorkDbCapture,
    /// The effect is outside WorkDb, so the executor journals it explicitly
    /// after the effect succeeds.
    ExecutorWritten,
}

/// One row of the catalogue.
///
/// [`CATALOGUE`] builds these as `const` struct literals, which is why every
/// row spells out all six fields: a `const` cannot call a builder, and here
/// that restriction is doing useful work rather than fighting us. A new
/// policy column *should* break all 18 rows and force a deliberate answer for
/// each verb — the same reasoning the repo applies to its DB mappers, which
/// are likewise kept on struct literals so a missing column is a compile
/// error. The `bon::Builder` derive below satisfies `rust/giant-structs` and
/// gives runtime callers (tests, and any future dynamic construction) the
/// builder; the const table cannot use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, bon::Builder)]
pub struct VerbSpec {
    /// Stable slug, as it appears in `boothby_actions.verb` and on the wire.
    pub slug: &'static str,
    /// `boothby_actions.target_kind` for this verb's target.
    pub target_kind: &'static str,
    pub autonomy: Autonomy,
    pub reversibility: Reversibility,
    pub cap_group: CapGroup,
    pub journal: JournalMode,
}

/// Every verb Boothby has, in the design's table order.
///
/// Fixed in v1: there is deliberately no registration hook that would let a
/// verb appear at runtime. Adding one is a code change, a design change, and
/// a review — which is the intended cost.
pub const CATALOGUE: &[VerbSpec] = &[
    // #1 — status -> archived, archived_reason set, actor boothby.
    VerbSpec {
        slug: "close_stale_task",
        target_kind: BOOTHBY_TARGET_TASK,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::CloseTask,
        journal: JournalMode::WorkDbCapture,
    },
    // #2 — as #1 plus `duplicate of T<n>` and a description cross-link.
    VerbSpec {
        slug: "close_duplicate_task",
        target_kind: BOOTHBY_TARGET_TASK,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::CloseTask,
        journal: JournalMode::WorkDbCapture,
    },
    // #3 — dismiss_attention: state -> dismissed.
    VerbSpec {
        slug: "dismiss_stale_attention",
        target_kind: BOOTHBY_TARGET_ATTENTION,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::DismissAttention,
        journal: JournalMode::WorkDbCapture,
    },
    // #4 — AttentionMerge fold + retire_group_if_empty. Undo is best-effort
    // (design risk #5): the fold restores, but group retirement side effects
    // may not, hence the `conflicted` fallback rather than a demotion to
    // `propose`.
    VerbSpec {
        slug: "merge_duplicate_attentions",
        target_kind: BOOTHBY_TARGET_ATTENTION,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::MergeAttention,
        journal: JournalMode::WorkDbCapture,
    },
    // #5 — legacy work_attention_items: status -> resolved.
    VerbSpec {
        slug: "resolve_legacy_attention_item",
        target_kind: BOOTHBY_TARGET_ATTENTION_ITEM,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::ResolveLegacyAttention,
        journal: JournalMode::WorkDbCapture,
    },
    // #6 — ProjectStatus::Archived, actor boothby.
    VerbSpec {
        slug: "archive_empty_project",
        target_kind: BOOTHBY_TARGET_PROJECT,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::ArchiveProject,
        journal: JournalMode::WorkDbCapture,
    },
    // #7 — soft-delete (deleted_at); restore_work_item is the exact inverse.
    VerbSpec {
        slug: "prune_abandoned_revision",
        target_kind: BOOTHBY_TARGET_TASK,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::PruneRevision,
        journal: JournalMode::WorkDbCapture,
    },
    // #8 — effort_level update where effort_is_hand_set is false.
    VerbSpec {
        slug: "rerun_effort_heuristic",
        target_kind: BOOTHBY_TARGET_TASK,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::RerunEffort,
        journal: JournalMode::WorkDbCapture,
    },
    // #9 — create_chore with created_via = boothby:<finding-id>. A chore is
    // a WorkDb row, but it is an *insert*, not a column delta, so there is no
    // before-image for the capture layer to diff: the executor journals it.
    VerbSpec {
        slug: "file_chore",
        target_kind: BOOTHBY_TARGET_TASK,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::FileFinding,
        journal: JournalMode::ExecutorWritten,
    },
    // #10 — `boss shake` path, label `boothby`. Semi: the issue can be
    // closed with a comment, but never unsent.
    VerbSpec {
        slug: "file_github_issue",
        target_kind: BOOTHBY_TARGET_ISSUE,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Semi,
        cap_group: CapGroup::FileFinding,
        journal: JournalMode::ExecutorWritten,
    },
    // #11 — create/refresh a review_required-style attention item.
    VerbSpec {
        slug: "nudge_stalled_review",
        target_kind: BOOTHBY_TARGET_ATTENTION,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::NudgeReview,
        journal: JournalMode::ExecutorWritten,
    },
    // #12 — status fix where merge_poller evidence is unambiguous.
    VerbSpec {
        slug: "reconcile_pr_task_drift",
        target_kind: BOOTHBY_TARGET_TASK,
        autonomy: Autonomy::Propose,
        reversibility: Reversibility::Reversible,
        cap_group: CapGroup::ReconcilePrDrift,
        journal: JournalMode::WorkDbCapture,
    },
    // #13 — the `bossctl agents reap` path. Auto, but only with two-pass
    // confirmation of death; redispatch is the recovery, not undo.
    VerbSpec {
        slug: "reap_dead_execution",
        target_kind: BOOTHBY_TARGET_EXECUTION,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Irreversible,
        cap_group: CapGroup::ReapExecution,
        journal: JournalMode::ExecutorWritten,
    },
    // #14 — bossctl work start / nudge-breaker reset / probe. Semi:
    // cancelling the new execution compensates.
    VerbSpec {
        slug: "redispatch_stuck_work",
        target_kind: BOOTHBY_TARGET_EXECUTION,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Semi,
        cap_group: CapGroup::RedispatchWork,
        journal: JournalMode::ExecutorWritten,
    },
    // #15 — engine-mediated `cube workspace force-release`.
    VerbSpec {
        slug: "force_release_lease",
        target_kind: BOOTHBY_TARGET_LEASE,
        autonomy: Autonomy::Propose,
        reversibility: Reversibility::Irreversible,
        cap_group: CapGroup::ForceReleaseLease,
        journal: JournalMode::ExecutorWritten,
    },
    // #16 — cube workspace reconcile/gc, dry-run first then apply. Auto
    // because the cube verbs are conservative by construction.
    VerbSpec {
        slug: "gc_orphaned_workspaces",
        target_kind: BOOTHBY_TARGET_WORKSPACE,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Irreversible,
        cap_group: CapGroup::GcWorkspaces,
        journal: JournalMode::ExecutorWritten,
    },
    // #17 — delete <state_root>/recovery/<exec-id>.patch past its TTL.
    VerbSpec {
        slug: "gc_recovery_patches",
        target_kind: BOOTHBY_TARGET_FILE,
        autonomy: Autonomy::Auto,
        reversibility: Reversibility::Irreversible,
        cap_group: CapGroup::GcRecoveryPatches,
        journal: JournalMode::ExecutorWritten,
    },
    // #18 — cancel_execution on rows the sweeps flagged as unowned.
    VerbSpec {
        slug: "cancel_ghost_execution",
        target_kind: BOOTHBY_TARGET_EXECUTION,
        autonomy: Autonomy::Propose,
        reversibility: Reversibility::Semi,
        cap_group: CapGroup::CancelGhostExecution,
        journal: JournalMode::ExecutorWritten,
    },
];

/// The spec for `slug`, or `None` if the catalogue has no such verb.
///
/// A linear scan over 18 entries, which is cheaper than any index at this
/// size and is called once per `boothby.act` — a rate bounded by an LLM
/// deciding to act, i.e. a handful per half hour.
pub fn lookup(slug: &str) -> Option<&'static VerbSpec> {
    CATALOGUE.iter().find(|spec| spec.slug == slug)
}

/// Every verb sharing `group`'s budget. Used by the executor to count a
/// group's spend across all of its member verbs.
pub fn verbs_in_group(group: CapGroup) -> impl Iterator<Item = &'static str> {
    CATALOGUE
        .iter()
        .filter(move |spec| spec.cap_group == group)
        .map(|spec| spec.slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn catalogue_holds_the_designs_eighteen_verbs() {
        assert_eq!(CATALOGUE.len(), 18, "the v1 catalogue is fixed at the design's 18 rows");
    }

    #[test]
    fn every_slug_is_unique() {
        let mut seen = HashSet::new();
        for spec in CATALOGUE {
            assert!(seen.insert(spec.slug), "duplicate catalogue slug: {}", spec.slug);
        }
    }

    #[test]
    fn lookup_finds_a_known_verb_and_rejects_an_unknown_one() {
        assert_eq!(lookup("close_stale_task").map(|s| s.slug), Some("close_stale_task"));
        // The catalogue is closed: an invented slug must not resolve, or the
        // executor's "refuse unknown verbs" rail has nothing to stand on.
        assert!(lookup("delete_everything").is_none());
        assert!(lookup("").is_none());
    }

    /// The design shares one budget of 5 across #1 and #2 precisely so that
    /// "at most 5 closes" cannot become 10 by routing half through the
    /// duplicate verb.
    #[test]
    fn closing_stale_and_duplicate_tasks_share_one_budget() {
        let stale = lookup("close_stale_task").unwrap();
        let duplicate = lookup("close_duplicate_task").unwrap();
        assert_eq!(stale.cap_group, duplicate.cap_group);
        assert_eq!(stale.cap_group.cap(), 5);

        let group: HashSet<_> = verbs_in_group(CapGroup::CloseTask).collect();
        assert_eq!(group, HashSet::from(["close_stale_task", "close_duplicate_task"]));
    }

    /// Same reasoning as the close budget: 3 filings per pass total, however
    /// they are routed (chore on a Boss dev machine, issue elsewhere).
    #[test]
    fn filing_a_chore_and_an_issue_share_one_budget() {
        let chore = lookup("file_chore").unwrap();
        let issue = lookup("file_github_issue").unwrap();
        assert_eq!(chore.cap_group, issue.cap_group);
        assert_eq!(chore.cap_group.cap(), 3);

        let group: HashSet<_> = verbs_in_group(CapGroup::FileFinding).collect();
        assert_eq!(group, HashSet::from(["file_chore", "file_github_issue"]));
    }

    /// Transcription check against the design's `Cap/pass` column. Written
    /// out longhand rather than derived, so a typo in `CapGroup::cap` fails
    /// here instead of silently widening a blast radius.
    #[test]
    fn per_verb_caps_match_the_design_table() {
        let expected = [
            ("close_stale_task", 5),
            ("close_duplicate_task", 5),
            ("dismiss_stale_attention", 5),
            ("merge_duplicate_attentions", 3),
            ("resolve_legacy_attention_item", 5),
            ("archive_empty_project", 2),
            ("prune_abandoned_revision", 5),
            ("rerun_effort_heuristic", 10),
            ("file_chore", 3),
            ("file_github_issue", 3),
            ("nudge_stalled_review", 3),
            ("reconcile_pr_task_drift", 3),
            ("reap_dead_execution", 3),
            ("redispatch_stuck_work", 3),
            ("force_release_lease", 2),
            ("gc_orphaned_workspaces", 1),
            ("gc_recovery_patches", 20),
            ("cancel_ghost_execution", 2),
        ];
        assert_eq!(expected.len(), CATALOGUE.len(), "every verb needs a cap assertion");
        for (slug, cap) in expected {
            let spec = lookup(slug).unwrap_or_else(|| panic!("{slug} is missing from the catalogue"));
            assert_eq!(spec.cap_group.cap(), cap, "{slug} cap drifted from the design");
        }
    }

    /// The design's `Default` column. A verb silently flipping to `auto`
    /// would hand an operator-gated blast radius to the agent, so pin it.
    #[test]
    fn only_the_designs_three_verbs_are_propose_gated() {
        let propose: HashSet<_> = CATALOGUE
            .iter()
            .filter(|s| s.autonomy == Autonomy::Propose)
            .map(|s| s.slug)
            .collect();
        assert_eq!(
            propose,
            HashSet::from([
                "reconcile_pr_task_drift",
                "force_release_lease",
                "cancel_ghost_execution"
            ]),
        );
    }

    /// The design's `Class` column for the I-class rows. These are the verbs
    /// with no undo, so they are exactly the set that must pass two-pass
    /// confirmation.
    #[test]
    fn the_irreversible_verbs_are_the_ones_needing_two_pass_confirmation() {
        let irreversible: HashSet<_> = CATALOGUE
            .iter()
            .filter(|s| s.reversibility == Reversibility::Irreversible)
            .map(|s| s.slug)
            .collect();
        assert_eq!(
            irreversible,
            HashSet::from([
                "reap_dead_execution",
                "force_release_lease",
                "gc_orphaned_workspaces",
                "gc_recovery_patches",
            ]),
        );
        for spec in CATALOGUE {
            assert_eq!(
                spec.reversibility.needs_two_pass_confirmation(),
                spec.reversibility == Reversibility::Irreversible,
                "{} disagrees about needing two-pass confirmation",
                spec.slug,
            );
        }
    }

    /// The reversibility string is what lands in `boothby_actions`, whose
    /// CHECK constraint only admits these three.
    #[test]
    fn reversibility_renders_the_protocol_constants() {
        assert_eq!(Reversibility::Reversible.as_str(), "reversible");
        assert_eq!(Reversibility::Semi.as_str(), "semi");
        assert_eq!(Reversibility::Irreversible.as_str(), "irreversible");
    }

    /// An I-class verb has no pre-image to restore, so nothing it does can be
    /// journalled by the column-diffing capture layer — it must be a verb the
    /// executor journals itself. The converse does not hold (`file_chore` is
    /// R-class but still executor-written, because an insert has no
    /// before-image either).
    #[test]
    fn no_irreversible_verb_relies_on_workdb_column_capture() {
        for spec in CATALOGUE
            .iter()
            .filter(|s| s.reversibility == Reversibility::Irreversible)
        {
            assert_eq!(
                spec.journal,
                JournalMode::ExecutorWritten,
                "{} is irreversible but expects a column delta to journal it",
                spec.slug,
            );
        }
    }

    /// `target_kind` lands in a column the UI groups by, so a typo would
    /// scatter a verb's actions into a kind nothing renders.
    #[test]
    fn every_target_kind_is_a_protocol_constant() {
        let known = HashSet::from([
            BOOTHBY_TARGET_TASK,
            BOOTHBY_TARGET_PROJECT,
            BOOTHBY_TARGET_ATTENTION,
            BOOTHBY_TARGET_ATTENTION_ITEM,
            BOOTHBY_TARGET_EXECUTION,
            BOOTHBY_TARGET_LEASE,
            BOOTHBY_TARGET_WORKSPACE,
            BOOTHBY_TARGET_FILE,
            BOOTHBY_TARGET_ISSUE,
        ]);
        for spec in CATALOGUE {
            assert!(
                known.contains(spec.target_kind),
                "{} has an unknown target_kind {:?}",
                spec.slug,
                spec.target_kind,
            );
        }
    }
}
