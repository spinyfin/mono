//! Producer checkpoints commit with their normalized records, never ahead of them.
use anyhow::{Result, ensure};
use boss_protocol_spoke::v1::{Record, Run};
use rusqlite::{OptionalExtension, params};

use crate::{Admission, Fault, Journal, records};

impl Journal {
    pub fn latest_source(&self, run: &Run, prefix: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.db.query_row(
            "SELECT checkpoint FROM sources WHERE run_id=?1 AND spawn_token=?2 AND substr(source,1,length(?3))=?3 ORDER BY rowid DESC LIMIT 1",
            params![run.run_id, run.spawn_token, prefix], |row| row.get(0),
        ).optional()?)
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
        ensure!(old.as_deref() == expected, "producer checkpoint changed");
        let added = if let Some(old) = &old {
            checkpoint.len().saturating_sub(old.len())
        } else {
            checkpoint.len() + source.len() + 128
        };
        records::admit(&tx, added as u64, Admission::Lifecycle, self.limits)?;
        tx.execute(
            "INSERT INTO sources VALUES(?1,?2,?3,?4) ON CONFLICT(run_id,spawn_token,source) DO UPDATE SET checkpoint=excluded.checkpoint",
            params![run.run_id, run.spawn_token, source, checkpoint],
        )?;
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
}
