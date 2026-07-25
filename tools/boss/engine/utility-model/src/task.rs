//! The engine-owned inference tasks that route through the utility seam.
//!
//! Each variant is one short helper completion the engine makes *for itself* —
//! never a worker dispatch. The per-task table below is the single place the
//! default model, the billing-bucket key env var, and the model-override env
//! var live; a provider reads it to build its answer and every call site reads
//! its model from the provider rather than from a private constant.

use std::fmt;

/// One short, engine-owned inference call.
///
/// These are deliberately enumerated rather than free-form strings: the set is
/// small, closed, and each member has a distinct latency/cost profile that an
/// operator may want to tune independently (a one-sentence kanban subtitle and
/// a full project plan should never be forced onto the same model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UtilityTask {
    /// One-sentence "what is this worker doing" for the kanban card
    /// (`live_status.rs`). Interactive path, fires per worker per tick.
    LiveStatus,
    /// Gerund-phrase pane titlebar label (`pane_summary.rs`). On the pane
    /// spawn path; cached per work item.
    PaneSummary,
    /// Graceful-degradation extraction of design questions / followups from a
    /// transcript tail (`attentions_detector.rs`). Off the hot path.
    AttentionsBackstop,
    /// Single-shot `revision` / `question` classification of one comment
    /// (`boss_engine_comment_classifier`). Detached-async, cheap, frequent.
    CommentIntent,
    /// Project decomposition into a task graph (`planner.rs`). Rare, slow, and
    /// the one member that legitimately wants a large model.
    Planner,
}

/// Every task, in declaration order. Used by providers that resolve their
/// whole table up front and by the crate's own coverage tests.
pub const ALL_TASKS: [UtilityTask; 5] = [
    UtilityTask::LiveStatus,
    UtilityTask::PaneSummary,
    UtilityTask::AttentionsBackstop,
    UtilityTask::CommentIntent,
    UtilityTask::Planner,
];

impl UtilityTask {
    /// Stable snake_case identifier. Appears in logs and forms the suffix of
    /// [`Self::model_env`], so it is part of the operator-facing contract —
    /// renaming one silently breaks an operator's env override.
    pub const fn slug(self) -> &'static str {
        match self {
            Self::LiveStatus => "live_status",
            Self::PaneSummary => "pane_summary",
            Self::AttentionsBackstop => "attentions_backstop",
            Self::CommentIntent => "comment_intent",
            Self::Planner => "planner",
        }
    }

    /// Optional per-feature credential env var, checked before the provider's
    /// base credential. These exist to route a feature's spend to a separate
    /// bucket and predate this seam — the values are carried over verbatim so
    /// an operator's existing environment keeps working.
    pub const fn key_env(self) -> Option<&'static str> {
        match self {
            Self::AttentionsBackstop => Some("BOSS_BACKSTOP_API_KEY"),
            Self::CommentIntent => Some("BOSS_INTENT_CLASSIFIER_API_KEY"),
            Self::LiveStatus | Self::PaneSummary | Self::Planner => None,
        }
    }

    /// Env var that overrides this task's model, e.g.
    /// `BOSS_UTILITY_MODEL_LIVE_STATUS`. Per-task rather than global on
    /// purpose: the whole point of the seam is that the cheap interactive
    /// calls and the expensive planning call move independently.
    pub const fn model_env(self) -> &'static str {
        match self {
            Self::LiveStatus => "BOSS_UTILITY_MODEL_LIVE_STATUS",
            Self::PaneSummary => "BOSS_UTILITY_MODEL_PANE_SUMMARY",
            Self::AttentionsBackstop => "BOSS_UTILITY_MODEL_ATTENTIONS_BACKSTOP",
            Self::CommentIntent => "BOSS_UTILITY_MODEL_COMMENT_INTENT",
            Self::Planner => "BOSS_UTILITY_MODEL_PLANNER",
        }
    }

    /// The model this task runs on when nothing overrides it.
    ///
    /// These are exactly the constants the call sites pinned before the seam
    /// existed. Introducing the seam must not move a single call site onto a
    /// different model — see the design doc's "Decision: UtilityModel shape
    /// and ownership" ("only the shape changes, not the model choice").
    pub const fn default_model(self) -> &'static str {
        match self {
            Self::LiveStatus => "claude-haiku-4-5-20251001",
            Self::PaneSummary => "claude-sonnet-4-6",
            Self::AttentionsBackstop => "claude-haiku-4-5-20251001",
            Self::CommentIntent => "claude-haiku-4-5-20251001",
            Self::Planner => "claude-opus-4-8",
        }
    }
}

impl fmt::Display for UtilityTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn slugs_are_unique() {
        let slugs: HashSet<&str> = ALL_TASKS.iter().map(|t| t.slug()).collect();
        assert_eq!(slugs.len(), ALL_TASKS.len(), "two tasks share a slug");
    }

    #[test]
    fn model_env_names_are_unique_and_slug_derived() {
        let envs: HashSet<&str> = ALL_TASKS.iter().map(|t| t.model_env()).collect();
        assert_eq!(envs.len(), ALL_TASKS.len(), "two tasks share a model env var");
        for task in ALL_TASKS {
            let expected = format!("BOSS_UTILITY_MODEL_{}", task.slug().to_uppercase());
            assert_eq!(task.model_env(), expected, "{task} model env drifted from its slug");
        }
    }

    /// The seam is a shape change, not a migration. If a default model here
    /// ever differs from what the call site pinned before, that is a
    /// behaviour change dressed up as plumbing.
    #[test]
    fn default_models_match_the_pre_seam_pins() {
        assert_eq!(UtilityTask::LiveStatus.default_model(), "claude-haiku-4-5-20251001");
        assert_eq!(UtilityTask::PaneSummary.default_model(), "claude-sonnet-4-6");
        assert_eq!(
            UtilityTask::AttentionsBackstop.default_model(),
            "claude-haiku-4-5-20251001"
        );
        assert_eq!(UtilityTask::CommentIntent.default_model(), "claude-haiku-4-5-20251001");
        assert_eq!(UtilityTask::Planner.default_model(), "claude-opus-4-8");
    }

    /// The per-feature billing buckets predate the seam; carrying them over
    /// verbatim is what keeps an existing operator environment working.
    #[test]
    fn key_env_overrides_match_the_pre_seam_env_vars() {
        assert_eq!(UtilityTask::AttentionsBackstop.key_env(), Some("BOSS_BACKSTOP_API_KEY"));
        assert_eq!(
            UtilityTask::CommentIntent.key_env(),
            Some("BOSS_INTENT_CLASSIFIER_API_KEY")
        );
        assert_eq!(UtilityTask::LiveStatus.key_env(), None);
        assert_eq!(UtilityTask::PaneSummary.key_env(), None);
        assert_eq!(UtilityTask::Planner.key_env(), None);
    }
}
