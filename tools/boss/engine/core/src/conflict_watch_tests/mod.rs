//! Tests for [`super`]'s conflict detection/resolution pipeline, grouped by
//! the concern each module exercises. Shared fixtures live in [`helpers`].

mod churn;
mod detection;
mod escalation;
mod foreign_bucket;
mod gates;
mod helpers;
mod rearm;
mod resolution;
mod retire;
mod revision;
mod supersession;
mod trunk_coordination;
