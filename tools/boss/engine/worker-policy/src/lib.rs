//! The worker exposure boundary: which frontend verbs a worker session may
//! call, and what is stripped from the rows it gets back.
//!
//! Both halves are pure functions over the wire types, with no engine state
//! and no database, so the boundary can be exercised exhaustively in unit
//! tests rather than only through a live socket. The engine supplies the one
//! thing this crate cannot know — whether the connection on the other end
//! *is* a worker (`ServerState::classify_peer`, from the socket peer's
//! process ancestry) — and then applies both halves at a single choke point
//! per connection.
//!
//! ## Why this is a crate and not a module
//!
//! The policy table is the security-relevant part of the mediation design and
//! it wants a fast, isolated test loop: 171 verbs, each of which has to be
//! deliberately classified. Keeping it out of `boss-engine`'s
//! rebuild/retest scope means changing an allow/deny decision recompiles a
//! small leaf crate, mirroring the `proposal-validation` split from the
//! previous task in this project.
//!
//! ## The two halves
//!
//! - [`policy`] — [`verb_decision`], a tier-keyed, deny-by-default classification
//!   of every [`FrontendRequest`]. Deliberately an exhaustive `match` with no
//!   wildcard arm: a newly added verb fails to compile here until someone
//!   decides whether workers may call it. That compile error is the point.
//! - [`proposal_kind_decision`] — an exhaustive second gate on proposal kinds.
//!   Progression permits reads and proposal submission, but every kind stays
//!   closed until its scoped mutation handler is implemented.
//! - [`sanitize`] — [`sanitize_event_for_worker`], which strips runtime-half
//!   fields from execution and run rows on their way out to a worker.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Transport and authn: the worker RPC tier" and §"Read-only model access
//! and the exposure boundary".

mod policy;
mod sanitize;
mod tiers;

#[cfg(test)]
mod tests;

pub use policy::{WorkerVerbDecision, variant_name, verb_decision, worker_verb_decision};
pub use sanitize::{SANITIZED_EXECUTION_FIELDS, sanitize_event_for_worker};
pub use tiers::{RpcTier, proposal_kind_decision};
