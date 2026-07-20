//! Typed client for Trunk's merge-queue REST API
//! (`https://api.trunk.io/v1`).
//!
//! # Crate boundary
//!
//! This crate owns Trunk queue transport + protocol ONLY: the six queue
//! endpoints Boss calls (`submitPullRequest`, `getSubmittedPullRequest`,
//! `listPullRequests`, `getQueue`, `cancelPullRequest`,
//! `restartTestsOnPullRequest`), their request/response types, the
//! [`TrunkError`] taxonomy, and retry/backoff. It must NEVER import from the
//! engine — that edge is one-way, engine -> `boss-trunk-client`. It also
//! knows nothing about *where* the org API token lives (Keychain, env var,
//! config file); callers supply one via [`TrunkTokenProvider`].
//!
//! Per the design (`docs/designs/trunk-merge-queue-integration-*.md`), queue
//! *administration* (creating/pausing/deleting queues) and the Prometheus
//! `getMergeQueueMetrics` endpoint are deliberately out of scope — the only
//! write verbs this crate exposes are entry-level.

mod client;
mod error;
mod models;
mod secret;

pub use client::{CallConfig, RetryPolicy, TRUNK_API_BASE_URL, TrunkClient};
pub use error::TrunkError;
pub use models::{
    GetQueueRequest, ListPullRequestsRequest, ListPullRequestsResponse, SubmitPullRequestRequest, TrunkPrLookup,
    TrunkPrRef, TrunkPrState, TrunkPriority, TrunkPullRequest, TrunkQueue, TrunkQueueState, TrunkRepoRef,
};
pub use secret::{SecretString, StaticTokenProvider, TrunkTokenProvider};
