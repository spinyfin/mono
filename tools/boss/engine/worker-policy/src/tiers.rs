//! Shared tiers and exhaustive proposal-kind admission.
use boss_protocol::{ProposalKind, WorkerTierDenial, WorkerTierDenialReason};

use crate::WorkerVerbDecision;

/// Authorization tier for a frontend RPC.
///
/// - `User`: any local client (the human's `boss` CLI, the macOS app,
///   read-only callers, and any documented `bossctl` verb that has no
///   privileged side effect — e.g. `workspace summary`).
/// - `AppOrBoss`: privileged operations the app and the Boss session
///   may both invoke. This is the right level for the imperative
///   `bossctl` verbs (`probe`, `agents stop`, `agents transcript`,
///   `work cancel`): the human runs them from wherever they happen
///   to be — Boss pane, app shell, *inside a worker pane*, or a
///   plain terminal that descends from neither trust root. The
///   admission rule is "descendant of app or Boss, OR not a
///   descendant of any registered worker pane" — workers are the
///   only sibling-process adversary in the V2 threat model, so
///   excluding worker subtrees is sufficient. Earlier revisions
///   gated strictly on app/Boss subtree membership and locked the
///   coordinator out whenever it ran from a shell outside both
///   (e.g. a tmux pane started before the app launched).
/// - `BossOnly`: reserved for future control verbs that must reject
///   worker-pane callers. No live verb uses this tier today; the
///   `bossctl` verbs that previously gated on it (`probe_run`,
///   `tail_run_transcript`, `stop_run`) were all downgraded after
///   they kept locking the coordinator out of legitimate calls. Keep
///   the tier so any future verb can opt into it explicitly rather
///   than accidentally inheriting it.
/// - `Worker`: the odd one out — a *classification* rather than a
///   privilege level. `authorize_rpc(Worker, peer)` asks "is this
///   peer a live worker session?", i.e. does its process ancestry
///   contain a registered worker pane shell. It is the inverse of the
///   worker-exclusion clause the two tiers above use as a fallback.
///   Unlike the other tiers it is deliberately *not* permissive in
///   test mode: "no trust roots configured" must not mean "everything
///   is a worker", or an engine started without the macOS app would
///   confine the coordinator to worker tier.
///
///   The live connection-level gate that actually admits a connection
///   to this crate's verb policy does not go through
///   `authorize_rpc(Worker, ..)` — in the engine core crate's
///   `app/trust.rs` it calls `ServerState::classify_peer` once per
///   connection, then enforces `Worker` behind the `worker_rpc_tier`
///   flag and `Progression` unconditionally (`handle_frontend_connection`,
///   `worker_tier_denial`). `authorize_rpc(Worker, ..)` delegates to
///   the same `classify_peer` call and exists for handlers or tests
///   that want to ask the classification question through the generic
///   `authorize_rpc` entry point rather than reaching for
///   `classify_peer` directly; it is not itself on the production
///   dispatch path.
/// - `Progression`: a classification like `Worker`, for a progression
///   session (an agent that unblocks stuck work). Its verb surface is
///   deny-by-default and smaller than a worker's (see
///   [`PROGRESSION_ALLOWED_VERBS`](crate::PROGRESSION_ALLOWED_VERBS)), and it is
///   enforced regardless of the `worker_rpc_tier` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcTier {
    User,
    AppOrBoss,
    BossOnly,
    Worker,
    Progression,
}

/// Every kind must be reviewed for both agent tiers when the vocabulary grows.
/// Progression ships closed; a kind is enabled only alongside its scoped handler.
pub fn proposal_kind_decision(tier: RpcTier, kind: ProposalKind) -> WorkerVerbDecision {
    match kind {
        ProposalKind::Attention
        | ProposalKind::EffortEscalation
        | ProposalKind::Blocked
        | ProposalKind::DeferredScope
        | ProposalKind::FollowupTask
        | ProposalKind::AutomationOutcome
        | ProposalKind::PrCreated
        | ProposalKind::ReviewGuide
        | ProposalKind::ReviewReport
        | ProposalKind::ReviewVerdict
        | ProposalKind::RunDone
        | ProposalKind::Wait => match tier {
            RpcTier::Progression => WorkerVerbDecision::Deny(Box::new(WorkerTierDenial::progression(
                format!("SubmitProposal:{}", kind.as_str()),
                WorkerTierDenialReason::ProgressionScope,
                format!("proposal kind `{kind}` is not enabled for them."),
                None,
            ))),
            RpcTier::Worker | RpcTier::User | RpcTier::AppOrBoss | RpcTier::BossOnly => WorkerVerbDecision::Allow,
        },
    }
}
