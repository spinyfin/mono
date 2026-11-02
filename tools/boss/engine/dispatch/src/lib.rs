//! Dispatch coordinator: admission, in-flight tracking, spillover and the
//! scheduling loop that turns `ready` executions into running workers.
//!
//! Atomic SQL claims stay in `boss_work_db`. Runner, host, completion and
//! lifecycle behaviour arrive through the `boss_execution_contracts`
//! interfaces the composition root injects, so this crate depends on no runner
//! or completion implementation and never on the engine runtime that consumes
//! it.

pub mod coordinator;
pub mod dispatch_inflight;
pub mod dispatch_metrics;
mod dispatch_spillover;

// Aliases under the module paths these sources have always used, so the
// moved coordinator code keeps resolving `crate::work`, `crate::metrics`, ...
use boss_conflict_diagnosis as conflict_diagnosis;
use boss_dispatch_events as dispatch_events;
use boss_host_execution::{cube as cube_commands, cube_op_budget};
use boss_metrics as metrics;
use boss_work_db as work;
