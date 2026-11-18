//! The agent-tier verb policy: translate the request contract into denials.
//!
//! [`verb_decision`] does not keep a second request inventory. Every
//! [`FrontendRequest`] is classified in [`boss_protocol::request_contract`];
//! this module turns [`WorkerAccess`] and [`ProgressionAccess`] into the
//! current [`WorkerVerbDecision`] objects. Trusted tiers stay Allow. The
//! compile error for a new verb lives on the contract match, which has no
//! wildcard arm.
//!
//! Progression is classified independently of worker access: a worker Allow
//! never opens the progression tier. The independently authored fixtures in
//! `tier_matrix_tests` are the expected progression set.

use crate::RpcTier;

use boss_protocol::{
    FrontendRequest, ProgressionAccess, WorkerAccess, WorkerTierDenial, WorkerTierDenialReason, request_contract,
};

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
/// Pure: the decision depends on the verb (and, for `UpdateWorkItem`, the
/// patch fields that select a redirect). Per-row scoping (a worker may only
/// submit proposals against *its own* work item, may only answer *its own*
/// comment thread) is enforced inside the individual handlers from the
/// peer-resolved execution — this function decides only whether the handler
/// runs at all. `GetRun` and `ListRuns` are the one exception: their scoping
/// needs a database lookup this crate deliberately does not have, so it runs
/// at the connection gate instead (`ServerState::worker_row_scope_denial` in
/// `boss-engine-core`'s `app/trust.rs`), ahead of the handler, denying a
/// `GetRun`/`ListRuns` whose target execution is not the caller's own.
pub fn worker_verb_decision(request: &FrontendRequest) -> WorkerVerbDecision {
    verb_decision(RpcTier::Worker, request)
}

/// Classify every request for the authenticated connection tier.
///
/// The two agent tiers are decided separately so neither can widen the other.
/// Worker and progression values come from the request contract; this function
/// never reads a tier out of the RPC payload.
pub fn verb_decision(tier: RpcTier, request: &FrontendRequest) -> WorkerVerbDecision {
    match tier {
        RpcTier::User | RpcTier::AppOrBoss | RpcTier::BossOnly => WorkerVerbDecision::Allow,
        RpcTier::Worker => worker_from_contract(request),
        RpcTier::Progression => progression_from_contract(request),
    }
}

fn worker_from_contract(request: &FrontendRequest) -> WorkerVerbDecision {
    match request_contract(request).worker {
        WorkerAccess::Allow => WorkerVerbDecision::Allow,
        WorkerAccess::Redirect(selector) => redirect(variant_name(request), selector.as_command()),
        WorkerAccess::Taxonomy => taxonomy(variant_name(request)),
        WorkerAccess::Runtime => runtime(variant_name(request)),
        WorkerAccess::Coordinator => coordinator(variant_name(request)),
    }
}

fn progression_from_contract(request: &FrontendRequest) -> WorkerVerbDecision {
    match request_contract(request).progression {
        ProgressionAccess::Allow => WorkerVerbDecision::Allow,
        ProgressionAccess::Deny => WorkerVerbDecision::Deny(Box::new(WorkerTierDenial::progression(
            variant_name(request),
            WorkerTierDenialReason::ProgressionScope,
            "progression sessions are limited to reads and enabled proposal kinds.",
            None,
        ))),
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

#[cfg(test)]
mod operator_question_tests {
    use super::*;

    #[test]
    fn workers_can_read_questions_but_cannot_authorize_themselves() {
        assert!(
            worker_verb_decision(&FrontendRequest::ListOperatorQuestions {
                id: "task_example".into()
            })
            .is_allowed()
        );
        assert!(
            !worker_verb_decision(&FrontendRequest::AnswerOperatorQuestion {
                id: "oq_example".into(),
                answer: boss_protocol::OperatorAnswer::YesNo { value: true },
            })
            .is_allowed()
        );
    }
}
