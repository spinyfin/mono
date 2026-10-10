//! Off-machine copies of the engine's finished `state.db` backups.
//!
//! The engine already takes consistent local snapshots (see
//! `boss_engine::database_backup`). This crate copies those finished
//! snapshots into a user-configured destination directory that some sync
//! agent (Google Drive for desktop, Dropbox, iCloud Drive, a NAS mount, ...)
//! replicates off the machine. It contains no provider logic: the
//! destination is just a directory.
//!
//! ## Layout
//!
//! `<destination>/<hostname>/state.db.bak-YYYYMMDD-HHMMSS`, so several
//! machines can share one destination.
//!
//! ## Atomicity
//!
//! A copy is streamed to an exclusive `state.db.bak-….<pid>.<sequence>.tmp` sibling in the host
//! directory, fsynced, then renamed into place. The final name therefore
//! only ever appears fully written.
//!
//! This crate returns errors and outcomes; metrics and the "never fail the
//! local backup" policy live in the engine, which depends on this crate and
//! not the other way round.

mod config;
mod copy;
mod retention;

pub use config::{CONFIG_SECTION, OffsiteConfig, ValidatedDestination, host_name, sanitize_host_component};
pub use copy::{BACKUP_FILE_PREFIX, CopyOutcome, backup_file_name, copy_open_to_offsite, copy_to_offsite};
pub use retention::{PruneOutcome, prune};
