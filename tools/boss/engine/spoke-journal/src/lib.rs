//! Durable, single-owner spoke inbox and observation spool. No work authority.
mod commands;
mod records;
mod schema {
    use anyhow::{Result, ensure};
    use rusqlite::Connection;

    pub fn migrate(db: &mut Connection) -> Result<()> {
        let version: u32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= 3,
            "journal schema is newer than this reader; rollback refused"
        );
        if version == 0 {
            let tx = db.transaction()?;
            tx.execute_batch(
                "
                CREATE TABLE identity (
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1), host TEXT NOT NULL,
                    journal TEXT NOT NULL, next_record INTEGER NOT NULL DEFAULT 1,
                    acked_record INTEGER NOT NULL DEFAULT 0, hub TEXT, authority TEXT,
                    command_journal TEXT, received_command INTEGER NOT NULL DEFAULT 0,
                    applied_command INTEGER NOT NULL DEFAULT 0,
                    queued_bytes INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE records (
                    sequence INTEGER PRIMARY KEY, bytes BLOB NOT NULL, digest BLOB NOT NULL,
                    created_ms INTEGER NOT NULL, host_only INTEGER NOT NULL
                );
                CREATE TABLE commands (
                    sequence INTEGER PRIMARY KEY, command_id TEXT NOT NULL UNIQUE,
                    bytes BLOB NOT NULL, digest BLOB NOT NULL, applied INTEGER NOT NULL DEFAULT 0
                );
                CREATE TRIGGER records_added AFTER INSERT ON records BEGIN
                    UPDATE identity SET queued_bytes=queued_bytes+length(new.bytes)+128;
                END;
                CREATE TRIGGER records_removed AFTER DELETE ON records BEGIN
                    UPDATE identity SET queued_bytes=queued_bytes-length(old.bytes)-128;
                END;
                CREATE TRIGGER commands_added AFTER INSERT ON commands BEGIN
                    UPDATE identity SET queued_bytes=queued_bytes+length(new.bytes)+128;
                END;
                PRAGMA user_version=1;
            ",
            )?;
            tx.commit()?;
        }
        if version < 2 {
            let tx = db.transaction()?;
            tx.execute_batch(
                "ALTER TABLE commands ADD COLUMN attempting INTEGER NOT NULL DEFAULT 0; PRAGMA user_version=2;",
            )?;
            tx.commit()?;
        }
        if version < 3 {
            let tx = db.transaction()?;
            tx.execute_batch(
                "ALTER TABLE commands ADD COLUMN recovery BLOB;
                 ALTER TABLE identity ADD COLUMN result_limit INTEGER NOT NULL DEFAULT 262144;
                 PRAGMA user_version=3;",
            )?;
            tx.commit()?;
        }
        Ok(())
    }
}

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
    #[error("negotiated transport limits cannot carry retained command identities or records")]
    TransportLimit,
}

impl Fault {
    /// Whether `error` is spool admission backpressure rather than a disk or
    /// integrity failure.
    pub fn is_full(error: &anyhow::Error) -> bool {
        matches!(error.downcast_ref::<Fault>(), Some(Fault::Full))
    }
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
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON; PRAGMA wal_autocheckpoint=64; PRAGMA journal_size_limit=1048576;")?;
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
        let limits = boss_protocol_spoke::limits::Limits::default()
            .negotiated(welcome.max_frame_bytes, welcome.max_inflight_bytes);
        let largest_record: usize = tx.query_row(
            "SELECT COALESCE(MAX(length(bytes)),0) FROM records WHERE sequence>?1",
            [receipt.through],
            |r| r.get(0),
        )?;
        let mut pending = tx.prepare("SELECT bytes FROM commands WHERE applied=0")?;
        let mut rows = pending.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            let command = <boss_protocol_spoke::v1::Command as prost::Message>::decode(bytes.as_slice())?;
            if commands::result_identity_bound(&command)
                > limits.max_frame_bytes.min(limits.max_inflight_bytes) as usize
            {
                return Err(Fault::TransportLimit.into());
            }
        }
        drop(rows);
        drop(pending);
        if largest_record > 0 && largest_record + 16 > limits.max_frame_bytes as usize {
            return Err(Fault::TransportLimit.into());
        }
        tx.execute(
            "UPDATE identity SET result_limit=?1",
            [limits.max_frame_bytes.min(limits.max_inflight_bytes)],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn receipt(&mut self, cursor: &Cursor) -> Result<()> {
        self.check_receipt(cursor)?;
        // The hub's receipt is re-sent in every Welcome, so losing this write to a
        // crash costs nothing. Skip the full-fsync barrier that record and command
        // commits need, rather than paying it once per receipted record.
        self.db.pragma_update(None, "synchronous", "NORMAL")?;
        let updated = self.db.execute(
            "UPDATE identity SET acked_record=MAX(acked_record,?1)",
            [cursor.through],
        );
        self.db.pragma_update(None, "synchronous", "FULL")?;
        updated?;
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
