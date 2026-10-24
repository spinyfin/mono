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

    /// Claim the oldest command before any host effect. On restart an attempting
    /// command is returned with interrupted=true; the executor must reconcile it.
    pub fn begin_pending(&mut self) -> Result<Option<(Command, bool)>> {
        let tx = self.db.transaction()?;
        let pending: Option<(Vec<u8>, Vec<u8>, bool)> = tx
            .query_row(
                "SELECT bytes,digest,attempting FROM commands WHERE applied=0 ORDER BY sequence LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((bytes, digest, interrupted)) = pending else {
            return Ok(None);
        };
        ensure!(Sha256::digest(&bytes)[..] == digest, "corrupt command digest");
        let command = Command::decode(bytes.as_slice())?;
        tx.execute("UPDATE commands SET attempting=1 WHERE sequence=?1", [command.sequence])?;
        tx.commit()?;
        Ok(Some((command, interrupted)))
    }

    /// Commit result and application cursor atomically. A full spool leaves the
    /// command attempting, so callers retain the result and retry this commit.
    pub fn complete_command(&mut self, command: &Command, result: CommandResult, stamp: &Stamp) -> Result<()> {
        ensure!(result.command_id == command.command_id, "result command mismatch");
        let tx = self.db.transaction()?;
        let pending: Option<(u64, String, u64)> = tx
            .query_row(
                "SELECT sequence,command_id,length(bytes) FROM commands WHERE applied=0 AND attempting=1 ORDER BY sequence LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((sequence, command_id, payload_bytes)) = pending else {
            anyhow::bail!("result does not match oldest attempting command");
        };
        ensure!(
            (sequence, command_id) == (command.sequence, command.command_id.clone()),
            "result does not match oldest attempting command"
        );
        let mut record = Record {
            observed: Some(stamp.clone()),
            run: command.run.clone(),
            body: Some(record::Body::Result(result)),
            ..Default::default()
        };
        records::insert(&tx, &mut record, Admission::Lifecycle, self.limits)?;
        // Shrink the applied command to a tombstone (id, sequence, digest) so its
        // payload stops counting against the spool.
        tx.execute("UPDATE identity SET queued_bytes=queued_bytes-?1", [payload_bytes])?;
        tx.execute(
            "UPDATE commands SET applied=1,bytes=zeroblob(0) WHERE sequence=?1",
            [command.sequence],
        )?;
        tx.execute("UPDATE identity SET applied_command=?1", [command.sequence])?;
        tx.commit()?;
        Ok(())
    }

    /// Durable rejection for callers without an installed execution capability.
    pub fn reject_pending(&mut self, stamp: &Stamp) -> Result<bool> {
        let Some((command, _)) = self.begin_pending()? else {
            return Ok(false);
        };
        let reason = if command.not_after_unix_ms <= stamp.wall_unix_ms {
            "command expired"
        } else {
            "execution capability unavailable"
        };
        self.complete_command(
            &command,
            CommandResult {
                command_id: command.command_id.clone(),
                code: ResultCode::Rejected.into(),
                reason: reason.into(),
                value: None,
            },
            stamp,
        )?;
        Ok(true)
    }
}
