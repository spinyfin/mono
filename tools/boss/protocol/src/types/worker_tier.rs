//! Typed refusals for the worker RPC tier.
//!
//! A frontend connection whose socket peer descends from a registered worker
//! pane shell executes at *worker tier*: it may read the work taxonomy,
//! submit proposals, and call the handful of sanctioned telemetry verbs — and
//! nothing else. Everything outside that set comes back as a
//! [`WorkerTierDenial`] instead of executing.
//!
//! The refusal is typed for the same reason a proposal refusal is (see
//! [`crate::ProposalSubmissionError`]): the worker on the other end is an LLM
//! that has to decide what to do next, and "denied" is not an instruction.
//! [`WorkerTierDenial::use_instead`] carries the verb it should have called —
//! usually a `boss propose …` — so the remediation is mechanical rather than
//! inferred from prose.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Transport and authn: the worker RPC tier".

use serde::{Deserialize, Serialize};

/// Why a worker-tier connection was refused a verb.
///
/// The vocabulary is keyed on *what the worker should do about it*, not on
/// which module said no: a `MutatingTaxonomy` refusal has a proposal verb to
/// redirect to, while `RuntimeIsolation` and `CoordinatorOnly` have no worker
/// route at all and the worker should stop asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTierDenialReason {
    /// A taxonomy write (`CreateTask`, `UpdateWorkItem`, dependency edges,
    /// attention actioning, …). Workers propose; the engine applies. This is
    /// the mediation invariant the whole proposal API exists to enforce, and
    /// it is the one reason that always carries a
    /// [`WorkerTierDenial::use_instead`].
    MutatingTaxonomy,
    /// The runtime half of worker isolation — dispatch state, slots, panes,
    /// live status, transcripts, other executions' runs, hosts, engine
    /// config. Unchanged by the proposal-API project (design §"Non-goals");
    /// there is no worker-facing equivalent and there is not meant to be.
    RuntimeIsolation,
    /// A coordinator/app-shaped verb: session registration, `bossctl`
    /// surfaces, planner control, automation management, GitHub/trunk auth.
    /// The worker answers to the coordinator, it does not drive it.
    CoordinatorOnly,
    /// A progression session reached outside its scope: either a verb or
    /// proposal kind that is not enabled for progression, or a target
    /// outside the neighbourhood captured when the session started.
    ProgressionScope,
}

impl WorkerTierDenialReason {
    pub const ALL: &'static [WorkerTierDenialReason] = &[
        WorkerTierDenialReason::MutatingTaxonomy,
        WorkerTierDenialReason::RuntimeIsolation,
        WorkerTierDenialReason::CoordinatorOnly,
        WorkerTierDenialReason::ProgressionScope,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            WorkerTierDenialReason::MutatingTaxonomy => "mutating_taxonomy",
            WorkerTierDenialReason::RuntimeIsolation => "runtime_isolation",
            WorkerTierDenialReason::CoordinatorOnly => "coordinator_only",
            WorkerTierDenialReason::ProgressionScope => "progression_scope",
        }
    }
}

impl std::fmt::Display for WorkerTierDenialReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A refused verb, as returned by [`crate::FrontendEvent::WorkerTierDenied`].
///
/// `verb` is the `FrontendRequest` variant name (`"CreateTask"`), which is
/// what engine logs and tests key on. `message` is human-readable on its own
/// so a caller that only prints the message still says something actionable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerTierDenial {
    pub verb: String,
    pub reason: WorkerTierDenialReason,
    pub message: String,
    /// The verb the worker should have called instead — e.g.
    /// `"boss propose followup-task"`. `None` when there is no worker-facing
    /// route to the capability at all, which is itself the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_instead: Option<String>,
}

impl WorkerTierDenial {
    /// A denial with a redirect: `use_instead` names the verb to call.
    pub fn redirect(verb: impl Into<String>, reason: WorkerTierDenialReason, use_instead: impl Into<String>) -> Self {
        let verb = verb.into();
        let use_instead = use_instead.into();
        Self {
            message: format!(
                "`{verb}` is not available to worker sessions: {}. Use `{use_instead}` instead.",
                reason.explanation()
            ),
            verb,
            reason,
            use_instead: Some(use_instead),
        }
    }

    /// A denial with no worker-facing alternative. The message says so
    /// explicitly rather than leaving the worker to guess that retrying a
    /// different way might work.
    pub fn closed(verb: impl Into<String>, reason: WorkerTierDenialReason) -> Self {
        let verb = verb.into();
        Self {
            message: format!(
                "`{verb}` is not available to worker sessions: {}. There is no worker-facing \
                 equivalent — ask the coordinator, or use `boss propose blocked --reason \"…\"` \
                 if this blocks your task.",
                reason.explanation()
            ),
            verb,
            reason,
            use_instead: None,
        }
    }

    /// A refusal for a progression session. Never carries worker guidance
    /// (`boss propose blocked` is itself closed to progression): the message
    /// states the progression boundary, and `use_instead` names the verb a
    /// progression session may use for the same intent, when one exists.
    pub fn progression(
        verb: impl Into<String>,
        reason: WorkerTierDenialReason,
        detail: impl AsRef<str>,
        use_instead: Option<String>,
    ) -> Self {
        let verb = verb.into();
        let mut message = format!("`{verb}` is not available to progression sessions: {}", detail.as_ref());
        match &use_instead {
            Some(alt) => message.push_str(&format!(" Use `{alt}` instead.")),
            None => message
                .push_str(" Progression sessions can read the work taxonomy and submit enabled proposal kinds only."),
        }
        Self {
            verb,
            reason,
            message,
            use_instead,
        }
    }
}

impl WorkerTierDenialReason {
    /// The mid-sentence clause used to build a denial message.
    fn explanation(self) -> &'static str {
        match self {
            WorkerTierDenialReason::MutatingTaxonomy => "workers propose taxonomy changes, the engine applies them",
            WorkerTierDenialReason::RuntimeIsolation => {
                "engine runtime state (dispatch, panes, transcripts, other executions) is off-limits to workers"
            }
            WorkerTierDenialReason::CoordinatorOnly => "this is a coordinator/app verb",
            WorkerTierDenialReason::ProgressionScope => "outside what a progression session is permitted to do",
        }
    }
}

impl std::fmt::Display for WorkerTierDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.reason, self.message)
    }
}

impl std::error::Error for WorkerTierDenial {}
