//! Tests for [`super`]'s execution-coordinator dispatch pipeline, grouped by
//! the concern each module exercises. Shared fixtures live in [`helpers`].

mod automation;
mod dispatch;
mod helpers;
mod pool;
mod recovery;
mod review_pause;
mod revision_gating;
mod spawn_failures;
mod unit;
