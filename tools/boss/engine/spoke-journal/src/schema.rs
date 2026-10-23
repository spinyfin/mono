use anyhow::{Result, ensure};
use rusqlite::Connection;

pub fn migrate(db: &mut Connection) -> Result<()> {
    let version: u32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        version <= 1,
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
                created_ms INTEGER NOT NULL, host_only INTEGER NOT NULL,
                epoch INTEGER NOT NULL, contract BLOB NOT NULL
            );
            CREATE TABLE commands (
                sequence INTEGER PRIMARY KEY, command_id TEXT NOT NULL UNIQUE,
                bytes BLOB NOT NULL, digest BLOB NOT NULL, applied INTEGER NOT NULL DEFAULT 0
            );
            CREATE TRIGGER records_added AFTER INSERT ON records BEGIN
                UPDATE identity SET queued_bytes=queued_bytes+length(new.bytes)+length(new.contract)+128;
            END;
            CREATE TRIGGER records_removed AFTER DELETE ON records BEGIN
                UPDATE identity SET queued_bytes=queued_bytes-length(old.bytes)-length(old.contract)-128;
            END;
            CREATE TRIGGER commands_added AFTER INSERT ON commands BEGIN
                UPDATE identity SET queued_bytes=queued_bytes+length(new.bytes)+128;
            END;
            PRAGMA user_version=1;
        ",
        )?;
        tx.commit()?;
    }
    Ok(())
}
