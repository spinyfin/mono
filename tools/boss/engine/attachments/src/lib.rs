//! Screenshot evidence for human reviewers: ingest, storage, retention, and
//! the loopback surface a reviewer actually looks at.
//!
//! ## The problem this crate exists for
//!
//! A worker that changes UI can render a screenshot but has nowhere to put it.
//! Committing capture PNGs to the branch is forbidden by repo policy — and
//! that policy is right: the one PR in this repo that ever linked
//! `raw.githubusercontent.com` images 404s today, because merging deleted the
//! branch the images lived on. But nothing replaced it, so reviewers of UI
//! changes get a worker's prose description of what it claims it saw. Evidence
//! a human cannot look at is not evidence.
//!
//! ## The shape
//!
//! - **Ingest** ([`store`]) — the worker hands the engine a *path*; the engine
//!   validates and reads it. Confined to the run's own workspace and temp
//!   dirs, format checked by magic number, size and dimensions capped. Every
//!   refusal is typed and loud.
//! - **Storage** ([`store::AttachmentStore`]) — content-addressed under the
//!   engine state root, so evidence outlives the ephemeral cube workspace that
//!   produced it and identical renders share one blob.
//! - **Retention** ([`retention`]) — age plus a total-bytes backstop, swept on
//!   a schedule and on demand, following the Codex/Grok home-retention
//!   precedent. Bounded and enforced, not deferred.
//! - **Surfacing** ([`http`]) — a loopback HTTP gallery. The worker pastes the
//!   gallery URL into its PR body; the reviewer clicks it from the GitHub PR
//!   and sees the images. That last step is the whole point: a mechanism that
//!   stores images but leaves the reviewer unable to see them has not solved
//!   anything.
//!
//! Ingest is mediated exactly like a worker proposal — the engine attributes
//! the call from the socket peer's pid and answers synchronously, so a bad
//! submission is a typed error the worker can fix mid-run. The RPC and
//! database halves live in `boss-engine-core` (`app/attachments.rs`,
//! `work/attachments.rs`); everything here is independent of them, so the
//! dependency edge runs one way.
//!
//! Design: `tools/boss/docs/designs/worker-screenshot-evidence-attachments.md`.

pub mod http;
mod image;
pub mod retention;
pub mod store;

pub use http::{EvidenceCatalog, WorkItemLabel};
pub use retention::{AttachmentRetentionPolicy, ReclaimPlan, RetainedAttachment, plan_reclaim};
pub use store::{AllowedRoots, AttachmentStore, IngestRejection, IngestedImage};
