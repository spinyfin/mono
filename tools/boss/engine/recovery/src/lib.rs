//! Execution recovery through engine-created references in the shared jj store.
//!
//! [`execution_bookmark`] creates, validates, and restores execution references.
//! [`recovery_backup`] exports those references as supplementary patch evidence,
//! without reading the originating workspace. [`recovery_apply`] retains the
//! patch filtering and replay utilities for saved artifacts.
//!
//! This crate has a single one-way consumer edge: `boss-engine` (engine/core)
//! depends on it, never the reverse.

pub mod execution_bookmark;
pub mod recovery_apply;
pub mod recovery_backup;
