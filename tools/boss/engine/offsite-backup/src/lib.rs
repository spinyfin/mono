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
//! ## One complete file per backup
//!
//! A sync agent uploads whatever it finds in the destination, including a
//! file that is still being written, and treats later writes and a rename as
//! further changes to an item it is already uploading. So a copy is written
//! and fsynced in a staging directory outside the destination (owned by the
//! caller, on the engine's own state volume), then moved into the host
//! directory with a single rename and checked there. The agent sees each
//! backup as one complete new file that is never modified afterwards.
//!
//! If the destination is on another volume a rename cannot cross over, so
//! the copy is staged as an exclusive `state.db.bak-….<pid>.<sequence>.tmp`
//! sibling inside the host directory and renamed there. The final name still
//! only ever appears fully written.
//!
//! This crate returns errors and outcomes; metrics and the "never fail the
//! local backup" policy live in the engine, which depends on this crate and
//! not the other way round.

mod config;
mod copy;
mod retention;

pub use config::{CONFIG_SECTION, OffsiteConfig, ValidatedDestination, sanitize_host_component};
pub use copy::{BACKUP_FILE_PREFIX, CopyOutcome, backup_file_name, copy_open_to_offsite, copy_to_offsite};
pub use retention::{PruneOutcome, prune, prune_staging};
