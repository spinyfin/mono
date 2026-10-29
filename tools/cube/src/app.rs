//! CLI command implementations for `cube`.
//!
//! This module is a thin shim: it wires up the per-area submodules and
//! re-exports the crate's public surface (`run`, `RunResult`, `CubeError`).
//! All logic lives in the submodules, grouped by the area of the CLI they
//! serve.

mod bazel_output_base;
mod change;
mod checkleft_gate;
mod cleanup;
mod disk;
mod dispatch;
mod display;
mod errors;
mod excludes;
mod gc;
mod gh_pr;
mod health;
mod jj;
mod lease_reclaim;
mod periodic_cleanup;
mod pr;
mod pr_bookmarks;
mod pr_gc;
mod pr_rebase;
mod provision;
mod reclaim;
mod reconcile;
mod repo;
mod reset;
mod salvage;
mod stage;
pub(crate) mod store_gc;
mod util;
mod workspace;
mod workspace_ops;
pub(crate) mod workspace_status;

#[cfg(test)]
mod tests;

pub use dispatch::run;
pub use errors::{CubeError, RunResult};
