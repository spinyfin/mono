//! Tests for [`super`]'s execution-coordinator dispatch pipeline, grouped by
//! the concern each module exercises. Shared fixtures live in [`helpers`].

mod automation;
mod blocked_workspace;
mod claimed_dispatch;
mod dispatch;
mod execution_bookmarks;
mod helpers;
mod local_worker_quarantine;
mod pause_admission;
mod pause_bypass;
mod pool;
mod post_merge_review_dispatch;
mod recovery;
mod review_batch_dispatch;
mod review_pause;
mod revision_gating;
mod spawn_failures;
mod unit;
