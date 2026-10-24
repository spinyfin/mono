use anyhow::{Result, ensure};
use boss_protocol_spoke::{Validate, limits::MAX_FRAME_BYTES, v1::Record};
use prost::Message;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::{Fault, Journal, Limits, usage};

#[derive(Clone, Copy)]
pub enum Admission {
    Lifecycle,
    Capacity,
    Bulk,
    Capture,
}

pub struct StoredRecord {
    pub sequence: u64,
    pub bytes: Vec<u8>,
}

impl Journal {
    /// Sequence and immutable bytes become visible in the same FULL-sync commit.
    pub fn append(&mut self, mut record: Record, class: Admission) -> Result<u64> {
        let tx = self.db.transaction()?;
        let sequence = insert(&tx, &mut record, class, self.limits)?;
        tx.commit()?;
        Ok(sequence)
    }

    /// Reads one record at a time; transport never materializes the entire spool.
    pub fn next_record(&self, after: u64) -> Result<Option<StoredRecord>> {
        let entry: Option<(u64, Vec<u8>, Vec<u8>)> = self
            .db
            .query_row(
                "SELECT sequence,bytes,digest FROM records WHERE sequence>?1 ORDER BY sequence LIMIT 1",
                [after],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if entry.is_none() && after < self.record_bounds()?.1 {
            return Err(Fault::ReplayGap("missing record suffix".into()).into());
        }
        entry
            .map(|(sequence, bytes, digest)| {
                ensure!(sequence == after + 1, "replay_gap: noncontiguous records");
                ensure!(Sha256::digest(&bytes)[..] == digest, "corrupt journal record digest");
                Ok(StoredRecord { sequence, bytes })
            })
            .transpose()
    }

    /// Only a contiguous, receipted host-only prefix older than seven days.
    /// Run records remain pinned until a future assignment reconciliation API.
    pub fn compact(&mut self, now_ms: i64) -> Result<()> {
        const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM records WHERE sequence <= (SELECT acked_record FROM identity) AND sequence < COALESCE((SELECT MIN(sequence) FROM records WHERE host_only=0 OR created_ms>?1),9223372036854775807)", [now_ms.saturating_sub(WEEK_MS)])?;
        tx.commit()?;
        Ok(())
    }
}

pub(crate) fn insert(db: &Connection, record: &mut Record, class: Admission, limits: Limits) -> Result<u64> {
    record.sequence = db.query_row("SELECT next_record FROM identity", [], |r| r.get(0))?;
    ensure!(record.sequence < i64::MAX as u64, "journal sequence exhausted");
    record.validate()?;
    let bytes = record.encode_to_vec();
    ensure!(
        bytes.len() + 16 <= MAX_FRAME_BYTES as usize,
        "record exceeds frame limit"
    );
    admit(db, bytes.len() as u64 + 128, class, limits)?;
    let observed = record.observed.as_ref().expect("validated stamp");
    db.execute(
        "INSERT INTO records VALUES(?1,?2,?3,?4,?5)",
        params![
            record.sequence,
            bytes,
            &Sha256::digest(&bytes)[..],
            observed.wall_unix_ms,
            record.run.is_none()
        ],
    )?;
    db.execute("UPDATE identity SET next_record=next_record+1", [])?;
    Ok(record.sequence)
}

pub(crate) fn admit(db: &Connection, added: u64, class: Admission, limits: Limits) -> Result<()> {
    let ordinary = limits.spool_bytes - limits.lifecycle_reserve;
    let ceiling = match class {
        Admission::Lifecycle => limits.spool_bytes,
        Admission::Bulk => ordinary * 90 / 100,
        Admission::Capture => ordinary * 75 / 100,
        Admission::Capacity => ordinary,
    };
    if usage(db)?.saturating_add(added) > ceiling {
        return Err(Fault::Full.into());
    }
    Ok(())
}
