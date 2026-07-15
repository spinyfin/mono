//! The worker-tier verb policy: one deny-by-default decision per
//! [`FrontendRequest`].
//!
//! ## Deny by default, exhaustively
//!
//! [`worker_verb_decision`] is a single `match` with **no wildcard arm**.
//! That is deliberate and load-bearing: a verb added to `FrontendRequest`
//! tomorrow will not compile until someone classifies it here. The
//! alternative — a `_ => Allow` or `_ => Deny` catch-all — either quietly
//! widens the worker's authority every time the protocol grows, or quietly
//! breaks a worker path with no signal at review time. The compile error is
//! the review prompt.
//!
//! ## What workers get
//!
//! Per design §"Read-only model access and the exposure boundary", the
//! isolation policy has two halves and this project relaxes only the first:
//!
//! - **The model half (relaxed):** the work taxonomy is readable — products,
//!   projects, tasks/chores/revisions, statuses, dependency edges, PR
//!   bindings, attentions, comments, and the execution/run rows for work the
//!   worker can already see (field-sanitized on the way out — see
//!   [`crate::sanitize`]). This is what ends stale-brief blindness and lets a
//!   worker check whether a follow-up already exists before proposing a
//!   duplicate.
//! - **The runtime half (unchanged):** dispatch state, slots, panes, live
//!   status, transcripts, hosts, engine config, sessions, and every
//!   `bossctl`-shaped verb stay denied.
//!
//! Writes are mediated: a worker submits proposals and the engine applies
//! them. The narrow exceptions are enumerated in [`worker_verb_decision`]'s
//! "sanctioned writes" arm, each with the reason it is sanctioned.

use boss_protocol::{FrontendRequest, WorkItemPatch, WorkerTierDenial, WorkerTierDenialReason};

/// Whether a worker-tier connection may execute a given verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerVerbDecision {
    /// Dispatch normally. The response still passes through
    /// [`crate::sanitize_event_for_worker`] on its way out.
    Allow,
    /// Refuse before dispatch — no handler runs. The denial names the verb
    /// and, where one exists, the `boss propose …` to use instead.
    Deny(Box<WorkerTierDenial>),
}

impl WorkerVerbDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, WorkerVerbDecision::Allow)
    }

    /// The denial, if this decision is one. Convenience for call sites that
    /// have already branched on [`Self::is_allowed`].
    pub fn denial(&self) -> Option<&WorkerTierDenial> {
        match self {
            WorkerVerbDecision::Allow => None,
            WorkerVerbDecision::Deny(denial) => Some(denial),
        }
    }
}

/// `boss propose followup-task` — the route for "this work should exist".
const PROPOSE_FOLLOWUP: &str = "boss propose followup-task";
/// `boss propose attention` — the route for "a human should look at this".
const PROPOSE_ATTENTION: &str = "boss propose attention";
/// `boss propose pr-created` — the route for declaring a PR was opened.
const PROPOSE_PR_CREATED: &str = "boss propose pr-created";
/// `boss propose effort-escalation` — the route for "this is bigger than filed".
const PROPOSE_EFFORT_ESCALATION: &str = "boss propose effort-escalation";
/// `boss propose deferred-scope` — the route for "I left part of this undone".
const PROPOSE_DEFERRED_SCOPE: &str = "boss propose deferred-scope";
/// The general fallback for an `UpdateWorkItem` patch that names neither a PR
/// binding nor an effort change — the general `boss propose` route, since no
/// single mechanical redirect fits every other field on the patch.
const PROPOSE_GENERAL: &str = "boss propose (pr-created, effort-escalation, blocked, ...)";

/// Which `boss propose …` a denied `UpdateWorkItem` should point at, based on
/// which field of the patch the worker was actually trying to set. Branching
/// here — rather than always naming `pr-created` — is what keeps the
/// redirect mechanical instead of guessed: a worker denied an effort change
/// gets told about `effort-escalation`, not a PR-binding verb that does not
/// apply to its intent.
fn update_work_item_redirect(patch: &WorkItemPatch) -> &'static str {
    if patch.pr_url.is_some() {
        PROPOSE_PR_CREATED
    } else if patch.effort_level.is_some() {
        PROPOSE_EFFORT_ESCALATION
    } else {
        PROPOSE_GENERAL
    }
}

fn redirect(verb: String, use_instead: &str) -> WorkerVerbDecision {
    WorkerVerbDecision::Deny(Box::new(WorkerTierDenial::redirect(
        verb,
        WorkerTierDenialReason::MutatingTaxonomy,
        use_instead,
    )))
}

fn closed(verb: String, reason: WorkerTierDenialReason) -> WorkerVerbDecision {
    WorkerVerbDecision::Deny(Box::new(WorkerTierDenial::closed(verb, reason)))
}

fn taxonomy(verb: String) -> WorkerVerbDecision {
    closed(verb, WorkerTierDenialReason::MutatingTaxonomy)
}

fn runtime(verb: String) -> WorkerVerbDecision {
    closed(verb, WorkerTierDenialReason::RuntimeIsolation)
}

fn coordinator(verb: String) -> WorkerVerbDecision {
    closed(verb, WorkerTierDenialReason::CoordinatorOnly)
}

/// Classify one request for a worker-tier caller.
///
/// Pure: the decision depends on the verb alone, never on the caller's
/// identity or on payload contents. Per-row scoping (a worker may only submit
/// proposals against *its own* work item, may only answer *its own* comment
/// thread) is enforced inside the individual handlers from the peer-resolved
/// execution — this function decides only whether the handler runs at all.
/// `GetRun` and `ListRuns` are the one exception: their scoping needs a
/// database lookup this crate deliberately does not have, so it runs at the
/// connection gate instead (`ServerState::worker_row_scope_denial` in
/// `boss-engine-core`'s `app/trust.rs`), ahead of the handler, denying a
/// `GetRun`/`ListRuns` whose target execution is not the caller's own.
pub fn worker_verb_decision(request: &FrontendRequest) -> WorkerVerbDecision {
    use WorkerVerbDecision::Allow;

    match request {
        // ── Allowed: taxonomy reads ──────────────────────────────────────
        //
        // The model half of the isolation boundary. Nothing here mutates,
        // and nothing here is scoped to the caller — a worker can see
        // sibling tasks in its project, which is the entire point (design
        // §"Goals": "ending stale-brief confusion and duplicated effort from
        // workers that cannot see sibling tasks").
        //
        // `ListProductDesignDocs` / `GetProductDesignDoc` join this group
        // for the same reason `ResolveProjectDesignDoc` is here: they are
        // read-only reads of a product's *own* repo, and they expose
        // nothing a worker could not already get by running `gh api` in
        // its own shell. Denying them would gate design-doc reading behind
        // a capability the worker demonstrably already has.
        FrontendRequest::FindWorkItemsByPr { .. }
        | FrontendRequest::GetProductDesignDoc { .. }
        | FrontendRequest::GetWorkItem { .. }
        | FrontendRequest::GetWorkItemByShortId { .. }
        | FrontendRequest::GetWorkTree { .. }
        | FrontendRequest::GetWorkerContext { .. }
        | FrontendRequest::ListChores { .. }
        | FrontendRequest::ListProductDesignDocs { .. }
        | FrontendRequest::ListProducts
        | FrontendRequest::ListProjects { .. }
        | FrontendRequest::ListRevisions { .. }
        | FrontendRequest::ListTasks { .. }
        | FrontendRequest::ResolveProjectDesignDoc { .. } => Allow,

        // ── Allowed: dependency-edge reads ───────────────────────────────
        FrontendRequest::ListDependencies { .. } | FrontendRequest::ListDependenciesDetailed { .. } => Allow,

        // ── Allowed: attention reads ─────────────────────────────────────
        //
        // Read-only visibility into what has been filed against the work,
        // including a worker's own prior escalations and deferred scope.
        FrontendRequest::GetAttentionGroup { .. }
        | FrontendRequest::GetAttentionItem { .. }
        | FrontendRequest::ListAttentionItems { .. }
        | FrontendRequest::ListAttentionItemsForWorkItem { .. }
        | FrontendRequest::ListAttentionGroups { .. }
        | FrontendRequest::ListDeferredScopeAttentions { .. } => Allow,

        // ── Allowed: comment reads ───────────────────────────────────────
        //
        // A revision worker addressing PR review comments has to be able to
        // read the thread it is answering.
        FrontendRequest::CommentsBannerState { .. } | FrontendRequest::CommentsList { .. } => Allow,

        // ── Allowed: execution / run reads (sanitized on the way out) ────
        //
        // These carry the only rows that mix the two halves, so they are the
        // reason `sanitize_event_for_worker` exists: the taxonomy-relevant
        // columns (status, PR binding, timestamps) are exposed while
        // `transcript_path` and friends are stripped. See `crate::sanitize`.
        //
        // `GetExecution`/`ListExecutions` are unscoped by design — sibling
        // executions are part of the model half a worker is meant to see,
        // same as sibling tasks above. `GetRun`/`ListRuns` are scoped to the
        // caller's own execution, but *not here*: this match is pure and has
        // no database access, so the scoping runs at the connection gate
        // (`ServerState::worker_row_scope_denial`) ahead of the handler. See
        // this function's doc comment.
        FrontendRequest::GetExecution { .. }
        | FrontendRequest::GetRun { .. }
        | FrontendRequest::ListExecutions { .. }
        | FrontendRequest::ListRuns { .. } => Allow,

        // ── Allowed: the proposal API itself ─────────────────────────────
        //
        // The whole point of the tier. Both verbs re-derive the caller's
        // execution from the socket peer independently of this gate, so a
        // worker still cannot reach another run's work item through them.
        FrontendRequest::ListProposals { .. } | FrontendRequest::SubmitProposal { .. } => Allow,

        // ── Allowed: sanctioned writes ───────────────────────────────────
        //
        // Each of these is a *declaration about the worker's own run* that
        // the engine already provenance-checks, not a taxonomy edit:
        //
        // - `CreateAutomationTask` is triage's `boss task create
        //   --automation`. The design keeps this create direct and mediates
        //   only the outcome declaration: the create is already
        //   provenance-checked, and it is the single point where a future
        //   structural dedup gate will attach (§"Risks / open questions").
        //   Removing it here would break every triage run.
        // - `RecordProducerSideConflict` / `MarkConflictResolutionFailed`
        //   are `boss engine conflicts …`, instructed by the conflict
        //   worker prompt (`runner.rs`) and by the worker preamble's
        //   merge-conflict telemetry section.
        // - The CI-remediation marks are `boss engine ci …`, instructed by
        //   the CI worker prompt; each is scoped to an attempt id the
        //   prompt handed the worker.
        // - `SetProjectDesignDoc` is the design worker recording where it
        //   put the doc it just wrote — named explicitly in the design's
        //   worker-tier verb policy.
        // - `CommentsPostAnswer` is the answer agent's reply; it already
        //   resolves comment and run from the caller's own `BOSS_RUN_ID`
        //   and cannot target another thread (see `app/comments.rs`).
        FrontendRequest::ClassifyCiRemediation { .. }
        | FrontendRequest::CommentsPostAnswer { .. }
        | FrontendRequest::CreateAutomationTask { .. }
        | FrontendRequest::MarkCiRemediationFailed { .. }
        | FrontendRequest::MarkCiRemediationNoop { .. }
        | FrontendRequest::MarkCiRemediationRetriggered { .. }
        | FrontendRequest::MarkCiRemediationSucceededViaRebase { .. }
        | FrontendRequest::MarkConflictResolutionFailed { .. }
        | FrontendRequest::RecordProducerSideConflict { .. }
        | FrontendRequest::SetProjectDesignDoc { .. } => Allow,

        // ── Allowed: engine version ──────────────────────────────────────
        //
        // Version-skew diagnosis ("is the engine running the build I just
        // made?") is a legitimate thing to do from a worker shell and leaks
        // nothing about other runs. `GetEngineHealth` is a different animal
        // and is denied below with the rest of the runtime half.
        FrontendRequest::GetEngineVersion => Allow,

        // ── Denied: taxonomy writes with a proposal route ────────────────
        //
        // The mediation invariant, now enforced by the engine rather than by
        // prompt text. Each redirect names the verb that *does* work, so the
        // worker's next move is mechanical.
        FrontendRequest::CreateChore { .. }
        | FrontendRequest::CreateInvestigation { .. }
        | FrontendRequest::CreateManyChores { .. }
        | FrontendRequest::CreateManyTasks { .. }
        | FrontendRequest::CreateTask { .. } => redirect(variant_name(request), PROPOSE_FOLLOWUP),

        FrontendRequest::CreateAttention { .. } | FrontendRequest::CreateAttentionItem { .. } => {
            redirect(variant_name(request), PROPOSE_ATTENTION)
        }

        FrontendRequest::RecordEffortEscalation { .. } => redirect(variant_name(request), PROPOSE_EFFORT_ESCALATION),

        FrontendRequest::AcceptDeferredScopeAttention { .. }
        | FrontendRequest::CreateTaskFromDeferredScopeAttention { .. } => {
            redirect(variant_name(request), PROPOSE_DEFERRED_SCOPE)
        }

        // `UpdateWorkItem` is how a work item's status, PR binding, effort,
        // and description are all edited, so the redirect branches on the
        // patch rather than always naming `pr-created` — a worker denied an
        // effort change should not be told to declare a PR.
        FrontendRequest::UpdateWorkItem { patch, .. } => {
            redirect(variant_name(request), update_work_item_redirect(patch))
        }

        // ── Denied: taxonomy writes with no proposal route ───────────────
        //
        // Structural edits — deleting work, re-parenting, reordering,
        // dependency surgery, actioning someone else's attention. There is
        // no worker-facing equivalent by design; `boss propose blocked` is
        // the escape hatch the `closed` message points at.
        FrontendRequest::ActionAttentionGroup { .. }
        | FrontendRequest::AddDependency { .. }
        | FrontendRequest::AnswerAttention { .. }
        | FrontendRequest::CreateProduct { .. }
        | FrontendRequest::CreateProject { .. }
        | FrontendRequest::CreateRevision { .. }
        | FrontendRequest::DeleteWorkItem { .. }
        | FrontendRequest::DismissAttention { .. }
        | FrontendRequest::LinkWorkItemExternalRef { .. }
        | FrontendRequest::RemoveDependency { .. }
        | FrontendRequest::ReorderProjectTasks { .. }
        | FrontendRequest::RestoreWorkItem { .. }
        | FrontendRequest::SetProductDefaultDriver { .. }
        | FrontendRequest::SetProductDefaultModel { .. }
        | FrontendRequest::SetProductEditorialRules { .. }
        | FrontendRequest::SetProductExternalTracker { .. }
        | FrontendRequest::SetProductMergeMechanism { .. }
        | FrontendRequest::UnlinkWorkItemExternalRef { .. } => taxonomy(variant_name(request)),

        // ── Denied: the runtime half ─────────────────────────────────────
        //
        // Dispatch state, slots, panes, live status, transcripts, other
        // executions' runs, hosts, engine config, metrics, the subscription
        // firehose. Unchanged by this project (design §"Non-goals":
        // "Relaxing the runtime half of worker isolation" is explicitly not
        // a goal). `TailRunTranscript` / `ExecutionTranscript` are the
        // sharpest edge here — they are how one worker would read another's
        // transcript.
        FrontendRequest::CancelExecution { .. }
        | FrontendRequest::CreateExecution { .. }
        | FrontendRequest::CreateRun { .. }
        | FrontendRequest::DebugLiveStatusPipeline
        | FrontendRequest::ExecutionTranscript { .. }
        | FrontendRequest::FocusWorkerPane { .. }
        | FrontendRequest::GetDispatchState
        | FrontendRequest::GetEngineHealth
        | FrontendRequest::GetSettings
        | FrontendRequest::GetTaskRuntime { .. }
        | FrontendRequest::InterruptWorkerPane { .. }
        | FrontendRequest::ListEngineAttempts { .. }
        | FrontendRequest::ListFeatureFlags
        | FrontendRequest::ListHuskPanes
        | FrontendRequest::ListLiveStatusDisabledSlots
        | FrontendRequest::ListWorkerLiveStates
        | FrontendRequest::MetricsListLive
        | FrontendRequest::MetricsReset { .. }
        | FrontendRequest::MetricsShowLive { .. }
        | FrontendRequest::OpenDocument { .. }
        | FrontendRequest::OpenLiveWorkspaceTerminal { .. }
        | FrontendRequest::OpenReviewTerminal { .. }
        | FrontendRequest::ProbeRun { .. }
        | FrontendRequest::ReapRun { .. }
        | FrontendRequest::ReleaseReviewTerminal { .. }
        | FrontendRequest::RequestExecution { .. }
        | FrontendRequest::RetirePane { .. }
        | FrontendRequest::RevealWorkItem { .. }
        | FrontendRequest::SendInputToWorker { .. }
        | FrontendRequest::SetDispatchPaused { .. }
        | FrontendRequest::SetFeatureFlag { .. }
        | FrontendRequest::SetLiveStatusEnabled { .. }
        | FrontendRequest::SetSetting { .. }
        | FrontendRequest::StopRun { .. }
        | FrontendRequest::Subscribe { .. }
        | FrontendRequest::TailRunTranscript { .. }
        | FrontendRequest::Unsubscribe { .. }
        | FrontendRequest::WorkerPoolSummary
        | FrontendRequest::WorkspacePoolSummary => runtime(variant_name(request)),

        // ── Denied: host registry ────────────────────────────────────────
        FrontendRequest::AddHost { .. }
        | FrontendRequest::AddHostTag { .. }
        | FrontendRequest::GetHost { .. }
        | FrontendRequest::ListHosts
        | FrontendRequest::RemoveHost { .. }
        | FrontendRequest::RemoveHostTag { .. }
        | FrontendRequest::SetHostEnabled { .. } => runtime(variant_name(request)),

        // ── Denied: session / trust plumbing ─────────────────────────────
        //
        // Registration verbs *establish* the trust roots this gate is built
        // on. A worker calling one would be repointing the engine's notion
        // of who the app or the Boss session is.
        FrontendRequest::EngineResponse { .. }
        | FrontendRequest::RegisterAppSession
        | FrontendRequest::RegisterBossSession { .. }
        | FrontendRequest::RegisterCapabilities { .. }
        | FrontendRequest::ReportWorkerSpawnFailed { .. }
        | FrontendRequest::Shutdown { .. }
        | FrontendRequest::SpawnCapabilityRestored
        | FrontendRequest::UpdateWorkerShellPid { .. }
        | FrontendRequest::WorkerPaneDied { .. } => coordinator(variant_name(request)),

        // ── Denied: coordinator control surfaces ─────────────────────────
        //
        // Automation management, the planner, CI/conflict *operator* verbs
        // (retry/abandon, as distinct from the worker's own mark-* calls
        // allowed above), review triggering, merge control, editorial rules,
        // auth, and operator comment actions. The worker answers to the
        // coordinator; it does not drive it.
        FrontendRequest::AbandonCiRemediation { .. }
        | FrontendRequest::AbandonConflictResolution { .. }
        | FrontendRequest::AuditProductEffort { .. }
        | FrontendRequest::CommentsCreate { .. }
        | FrontendRequest::CommentsDismiss { .. }
        | FrontendRequest::CommentsPostFollowup { .. }
        | FrontendRequest::CommentsResolve { .. }
        | FrontendRequest::CommentsReviseDoc { .. }
        | FrontendRequest::CommentsSetIntent { .. }
        | FrontendRequest::CommentsSetStatus { .. }
        | FrontendRequest::CommentsUpdateAnchor { .. }
        | FrontendRequest::CreateAutomation { .. }
        | FrontendRequest::DeleteAutomation { .. }
        | FrontendRequest::DisableAutomation { .. }
        | FrontendRequest::EnableAutomation { .. }
        | FrontendRequest::EvaluateEditorialRules { .. }
        | FrontendRequest::BoothbyAct { .. }
        | FrontendRequest::GetAutomation { .. }
        // Automation *state* is coordinator configuration, not work
        // taxonomy: it is not in the design's exposed read set, and a triage
        // worker never needs it — its automation id arrives in the prompt,
        // and the open-task cap and pre-file dedup gate are re-checked
        // engine-side inside `CreateAutomationTask`.
        | FrontendRequest::GetAutomationOpenTaskCount { .. }
        | FrontendRequest::ListAutomationTasks { .. }
        | FrontendRequest::GetAutomationState
        | FrontendRequest::GetCiBudget { .. }
        | FrontendRequest::GetCiRemediation { .. }
        | FrontendRequest::GetConflictHotspots { .. }
        | FrontendRequest::GetConflictResolution { .. }
        | FrontendRequest::GitHubAuthCancel
        | FrontendRequest::GitHubAuthDisconnect
        | FrontendRequest::GitHubAuthStart
        | FrontendRequest::GitHubAuthStatus
        | FrontendRequest::KickPrReconcilers
        | FrontendRequest::ListAttentionMerges { .. }
        | FrontendRequest::ListAutomationDedupSuppressions { .. }
        | FrontendRequest::ListAutomationRuns { .. }
        | FrontendRequest::ListAutomations { .. }
        | FrontendRequest::ListCiRemediations { .. }
        | FrontendRequest::ListConflictResolutions { .. }
        | FrontendRequest::ListEditorialActions { .. }
        | FrontendRequest::ListPlannerRuns { .. }
        | FrontendRequest::MergeWhenReady { .. }
        | FrontendRequest::PlanProject { .. }
        | FrontendRequest::ReleaseProject { .. }
        | FrontendRequest::RetryCiRemediation { .. }
        | FrontendRequest::RetryConflictResolution { .. }
        | FrontendRequest::RunAutomation { .. }
        | FrontendRequest::SetAutomationPaused { .. }
        | FrontendRequest::SetCiBudget { .. }
        | FrontendRequest::SyncProductExternalTracker { .. }
        | FrontendRequest::TriggerPrReview { .. }
        | FrontendRequest::TrunkSetToken { .. }
        | FrontendRequest::TrunkStatus
        | FrontendRequest::UnpopulateProject { .. }
        | FrontendRequest::UpdateAutomation { .. } => coordinator(variant_name(request)),
    }
}

/// The `FrontendRequest` variant name, for denial messages and logs.
///
/// Read back off serde's own `"type"` discriminator rather than
/// hand-maintained per arm. `FrontendRequest` is
/// `#[serde(tag = "type", rename_all = "snake_case")]`, so that tag *is* the
/// canonical wire name of the verb — deriving from it means a renamed verb
/// cannot leave a stale string behind here, which a second 171-arm `match`
/// very much could.
///
/// Only ever called on the denial path, so the round-trip through
/// `serde_json` costs nothing on allowed traffic.
pub fn variant_name(request: &FrontendRequest) -> String {
    serde_json::to_value(request)
        .ok()
        .as_ref()
        .and_then(|value| value.get("type"))
        .and_then(|tag| tag.as_str())
        .map(snake_to_upper_camel)
        // Unreachable for the plain-data `FrontendRequest` variants, but a
        // denial must still render *something* rather than panicking inside
        // the RPC path.
        .unwrap_or_else(|| "UnknownVerb".to_owned())
}

/// `create_task` → `CreateTask`. The inverse of serde's `rename_all =
/// "snake_case"` for these identifiers, which are all ASCII alphanumerics
/// plus `_`.
fn snake_to_upper_camel(tag: &str) -> String {
    let mut out = String::with_capacity(tag.len());
    let mut capitalize = true;
    for ch in tag.chars() {
        if ch == '_' {
            capitalize = true;
        } else if capitalize {
            out.extend(ch.to_uppercase());
            capitalize = false;
        } else {
            out.push(ch);
        }
    }
    out
}
