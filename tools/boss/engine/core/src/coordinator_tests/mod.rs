//! Tests for [`super`]'s execution-coordinator dispatch pipeline, grouped by
//! the concern each module exercises. Shared fixtures live in [`helpers`].

mod automation;
mod claimed_dispatch;
mod dispatch;
mod helpers;
mod pause_admission;
mod pause_bypass;
mod pool;
mod recovery;
mod review_pause;
mod revision_gating;
mod spawn_failures;
mod unit;
