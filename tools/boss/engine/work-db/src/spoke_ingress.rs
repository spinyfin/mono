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
        if commit.sequence == 0 || commit.frame.is_empty() {
            return Err(fault(HostOutboxFault::Denied));
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
            if previous != commit.frame {
                return Err(fault(HostOutboxFault::Denied));
            }
            if commit.sequence > received {
                return Err(fault(HostOutboxFault::Denied));
            }
            tx.commit()?;
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
                settle_host_command_in_tx(&tx, commit.host_id, result.command_id, result.run, result.bytes)?;
            }
            tx.commit()?;
            return Ok(received);
        }
        if commit.sequence != received + 1 {
            return Err(fault(HostOutboxFault::ReplayGap));
        }
        for run in commit.runs {
            if !host_owns_run(&tx, commit.host_id, run)? {
                return Err(fault(HostOutboxFault::Denied));
            }
        }
        let frame_len = commit.frame.len() as i64;
        if record_count >= quota.records || byte_count + frame_len > quota.bytes {
            return Err(fault(HostOutboxFault::Full));
        }
        if let Some(result) = commit.result {
            settle_host_command_in_tx(&tx, commit.host_id, result.command_id, result.run, result.bytes)?;
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
        tx.commit()?;
        Ok(commit.sequence)
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
              WHERE host_id = ?1 AND journal_id = ?2 AND sequence <= ?3",
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
    }

    fn spoke_db() -> WorkDb {
        let db = db();
        db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
        db
    }

    fn key(assignment: &SpokeAssignment) -> SpokeRunKey {
        SpokeRunKey {
            execution_id: assignment.execution_id.clone(),
            assignment_generation: assignment.assignment_generation,
            run_id: assignment.run_id.clone(),
            spawn_token: assignment.spawn_token.clone(),
        }
    }

    fn fault_of(error: &anyhow::Error) -> Option<HostOutboxFault> {
        error.downcast_ref::<HostOutboxFault>().copied()
    }

    fn commit<'a>(
        host_id: &'a str,
        admission: &'a SpokeJournalAdmission,
        sequence: u64,
        frame: &'a [u8],
        runs: &'a [SpokeRunKey],
        result: Option<SpokeIngressResult<'a>>,
    ) -> SpokeIngressCommit<'a> {
        SpokeIngressCommit::builder()
            .host_id(host_id)
            .generation(admission.generation)
            .journal_id(&admission.journal_id)
            .sequence(sequence)
            .frame(frame)
            .epoch(1)
            .features("execution=1")
            .observed_at_ms(1)
            .runs(runs)
            .maybe_result(result)
            .build()
    }

    #[test]
    fn first_contact_admits_an_empty_journal_and_increments_generation() {
        let db = db();
        let first = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        assert_eq!(
            first,
            SpokeJournalAdmission {
                journal_id: "journal".into(),
                generation: 1,
                received: 0,
            }
        );
        let second = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        assert_eq!(second.generation, 2);
        assert_eq!(second.received, 0);
        assert_eq!(
            fault_of(&db.open_spoke_journal("host-a", "other", 0, 1).unwrap_err()),
            Some(HostOutboxFault::ReplayGap)
        );
        assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().generation, 2);
    }

    #[test]
    fn a_spoke_that_forgot_records_or_compacted_past_receipt_is_a_replay_gap() {
        let db = db();
        let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        db.commit_spoke_record(commit("host-a", &admission, 1, b"one", &[], None))
            .unwrap();
        assert_eq!(
            fault_of(&db.open_spoke_journal("host-a", "journal", 0, 1).unwrap_err()),
            Some(HostOutboxFault::ReplayGap)
        );
        assert_eq!(
            fault_of(&db.open_spoke_journal("host-a", "journal", 1, 3).unwrap_err()),
            Some(HostOutboxFault::ReplayGap)
        );
        let again = db.open_spoke_journal("host-a", "journal", 1, 1).unwrap();
        assert_eq!(again.received, 1);
        assert_eq!(again.generation, 2);
    }

    #[test]
    fn a_new_journal_that_compacted_past_the_first_record_is_a_replay_gap() {
        let db = db();
        assert_eq!(
            fault_of(&db.open_spoke_journal("host-a", "journal", 4, 2).unwrap_err()),
            Some(HostOutboxFault::ReplayGap)
        );
        assert!(db.spoke_journal("host-a").unwrap().is_none());
    }

    #[test]
    fn a_new_journal_that_still_retains_its_first_record_replays_from_the_start() {
        let db = db();
        let admission = db.open_spoke_journal("host-a", "journal", 4, 1).unwrap();
        assert_eq!(admission.generation, 1);
        assert_eq!(admission.received, 0);
        for sequence in 1..=4 {
            assert_eq!(
                db.commit_spoke_record(commit("host-a", &admission, sequence, b"frame", &[], None))
                    .unwrap(),
                sequence
            );
        }
    }

    #[test]
    fn frames_are_contiguous_immutable_and_owned() {
        let db = spoke_db();
        let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        let run = key(&owned);
        let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        assert_eq!(
            db.commit_spoke_record(commit(
                "host-a",
                &admission,
                1,
                b"one",
                std::slice::from_ref(&run),
                None
            ))
            .unwrap(),
            1
        );
        assert_eq!(
            db.commit_spoke_record(commit(
                "host-a",
                &admission,
                1,
                b"one",
                std::slice::from_ref(&run),
                None
            ))
            .unwrap(),
            1
        );
        assert_eq!(
            fault_of(
                &db.commit_spoke_record(commit(
                    "host-a",
                    &admission,
                    1,
                    b"other",
                    std::slice::from_ref(&run),
                    None
                ))
                .unwrap_err()
            ),
            Some(HostOutboxFault::Denied)
        );
        assert_eq!(
            fault_of(
                &db.commit_spoke_record(commit(
                    "host-a",
                    &admission,
                    3,
                    b"three",
                    std::slice::from_ref(&run),
                    None
                ))
                .unwrap_err()
            ),
            Some(HostOutboxFault::ReplayGap)
        );
        let mut foreign = run.clone();
        foreign.spawn_token = "forged".into();
        assert_eq!(
            fault_of(
                &db.commit_spoke_record(commit(
                    "host-a",
                    &admission,
                    2,
                    b"two",
                    std::slice::from_ref(&foreign),
                    None
                ))
                .unwrap_err()
            ),
            Some(HostOutboxFault::Denied)
        );
        assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 1);
        assert_eq!(
            db.spoke_ingress_frame("host-a", 1).unwrap().as_deref(),
            Some(&b"one"[..])
        );
    }

    #[test]
    fn a_fenced_generation_cannot_commit() {
        let db = db();
        let first = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        let second = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        assert_eq!(
            fault_of(
                &db.commit_spoke_record(commit("host-a", &first, 1, b"one", &[], None))
                    .unwrap_err()
            ),
            Some(HostOutboxFault::Denied)
        );
        assert_eq!(
            db.commit_spoke_record(commit("host-a", &second, 1, b"one", &[], None))
                .unwrap(),
            1
        );
    }

    #[test]
    fn a_result_survives_reopen_after_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.db");
        let run;
        {
            let db = WorkDb::open(path.clone()).unwrap();
            db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
            let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
            run = key(&owned);
            db.append_host_command("host-a", "c1", Some(&run), RunAdmission::Start, b"req", &|seq| {
                seq.to_be_bytes().to_vec()
            })
            .unwrap();
            let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
            let result = SpokeIngressResult {
                command_id: "c1",
                run: Some(&run),
                bytes: b"ok",
            };
            db.commit_spoke_record(commit(
                "host-a",
                &admission,
                1,
                b"frame",
                std::slice::from_ref(&run),
                Some(result),
            ))
            .unwrap();
        }
        let db = WorkDb::open(path).unwrap();
        assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 1);
        assert_eq!(
            db.host_command_result("host-a", "c1").unwrap().as_deref(),
            Some(&b"ok"[..])
        );
        assert_eq!(
            db.spoke_ingress_frame("host-a", 1).unwrap().as_deref(),
            Some(&b"frame"[..])
        );
    }

    #[test]
    fn quota_counts_encoded_frame_bytes() {
        let db = db();
        let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        let huge = vec![0u8; MAX_HOST_INGRESS_BYTES as usize + 1];
        assert_eq!(
            fault_of(
                &db.commit_spoke_record(commit("host-a", &admission, 1, &huge, &[], None))
                    .unwrap_err()
            ),
            Some(HostOutboxFault::Full)
        );
        assert!(db.spoke_ingress_frame("host-a", 1).unwrap().is_none());
        assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 0);
    }

    #[test]
    fn quota_is_o1_exact_at_the_boundary_and_ignores_duplicates() {
        let db = db();
        let quota = IngressQuota { records: 2, bytes: 10 };
        let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        let put = |sequence: u64, frame: &[u8]| {
            db.commit_spoke_record_within(commit("host-a", &admission, sequence, frame, &[], None), quota)
        };
        assert_eq!(put(1, b"aaaa").unwrap(), 1);
        // An identical duplicate stores nothing and consumes no quota.
        assert_eq!(put(1, b"aaaa").unwrap(), 1);
        assert_eq!(put(1, b"aaaa").unwrap(), 1);
        assert_eq!(put(2, b"bbbbbb").unwrap(), 2); // exactly 10 bytes
        assert_eq!(fault_of(&put(3, b"c").unwrap_err()), Some(HostOutboxFault::Full));
        assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 2);
    }

    #[test]
    fn a_host_at_its_quota_recovers_after_pruning() {
        let db = db();
        let quota = IngressQuota { records: 2, bytes: 100 };
        let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        let put = |sequence: u64| {
            db.commit_spoke_record_within(commit("host-a", &admission, sequence, b"frame", &[], None), quota)
        };
        put(1).unwrap();
        put(2).unwrap();
        assert_eq!(fault_of(&put(3).unwrap_err()), Some(HostOutboxFault::Full));
        assert_eq!(db.prune_spoke_ingress("host-a", 1).unwrap(), 1);
        // Pruning past the receipt clamps to it; repeating is a no-op.
        assert_eq!(db.prune_spoke_ingress("host-a", 1).unwrap(), 0);
        assert_eq!(put(3).unwrap(), 3);
        assert_eq!(fault_of(&put(4).unwrap_err()), Some(HostOutboxFault::Full));
        assert_eq!(db.prune_spoke_ingress("host-a", 99).unwrap(), 2);
        assert_eq!(put(4).unwrap(), 4);
        // A retransmission of a pruned frame is acknowledged without storing it.
        assert_eq!(put(1).unwrap(), 4);
        assert!(db.spoke_ingress_frame("host-a", 1).unwrap().is_none());
        assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 4);
        // A reconnect after pruning still resumes at the receipt.
        let again = db.open_spoke_journal("host-a", "journal", 4, 1).unwrap();
        assert_eq!(again.received, 4);
    }

    #[test]
    fn a_result_at_a_pruned_sequence_is_not_accepted_unless_already_settled() {
        let db = db();
        db.set_host_transport("host-a", HostTransport::SpokeReady).unwrap();
        let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        let run = key(&owned);
        db.append_host_command("host-a", "c1", Some(&run), RunAdmission::Start, b"req", &|seq| {
            seq.to_be_bytes().to_vec()
        })
        .unwrap();
        let admission = db.open_spoke_journal("host-a", "journal", 0, 1).unwrap();
        db.commit_spoke_record(commit("host-a", &admission, 1, b"capacity", &[], None))
            .unwrap();
        db.prune_spoke_ingress("host-a", 1).unwrap();
        let result = || SpokeIngressResult {
            command_id: "c1",
            run: Some(&run),
            bytes: b"ok",
        };
        let replay = |result| {
            db.commit_spoke_record(commit(
                "host-a",
                &admission,
                1,
                b"fabricated",
                std::slice::from_ref(&run),
                Some(result),
            ))
        };
        assert_eq!(fault_of(&replay(result()).unwrap_err()), Some(HostOutboxFault::Denied));
        assert!(db.host_command_result("host-a", "c1").unwrap().is_none());
        assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 1);
        // Once the result is genuinely durable, a retransmission is acknowledged.
        db.commit_spoke_record(commit(
            "host-a",
            &admission,
            2,
            b"real",
            std::slice::from_ref(&run),
            Some(result()),
        ))
        .unwrap();
        db.prune_spoke_ingress("host-a", 2).unwrap();
        assert_eq!(replay(result()).unwrap(), 2);
        let other = SpokeIngressResult {
            command_id: "c1",
            run: Some(&run),
            bytes: b"different",
        };
        assert_eq!(fault_of(&replay(other).unwrap_err()), Some(HostOutboxFault::Denied));
        let mut forged = run.clone();
        forged.spawn_token = "forged".into();
        let wrong_run = SpokeIngressResult {
            command_id: "c1",
            run: Some(&forged),
            bytes: b"ok",
        };
        assert_eq!(fault_of(&replay(wrong_run).unwrap_err()), Some(HostOutboxFault::Denied));
        assert_eq!(
            db.host_command_result("host-a", "c1").unwrap().as_deref(),
            Some(&b"ok"[..])
        );
    }
}
