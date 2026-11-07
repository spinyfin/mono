//! Producer checkpoints commit with their normalized records, never ahead of them.
use anyhow::{Result, ensure};
use boss_protocol_spoke::v1::{Record, Run};
use rusqlite::{OptionalExtension, params};

use crate::{Admission, Fault, Journal, records};

/// Primary-key receipt for the most recently accepted hook of a run.
pub const LATEST_HOOK_SOURCE: &str = "hook";

impl Journal {
    pub fn latest_source(&self, run: &Run) -> Result<Option<Vec<u8>>> {
        self.source(run, LATEST_HOOK_SOURCE)
    }

    pub fn source(&self, run: &Run, source: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row(
                "SELECT checkpoint FROM sources WHERE run_id=?1 AND spawn_token=?2 AND source=?3",
                params![run.run_id, run.spawn_token, source],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Compare-and-swap prevents a concurrent hook/drain or tail from advancing
    /// an obsolete cursor. A duplicate checkpoint is a durable receipt too.
    pub fn commit_source(
        &mut self,
        run: &Run,
        source: &str,
        expected: Option<&[u8]>,
        checkpoint: &[u8],
        observations: Vec<(Record, Admission)>,
    ) -> Result<()> {
        ensure!(!source.is_empty() && source.len() <= 1024, "invalid producer identity");
        ensure!(checkpoint.len() <= 512 * 1024, "source checkpoint too large");
        let tx = self.db.transaction()?;
        let old: Option<Vec<u8>> = tx
            .query_row(
                "SELECT checkpoint FROM sources WHERE run_id=?1 AND spawn_token=?2 AND source=?3",
                params![run.run_id, run.spawn_token, source],
                |row| row.get(0),
            )
            .optional()?;
        if old.as_deref() == Some(checkpoint) {
            return Ok(());
        }
        if old.as_deref() != expected {
            return Err(crate::Fault::CheckpointConflict.into());
        }
        let mirror_latest = source.starts_with("hook:");
        let latest_old: Option<Vec<u8>> = if mirror_latest {
            tx.query_row(
                "SELECT checkpoint FROM sources WHERE run_id=?1 AND spawn_token=?2 AND source=?3",
                params![run.run_id, run.spawn_token, LATEST_HOOK_SOURCE],
                |row| row.get(0),
            )
            .optional()?
        } else {
            None
        };
        let mut added = if let Some(old) = &old {
            checkpoint.len().saturating_sub(old.len())
        } else {
            checkpoint.len() + source.len() + 128
        };
        if mirror_latest {
            added += match &latest_old {
                Some(old) if old.as_slice() == checkpoint => 0,
                Some(old) => checkpoint.len().saturating_sub(old.len()),
                None => checkpoint.len() + LATEST_HOOK_SOURCE.len() + 128,
            };
        }
        records::admit(&tx, added as u64, Admission::Lifecycle, self.limits)?;
        tx.execute(
            "INSERT INTO sources VALUES(?1,?2,?3,?4) ON CONFLICT(run_id,spawn_token,source) DO UPDATE SET checkpoint=excluded.checkpoint",
            params![run.run_id, run.spawn_token, source, checkpoint],
        )?;
        if mirror_latest {
            tx.execute(
                "INSERT INTO sources VALUES(?1,?2,?3,?4) ON CONFLICT(run_id,spawn_token,source) DO UPDATE SET checkpoint=excluded.checkpoint",
                params![run.run_id, run.spawn_token, LATEST_HOOK_SOURCE, checkpoint],
            )?;
        }
        for (mut record, class) in observations {
            ensure!(record.run.as_ref() == Some(run), "producer run mismatch");
            records::insert(&tx, &mut record, class, self.limits)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Do not number a bulk backlog ahead of lifecycle evidence. Receipts
    /// replenish this bounded window even while retained records pin the spool.
    pub fn bulk_credit(&self) -> Result<usize> {
        let (pending, limit): (u64, u64) = self.db.query_row(
            "SELECT COALESCE((SELECT SUM(length(bytes)+16) FROM records WHERE sequence>acked_record),0),result_limit FROM identity",
            [], |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let credit = limit.saturating_sub(pending).min(64 * 1024 + 4096);
        if credit <= 4096 {
            return Err(Fault::Full.into());
        }
        Ok((credit - 4096) as usize)
    }

    /// Drop producer receipts and transcript checkpoints after drain. Keep the
    /// tiny `drained` marker so reclaim can retry if home removal fails.
    pub fn prune_sources(&mut self, run: &Run) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "DELETE FROM sources WHERE run_id=?1 AND spawn_token=?2 AND source!='drained'",
            params![run.run_id, run.spawn_token],
        )?;
        tx.commit()?;
        Ok(())
    }
}
