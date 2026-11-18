//! Execution recovery through engine-created references in the shared jj store.
//!
//! [`execution_bookmark`] records where an execution started, preserves its
//! unpublished tip at teardown, restores that tip into a successor's
//! workspace, and forgets it once it is finished with. [`recovery_apply`]
//! retains the patch filtering and replay utilities for saved artifacts.
//!
//! This crate has a single one-way consumer edge: `boss-engine` (engine/core)
//! depends on it, never the reverse.

pub mod execution_bookmark;
pub mod recovery_apply;
