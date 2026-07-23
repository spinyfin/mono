//! Wire-level types shared between `boss-engine` and the `boss` CLI.
//!
//! This module is a facade. Every type lives in a focused submodule below and
//! is re-exported here, so downstream code keeps importing from
//! `boss_protocol::types` (or the crate root) exactly as before.
//!
//! Field-ordering convention (applies to every struct in these submodules):
//!
//!   1. Identity fields first: `id`, `short_id`, primary FK identifiers.
//!
//!   2. Required (non-`Option`) fields, alphabetical within this group.
//!
//!   3. Optional (`Option<T>`) fields, alphabetical within this group.
//!
//! New struct *definitions* go in alphabetical order by type name within
//! their submodule. Both orderings reduce merge conflicts when adding new
//! structs or fields. Serde JSON and Swift Codable are both name-keyed, so
//! field order does not affect wire format.
//!
//! The pre-split file had drifted from the alphabetical ordering in places.
//! Splitting it preserved each type's existing relative order rather than
//! reshuffling, to keep the split reviewable as a pure move, so some
//! submodules are not strictly sorted yet.

mod attention;
mod automation;
mod ci;
mod comment;
mod common;
mod context;
mod dependency;
mod execution;
mod planner_run;
mod product;
mod project;
mod proposal;
mod task;
mod work_item;
mod worker_tier;

#[cfg(test)]
mod tests;

pub use attention::*;
pub use automation::*;
pub use ci::*;
pub use comment::*;
pub use common::*;
pub use context::*;
pub use dependency::*;
pub use execution::*;
pub use planner_run::*;
pub use product::*;
pub use project::*;
pub use proposal::*;
pub use task::*;
pub use work_item::*;
pub use worker_tier::*;
