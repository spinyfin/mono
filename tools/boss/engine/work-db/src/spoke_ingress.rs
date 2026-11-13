//! Durable hub observation journal: connection generations, contiguous
//! immutable frames, and atomic command-result settlement.
//!
//! Payloads stay opaque bytes. The transport crate owns encoding; this module
//! keeps the WorkDb transaction that must not advance a receipt past a result
//! the command outbox cannot later query. Projection of work status is a
//! later consumer of these rows, not this write path.
use crate::spoke_hub::settle_host_command_in_tx;
use crate::*;

pub(crate) const MIGRATION_ID: &str = "20261109_1800_spoke_ingress_journal";

/// Most encoded observation bytes one host may hold unpruned on the hub.
pub const MAX_HOST_INGRESS_BYTES: i64 = 512 * 1024 * 1024;
/// Most observation frames one host may hold unpruned on the hub.
pub const MAX_HOST_INGRESS_RECORDS: i64 = 1_048_576;

/// Per-host bound on retained (not yet pruned) ingress.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IngressQuota {
    pub records: i64,
    pub bytes: i64,
}

const DEFAULT_QUOTA: IngressQuota = IngressQuota {
    records: MAX_HOST_INGRESS_RECORDS,
    bytes: MAX_HOST_INGRESS_BYTES,
};

/// The durable journal cursor advertised on Welcome and record receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpokeJournalAdmission {
    pub journal_id: String,
    pub generation: u64,
    pub received: u64,
}

/// A command result that must settle in the same transaction as its frame.
#[derive(Debug, Clone, Copy)]
pub struct SpokeIngressResult<'a> {
    pub command_id: &'a str,
    pub run: Option<&'a SpokeRunKey>,
    pub bytes: &'a [u8],
}

/// One observation to store, including every run it claims to describe.
#[derive(Debug, Clone, Copy, bon::Builder)]
#[builder(on(String, into))]
pub struct SpokeIngressCommit<'a> {
    pub host_id: &'a str,
    pub generation: u64,
    pub journal_id: &'a str,
    pub sequence: u64,
    pub frame: &'a [u8],
    pub epoch: u32,
    pub features: &'a str,
    pub observed_at_ms: i64,
    pub runs: &'a [SpokeRunKey],
    pub result: Option<SpokeIngressResult<'a>>,
    /// Flush collection promptly for control and proposal responses. Earlier
    /// records from this host still commit first.
    #[builder(default)]
    pub urgent: bool,
    /// When false, the receipt still advances and a frameless tombstone is
    /// stored so projection can skip the sequence, but the payload is not kept
    /// and does not count against the host's ingress quota. Pane captures are
    /// live display data: nothing replays them, and retaining each snapshot
    /// would fill the quota on an idle viewer.
    #[builder(default = true)]
    pub retain: bool,
}

fn fault(fault: HostOutboxFault) -> anyhow::Error {
    fault.into()
}

fn now_ms() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_ms() as i64
}

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS spoke_journals (
            host_id    TEXT PRIMARY KEY,
            journal_id TEXT NOT NULL,
            generation INTEGER NOT NULL DEFAULT 0,
            received   INTEGER NOT NULL DEFAULT 0,
            record_count   INTEGER NOT NULL DEFAULT 0,
            byte_count     INTEGER NOT NULL DEFAULT 0,
            pruned_through INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS spoke_ingress (
            host_id        TEXT NOT NULL,
            journal_id     TEXT NOT NULL,
            sequence       INTEGER NOT NULL,
            frame          BLOB NOT NULL,
            epoch          INTEGER NOT NULL,
            features       TEXT NOT NULL,
            observed_at_ms INTEGER NOT NULL,
            PRIMARY KEY (host_id, journal_id, sequence)
         );",
    )?;
    Ok(())
}

fn host_owns_run(tx: &rusqlite::Transaction<'_>, host_id: &str, run: &SpokeRunKey) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM spoke_assignments
          WHERE execution_id = ?1 AND assignment_generation = ?2 AND run_id = ?3
            AND spawn_token = ?4 AND host_id = ?5)",
        params![
            run.execution_id,
            run.assignment_generation as i64,
            run.run_id,
            run.spawn_token,
            host_id
        ],
        |row| row.get(0),
    )?)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Claim the next durable connection generation for `host_id`.
    ///
    /// A host with no journal is admitted on first contact when its journal
    /// still retains sequence 1: that host is already authenticated, which is
    /// the authorization a new journal needs, and the hub simply has received
    /// nothing yet, so the whole prefix replays. A first Hello whose journal
    /// has compacted past sequence 1 is a replay gap. A later Hello that names
    /// a different journal, or that cannot cover the hub's receipt, is also a
    /// replay gap. Failure does not increment the generation.
    pub fn open_spoke_journal(
        &self,
        host_id: &str,
        journal_id: &str,
        last_record: u64,
        first_retained: u64,
    ) -> Result<SpokeJournalAdmission> {
        if host_id.is_empty() || journal_id.is_empty() {
            return Err(fault(HostOutboxFault::Denied));
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT journal_id, generation, received FROM spoke_journals WHERE host_id = ?1",
                [host_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let admission = match stored {
            None => {
                if first_retained > 1 {
                    return Err(fault(HostOutboxFault::ReplayGap));
                }
                tx.execute(
                    "INSERT INTO spoke_journals(host_id, journal_id, generation, received)
                     VALUES (?1, ?2, 1, 0)",
                    params![host_id, journal_id],
                )?;
                SpokeJournalAdmission {
                    journal_id: journal_id.to_owned(),
                    generation: 1,
                    received: 0,
                }
            }
            Some((id, generation, received)) => {
                if id != journal_id
                    || (received as u64) > last_record
                    || first_retained > (received as u64).saturating_add(1)
                {
                    return Err(fault(HostOutboxFault::ReplayGap));
                }
                let next = generation + 1;
                tx.execute(
                    "UPDATE spoke_journals SET generation = ?2 WHERE host_id = ?1",
                    params![host_id, next],
                )?;
                SpokeJournalAdmission {
                    journal_id: id,
                    generation: next as u64,
                    received: received as u64,
                }
            }
        };
        tx.commit()?;
        Ok(admission)
    }

    /// Store one contiguous (or identical duplicate) frame and advance the
    /// durable receipt in the same transaction. A result is settled here so a
    /// crash after receipt still answers `host_command_result`.
    pub fn commit_spoke_record(&self, commit: SpokeIngressCommit<'_>) -> Result<u64> {
        self.commit_spoke_record_within(commit, DEFAULT_QUOTA)
    }

    pub(crate) fn commit_spoke_record_within(
        &self,
        commit: SpokeIngressCommit<'_>,
        quota: IngressQuota,
    ) -> Result<u64> {
        let host = commit.host_id.to_owned();
        let journal = commit.journal_id.to_owned();
        let frame = commit.frame.to_vec();
        let features = commit.features.to_owned();
        let runs = commit.runs.to_vec();
        let result = commit
            .result
            .map(|r| (r.command_id.to_owned(), r.run.cloned(), r.bytes.to_vec()));
        let generation = commit.generation;
        let sequence = commit.sequence;
        let epoch = commit.epoch;
        let observed_at_ms = commit.observed_at_ms;
        let retain = commit.retain;
        let urgent = commit.urgent || result.is_some();
        self.ingress_writer()?
            .submit(host.clone(), frame.len(), urgent, move |tx| {
                let result = result.as_ref().map(|(command_id, run, bytes)| SpokeIngressResult {
                    command_id,
                    run: run.as_ref(),
                    bytes,
                });
                commit_in_tx(
                    tx,
                    SpokeIngressCommit::builder()
                        .host_id(&host)
                        .generation(generation)
                        .journal_id(&journal)
                        .sequence(sequence)
                        .frame(&frame)
                        .epoch(epoch)
                        .features(&features)
                        .observed_at_ms(observed_at_ms)
                        .runs(&runs)
                        .maybe_result(result)
                        .retain(retain)
                        .build(),
                    quota,
                )
            })?
            .wait()
    }

    /// Drop stored frames at or below `through` once a consumer no longer
    /// needs them, releasing their share of the host's quota. Only frames the
    /// hub has already received can be pruned; the receipt never moves. Returns
    /// the number of frames removed.
    pub fn prune_spoke_ingress(&self, host_id: &str, through: u64) -> Result<u64> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT journal_id, received, pruned_through FROM spoke_journals WHERE host_id = ?1",
                [host_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((journal_id, received, pruned_through)) = stored else {
            return Err(fault(HostOutboxFault::Denied));
        };
        let through = through.min(received as u64) as i64;
        if through <= pruned_through {
            return Ok(0);
        }
        let (count, bytes): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(frame)), 0) FROM spoke_ingress
              WHERE host_id = ?1 AND journal_id = ?2 AND sequence <= ?3 AND LENGTH(frame) > 0",
            params![host_id, journal_id, through],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        tx.execute(
            "DELETE FROM spoke_ingress WHERE host_id = ?1 AND journal_id = ?2 AND sequence <= ?3",
            params![host_id, journal_id, through],
        )?;
        tx.execute(
            "UPDATE spoke_journals
                SET record_count = record_count - ?2, byte_count = byte_count - ?3, pruned_through = ?4
              WHERE host_id = ?1",
            params![host_id, count, bytes, through],
        )?;
        tx.commit()?;
        Ok(count as u64)
    }

    /// The durable journal a host last admitted, if any.
    pub fn spoke_journal(&self, host_id: &str) -> Result<Option<SpokeJournalAdmission>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT journal_id, generation, received FROM spoke_journals WHERE host_id = ?1",
                [host_id],
                |row| {
                    Ok(SpokeJournalAdmission {
                        journal_id: row.get(0)?,
                        generation: row.get::<_, i64>(1)? as u64,
                        received: row.get::<_, i64>(2)? as u64,
                    })
                },
            )
            .optional()?)
    }

    /// Original encoded frame bytes stored at `sequence`, if any.
    pub fn spoke_ingress_frame(&self, host_id: &str, sequence: u64) -> Result<Option<Vec<u8>>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT frame FROM spoke_ingress WHERE host_id = ?1 AND sequence = ?2",
                params![host_id, sequence as i64],
                |row| row.get(0),
            )
            .optional()?)
    }
}

fn commit_in_tx(tx: &rusqlite::Transaction<'_>, commit: SpokeIngressCommit<'_>, quota: IngressQuota) -> Result<u64> {
    if commit.sequence == 0 || commit.frame.is_empty() || (!commit.retain && commit.result.is_some()) {
        return Err(fault(HostOutboxFault::Denied));
    }
    let stored: Option<(String, i64, i64, i64, i64, i64)> = tx
        .query_row(
            "SELECT journal_id, generation, received, record_count, byte_count, pruned_through
                   FROM spoke_journals WHERE host_id = ?1",
            [commit.host_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((journal_id, generation, received, record_count, byte_count, pruned_through)) = stored else {
        return Err(fault(HostOutboxFault::Denied));
    };
    if journal_id != commit.journal_id || generation as u64 != commit.generation {
        return Err(fault(HostOutboxFault::Denied));
    }
    let received = received as u64;
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT frame FROM spoke_ingress
                 WHERE host_id = ?1 AND journal_id = ?2 AND sequence = ?3",
            params![commit.host_id, commit.journal_id, commit.sequence as i64],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(previous) = existing {
        if previous.is_empty() {
            // A frameless tombstone marks an intentionally unretained
            // sequence. Retransmitting it as retained or as a result is a
            // lie; another unretained frame is already receipted.
            if commit.retain || commit.result.is_some() {
                return Err(fault(HostOutboxFault::Denied));
            }
            return Ok(received);
        }
        if previous != commit.frame {
            return Err(fault(HostOutboxFault::Denied));
        }
        if commit.sequence > received {
            return Err(fault(HostOutboxFault::Denied));
        }
        return Ok(received);
    }
    if commit.sequence <= pruned_through as u64 {
        // Already received and since pruned: the original bytes are gone,
        // so the duplicate cannot be compared. A result is acknowledged
        // only when that command already holds a durable result whose run
        // and bytes both match; otherwise the hub would report a result it
        // never persisted, or bind a pending command to a vanished frame.
        if let Some(result) = commit.result {
            let settled: bool = tx
                .query_row(
                    "SELECT result IS NOT NULL FROM host_commands
                          WHERE host_id = ?1 AND command_id = ?2",
                    params![commit.host_id, result.command_id],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(false);
            if !settled {
                return Err(fault(HostOutboxFault::Denied));
            }
            settle_host_command_in_tx(tx, commit.host_id, result.command_id, result.run, result.bytes)?;
        }
        return Ok(received);
    }
    if commit.sequence <= received {
        // Already receipted and since pruned (including an unretained
        // tombstone). Results are always stored, so a result at such a
        // sequence is a lie. A retained-type frame at a vanished sequence
        // is also a lie: the hub never kept those bytes.
        if commit.result.is_some() || commit.retain {
            return Err(fault(HostOutboxFault::Denied));
        }
        return Ok(received);
    }
    if commit.sequence != received + 1 {
        return Err(fault(HostOutboxFault::ReplayGap));
    }
    for run in commit.runs {
        if !host_owns_run(tx, commit.host_id, run)? {
            return Err(fault(HostOutboxFault::Denied));
        }
    }
    if let Some(result) = commit.result {
        settle_host_command_in_tx(tx, commit.host_id, result.command_id, result.run, result.bytes)?;
    }
    if commit.retain {
        let frame_len = commit.frame.len() as i64;
        if record_count >= quota.records || byte_count + frame_len > quota.bytes {
            return Err(fault(HostOutboxFault::Full));
        }
        tx.execute(
            "INSERT INTO spoke_ingress
                    (host_id, journal_id, sequence, frame, epoch, features, observed_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                commit.host_id,
                commit.journal_id,
                commit.sequence as i64,
                commit.frame,
                commit.epoch as i64,
                commit.features,
                if commit.observed_at_ms == 0 {
                    now_ms()
                } else {
                    commit.observed_at_ms
                }
            ],
        )?;
        tx.execute(
            "UPDATE spoke_journals
                    SET received = ?2, record_count = record_count + 1, byte_count = byte_count + ?3
                  WHERE host_id = ?1",
            params![commit.host_id, commit.sequence as i64, frame_len],
        )?;
    } else {
        // Empty blob: durable evidence of an intentionally unretained
        // sequence so the projector can skip it instead of halting on a hole.
        tx.execute(
            "INSERT INTO spoke_ingress
                    (host_id, journal_id, sequence, frame, epoch, features, observed_at_ms)
                 VALUES (?1, ?2, ?3, X'', ?4, ?5, ?6)",
            params![
                commit.host_id,
                commit.journal_id,
                commit.sequence as i64,
                commit.epoch as i64,
                commit.features,
                if commit.observed_at_ms == 0 {
                    now_ms()
                } else {
                    commit.observed_at_ms
                }
            ],
        )?;
        tx.execute(
            "UPDATE spoke_journals SET received = ?2 WHERE host_id = ?1",
            params![commit.host_id, commit.sequence as i64],
        )?;
    }
    Ok(commit.sequence)
}

#[cfg(test)]
#[path = "spoke_ingress_tests.rs"]
mod tests;
