use anyhow::{Result, ensure};
use boss_protocol_spoke::{
    Validate,
    limits::MAX_FRAME_BYTES,
    v1::{Command, CommandResult, Record, ResultCode, Stamp, WorkspaceKind, command, record},
};
use prost::Message;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::{Admission, Fault, Journal, records};

/// Includes stamp, sequence, result diagnostic and protobuf envelopes.
pub(crate) fn result_identity_bound(command: &Command) -> usize {
    command.command_id.len() + command.run.as_ref().map_or(0, Message::encoded_len) + 512
}

impl Journal {
    pub fn recovery_snapshot(&self, sequence: u64) -> Result<Option<Vec<u8>>> {
        Ok(self.db.query_row(
            "SELECT recovery FROM commands WHERE sequence=?1 AND attempting=1 AND applied=0",
            [sequence],
            |r| r.get(0),
        )?)
    }

    /// FULL-sync commit before any operation that may move recovery pointers.
    pub fn save_recovery_snapshot(&mut self, sequence: u64, bytes: &[u8]) -> Result<()> {
        let tx = self.db.transaction()?;
        records::admit(&tx, bytes.len() as u64, Admission::Lifecycle, self.limits)?;
        let changed = tx.execute(
            "UPDATE commands SET recovery=?1 WHERE sequence=?2 AND attempting=1 AND applied=0 AND recovery IS NULL",
            params![bytes, sequence],
        )?;
        ensure!(
            changed == 1,
            "recovery snapshot already saved or command is not attempting"
        );
        tx.execute("UPDATE identity SET queued_bytes=queued_bytes+?1", [bytes.len() as u64])?;
        tx.commit()?;
        Ok(())
    }

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
        let result_limit: usize = tx.query_row("SELECT result_limit FROM identity", [], |r| r.get(0))?;
        if result_identity_bound(&command) > result_limit {
            return Err(Fault::TransportLimit.into());
        }
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
        let was_rejected = result.code == ResultCode::Rejected as i32;
        let tx = self.db.transaction()?;
        let pending: Option<(u64, String, u64)> = tx
            .query_row(
                "SELECT sequence,command_id,length(bytes)+COALESCE(length(recovery),0) FROM commands WHERE applied=0 AND attempting=1 ORDER BY sequence LIMIT 1",
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
        let limit: usize = tx.query_row("SELECT result_limit FROM identity", [], |r| r.get(0))?;
        // Reserve the maximum sequence varint and the enclosing SpokeFrame tag/length.
        if record.encoded_len() + 16 > limit {
            let read_only = matches!(&command.body, Some(command::Body::Workspace(op))
                if matches!(WorkspaceKind::try_from(op.kind),
                    Ok(WorkspaceKind::ListRepos | WorkspaceKind::ListWorkspaces | WorkspaceKind::Status | WorkspaceKind::DiffBookmark)));
            record.body = Some(record::Body::Result(CommandResult {
                command_id: command.command_id.clone(),
                code: if read_only || was_rejected {
                    ResultCode::Rejected
                } else {
                    ResultCode::OutcomeUncertain
                }
                .into(),
                reason: "result exceeds negotiated transport limit; request a bounded read or reconcile effects".into(),
                value: None,
            }));
        }
        ensure!(
            record.encoded_len() + 16 <= limit,
            "negotiated limit cannot fit command result identity"
        );
        records::insert(&tx, &mut record, Admission::Lifecycle, self.limits)?;
        // Release the command payload and its durable recovery snapshot.
        tx.execute("UPDATE identity SET queued_bytes=queued_bytes-?1", [payload_bytes])?;
        tx.execute(
            "UPDATE commands SET applied=1,bytes=zeroblob(0),recovery=NULL WHERE sequence=?1",
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
