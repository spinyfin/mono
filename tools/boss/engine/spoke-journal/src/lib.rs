//! Durable, single-owner spoke inbox and observation spool. No work authority.
mod commands;
mod records;
mod schema;

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Result, bail, ensure};
use boss_protocol_spoke::v1::{Cursor, Welcome};
use fs4::fs_std::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

pub use records::{Admission, StoredRecord};

/// Logical payload bounds, including retained records and command tombstones.
/// SQLite's page ceiling separately bounds physical database growth.
#[derive(Clone, Copy)]
pub struct Limits {
    pub spool_bytes: u64,
    pub lifecycle_reserve: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            spool_bytes: 8 * 1024 * 1024 * 1024,
            lifecycle_reserve: 64 * 1024 * 1024,
        }
    }
}

pub struct Journal {
    db: Connection,
    _lock: File,
    limits: Limits,
}

#[derive(Debug, thiserror::Error)]
pub enum Fault {
    #[error("replay_gap: {0}")]
    ReplayGap(String),
    #[error("spoke spool full; producer must retain its source")]
    Full,
}

impl Journal {
    pub fn open(root: &Path, host: &str, limits: Limits) -> Result<Self> {
        ensure!(boss_protocol_spoke::identity::is_valid_host_id(host), "invalid host id");
        ensure!(limits.spool_bytes > limits.lifecycle_reserve, "invalid spool limits");
        if !root.exists() {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(root)?;
        }
        let metadata = fs::symlink_metadata(root)?;
        ensure!(
            metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
            "state directory must be private (0700)"
        );
        let lock = private_file(&root.join("daemon.lock"))?;
        ensure!(FileExt::try_lock_exclusive(&lock)?, "spoke daemon lock is held");
        let db_path = root.join("journal.db");
        private_file(&db_path)?;
        let mut db = Connection::open(db_path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=64; PRAGMA journal_size_limit=1048576;")?;
        schema::migrate(&mut db)?;
        let pages = (limits.spool_bytes / 4096).max(64);
        db.pragma_update(None, "max_page_count", pages)?;
        let tx = db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO identity(singleton,host,journal) VALUES(1,?1,?2)",
            params![host, Uuid::new_v4().to_string()],
        )?;
        let stored: String = tx.query_row("SELECT host FROM identity", [], |r| r.get(0))?;
        ensure!(stored == host, "journal belongs to another host");
        tx.commit()?;
        // Persist newly created directory entries as well as SQLite's WAL commits.
        File::open(root)?.sync_all()?;
        Ok(Self {
            db,
            _lock: lock,
            limits,
        })
    }

    pub fn identity(&self) -> Result<(String, String)> {
        Ok(self
            .db
            .query_row("SELECT host,journal FROM identity", [], |r| Ok((r.get(0)?, r.get(1)?)))?)
    }

    pub fn record_bounds(&self) -> Result<(u64, u64)> {
        Ok(self.db.query_row(
            "SELECT COALESCE((SELECT MIN(sequence) FROM records),next_record),next_record-1 FROM identity",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    pub fn received_commands(&self) -> Result<Option<Cursor>> {
        let (id, through): (Option<String>, u64) =
            self.db
                .query_row("SELECT command_journal,received_command FROM identity", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
        Ok(id.map(|journal_id| Cursor { journal_id, through }))
    }

    pub fn applied_commands(&self) -> Result<u64> {
        Ok(self
            .db
            .query_row("SELECT applied_command FROM identity", [], |r| r.get(0))?)
    }

    pub fn bytes(&self) -> Result<u64> {
        usage(&self.db)
    }

    /// Bind once; a changed authority or lost history needs explicit reconciliation.
    /// A restored receipt may move backwards only within retained history.
    pub fn welcome(&mut self, welcome: &Welcome) -> Result<()> {
        let receipt = welcome
            .received_records
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing receipt"))?;
        self.check_receipt(receipt)?;
        let tx = self.db.transaction()?;
        let old: Option<(String, String, String)> = tx
            .query_row(
                "SELECT hub,authority,command_journal FROM identity WHERE hub IS NOT NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((hub, authority, commands)) = old
            && (hub != welcome.hub_id
                || authority != welcome.authority_generation
                || commands != welcome.command_journal_id)
        {
            return Err(Fault::ReplayGap("hub authority or command journal changed".into()).into());
        }
        tx.execute(
            "UPDATE identity SET hub=?1,authority=?2,command_journal=?3,acked_record=?4",
            params![
                welcome.hub_id,
                welcome.authority_generation,
                welcome.command_journal_id,
                receipt.through
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn receipt(&mut self, cursor: &Cursor) -> Result<()> {
        self.check_receipt(cursor)?;
        self.db.execute(
            "UPDATE identity SET acked_record=MAX(acked_record,?1)",
            [cursor.through],
        )?;
        Ok(())
    }

    fn check_receipt(&self, cursor: &Cursor) -> Result<()> {
        let (_, id) = self.identity()?;
        let (first, last) = self.record_bounds()?;
        if cursor.journal_id != id || cursor.through < first - 1 || cursor.through > last {
            return Err(Fault::ReplayGap(format!(
                "receipt {}:{} outside {id}:{}..={last}",
                cursor.journal_id,
                cursor.through,
                first - 1
            ))
            .into());
        }
        Ok(())
    }
}

fn usage(db: &Connection) -> Result<u64> {
    Ok(db.query_row("SELECT queued_bytes FROM identity", [], |r| r.get(0))?)
}

fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.permissions().mode() & 0o077 != 0 {
        bail!("journal files must be private regular files");
    }
    Ok(file)
}
