//! Crash-recovery patch flow for dead executions.
//!
//! When the engine detects that an execution has died with uncommitted work
//! in its leased cube workspace, [`recovery_backup`] captures that work to a
//! durable patch file keyed by execution id, and [`recovery_apply`] later
//! locates, filters, and replays that patch into the resuming worker's
//! workspace. The two halves share the patch naming and bookkeeping filter so
//! a patch the backup path writes is exactly the one the apply path reads.
//!
//! This crate has a single one-way consumer edge: `boss-engine` (engine/core)
//! depends on it, never the reverse.

pub mod recovery_apply;
pub mod recovery_backup;
