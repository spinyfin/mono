//! Tests for [`super`]'s CI-watch detection/remediation pipeline, grouped by
//! the concern each module exercises. Shared fixtures live in [`helpers`].

mod back_to_back;
mod detection;
mod helpers;
mod noop_validation;
mod pre_triage;
mod rebase_first;
mod rebounce;
mod revision;
mod revision_description;
mod trunk_eviction;
