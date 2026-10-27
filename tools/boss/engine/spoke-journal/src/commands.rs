use anyhow::{Result, ensure};
use boss_protocol_spoke::{
    Validate,
    limits::MAX_FRAME_BYTES,
    v1::{Command, CommandResult, Record, ResultCode, Stamp, record},
};
use prost::Message;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::{Admission, Journal, records};

impl Journal {
    /// Raw bytes are retained, including fields unknown to this binary.
    /// No receipt may be sent until this method returns successfully.
    pub fn receive_command(&mut self, bytes: &[u8]) -> Result<u64> {
        ensure!(
            bytes.len() + 16 <= MAX_FRAME_BYTES as usize,
            "command exceeds frame limit"
        );
        let command = Command::decode(bytes)?;
        command.validate()?;
        ensure!(command.sequence < i64::MAX as u64, "command sequence exhausted");
        let tx = self.db.transaction()?;
        let (journal, through): (Option<String>, u64) =
            tx.query_row("SELECT command_journal,received_command FROM identity", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        ensure!(journal.is_some(), "command before Welcome");
        let previous: Option<Vec<u8>> = tx
            .query_row(
                "SELECT digest FROM commands WHERE sequence=?1 OR command_id=?2",
                params![command.sequence, command.command_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            ensure!(
                previous[..] == Sha256::digest(bytes)[..],
                "command identity reused with different bytes"
            );
            return Ok(through);
        }
        ensure!(command.sequence == through + 1, "command sequence gap");
        records::admit(&tx, bytes.len() as u64 + 128, Admission::Lifecycle, self.limits)?;
        tx.execute(
            "INSERT INTO commands(sequence,command_id,bytes,digest) VALUES(?1,?2,?3,?4)",
            params![command.sequence, command.command_id, bytes, &Sha256::digest(bytes)[..]],
        )?;
        tx.execute("UPDATE identity SET received_command=?1", [command.sequence])?;
        tx.commit()?;
        Ok(command.sequence)
    }

    /// The foundation daemon has no executor. Reject durably in order, including
    /// commands received just before a crash. Receipt alone never means success.
    pub fn reject_pending(&mut self, stamp: &Stamp) -> Result<bool> {
        let tx = self.db.transaction()?;
        let pending: Option<(u64, Vec<u8>, Vec<u8>)> = tx
            .query_row(
                "SELECT sequence,bytes,digest FROM commands WHERE applied=0 ORDER BY sequence LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((sequence, bytes, digest)) = pending else {
            return Ok(false);
        };
        ensure!(Sha256::digest(&bytes)[..] == digest, "corrupt command digest");
        let command = Command::decode(bytes.as_slice())?;
        let reason = if command.not_after_unix_ms <= stamp.wall_unix_ms {
            "command expired"
        } else {
            "execution capability unavailable"
        };
        let mut result = Record {
            observed: Some(stamp.clone()),
            run: command.run,
            body: Some(record::Body::Result(CommandResult {
                command_id: command.command_id,
                code: ResultCode::Rejected.into(),
                reason: reason.into(),
                value: None,
            })),
            ..Default::default()
        };
        records::insert(&tx, &mut result, Admission::Lifecycle, self.limits)?;
        // Shrink the applied command to a tombstone (id, sequence, digest) so its
        // payload stops counting against the spool.
        tx.execute("UPDATE identity SET queued_bytes=queued_bytes-?1", [bytes.len() as u64])?;
        tx.execute(
            "UPDATE commands SET applied=1,bytes=zeroblob(0) WHERE sequence=?1",
            [sequence],
        )?;
        tx.execute("UPDATE identity SET applied_command=?1", [sequence])?;
        tx.commit()?;
        Ok(true)
    }
}
