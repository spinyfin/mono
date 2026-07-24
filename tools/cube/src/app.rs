//! CLI command implementations for `cube`.
//!
//! This module is a thin shim: it wires up the per-area submodules and
//! re-exports the crate's public surface (`run`, `RunResult`, `CubeError`).
//! All logic lives in the submodules, grouped by the area of the CLI they
//! serve.

mod change;
mod checkleft_gate;
mod dispatch;
mod display;
mod errors;
mod excludes;
mod gc;
mod health;
mod jj;
mod pr;
mod provision;
mod reconcile;
mod repo;
mod reset;
mod stage;
mod util;
mod workspace;
mod workspace_ops;

#[cfg(test)]
mod tests;

pub use dispatch::run;
pub use errors::{CubeError, RunResult};
