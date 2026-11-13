//! WorkDb implementations of the stream manager's durable contracts.

use boss_protocol_spoke::{
    negotiate::Selection,
    v1::{Command, CommandResult, Cursor, Hello, Record, ResultCode, Run, record},
};
use boss_spoke_stream::store::{
    Admission, CommandAdmission, CommandJournal, CommandOutbox, PendingCommand, Session, SpokeObservationSink,
    StoreError,
};
use boss_work_db::{HostOutboxFault, RunAdmission, SpokeIngressCommit, SpokeIngressResult, SpokeRunKey, WorkDb};
use prost::Message;
use std::sync::Arc;
use tokio::sync::Notify;

/// Observation journal backed by `spoke_journals` / `spoke_ingress`.
pub struct WorkDbSink {
    db: WorkDb,
    /// Told after each durable commit, so the projector applies a record
    /// promptly. Receipt never waits on it, and a missed wake costs one sweep.
    wake: Option<Arc<Notify>>,
}

impl WorkDbSink {
    pub fn new(db: WorkDb) -> Self {
        Self { db, wake: None }
    }

    /// A sink that wakes `wake` after every record it commits.
    pub fn with_wake(db: WorkDb, wake: Arc<Notify>) -> Self {
        Self { db, wake: Some(wake) }
    }
}

/// Command outbox backed by `host_commands` / `host_command_journals`.
pub struct WorkDbOutbox {
    db: WorkDb,
}

impl WorkDbOutbox {
    pub fn new(db: WorkDb) -> Self {
        Self { db }
    }
}

async fn blocking<T, F>(db: &WorkDb, f: F) -> Result<T, StoreError>
where
    T: Send + 'static,
    F: FnOnce(&WorkDb) -> Result<T, StoreError> + Send + 'static,
{
    let db = db.clone();
    tokio::task::spawn_blocking(move || f(&db))
        .await
        .unwrap_or(Err(StoreError::Unavailable))
}

fn store_error(error: anyhow::Error) -> StoreError {
    match error.downcast_ref::<HostOutboxFault>().copied() {
        Some(HostOutboxFault::ReplayGap) => StoreError::ReplayGap,
        Some(HostOutboxFault::Denied) => StoreError::Denied,
        Some(HostOutboxFault::Full) => StoreError::Full,
        None => StoreError::Unavailable,
    }
}

fn run_key(run: &Run) -> SpokeRunKey {
    SpokeRunKey {
        execution_id: run.execution_id.clone(),
        assignment_generation: run.assignment_generation,
        run_id: run.run_id.clone(),
        spawn_token: run.spawn_token.clone(),
    }
}

fn claimed_runs(record: &Record) -> Vec<SpokeRunKey> {
    let mut runs = Vec::new();
    if let Some(run) = &record.run {
        runs.push(run_key(run));
    }
    if let Some(record::Body::Inventory(snapshot)) = &record.body {
        for observation in &snapshot.runs {
            if let Some(run) = &observation.run {
                runs.push(run_key(run));
            }
        }
    }
    runs
}

fn encode_features(selection: &Selection) -> String {
    selection
        .to_features()
        .into_iter()
        .map(|feature| format!("{}={}", feature.name, feature.revision))
        .collect::<Vec<_>>()
        .join(",")
}

fn observed_at_ms(record: &Record) -> i64 {
    record.observed.as_ref().map(|stamp| stamp.wall_unix_ms).unwrap_or(0)
}

fn run_admission(admission: CommandAdmission) -> RunAdmission {
    match admission {
        CommandAdmission::Start => RunAdmission::Start,
        CommandAdmission::Recovery => RunAdmission::Recovery,
    }
}

fn decode_command(bytes: &[u8]) -> Result<Command, StoreError> {
    Command::decode(bytes).map_err(|_| StoreError::Unavailable)
}

fn decode_result(bytes: &[u8]) -> Result<CommandResult, StoreError> {
    CommandResult::decode(bytes).map_err(|_| StoreError::Unavailable)
}

#[tonic::async_trait]
impl SpokeObservationSink for WorkDbSink {
    async fn open(&self, hello: &Hello, _: &Selection) -> Result<Admission, StoreError> {
        let host_id = hello.host_id.clone();
        let journal_id = hello.journal_id.clone();
        let last_record = hello.last_record;
        let first_retained = hello.first_retained;
        blocking(&self.db, move |db| {
            let admission = db
                .open_spoke_journal(&host_id, &journal_id, last_record, first_retained)
                .map_err(store_error)?;
            Ok(Admission {
                generation: admission.generation,
                received: Cursor {
                    journal_id: admission.journal_id,
                    through: admission.received,
                },
            })
        })
        .await
    }

    async fn commit(&self, session: &Session, raw_frame: &[u8], record: &Record) -> Result<Cursor, StoreError> {
        let host_id = session.host_id.clone();
        let journal_id = session.journal_id.clone();
        let generation = session.generation;
        let epoch = session.selection.epoch;
        let features = encode_features(&session.selection);
        let frame = raw_frame.to_vec();
        let sequence = record.sequence;
        let observed_at_ms = observed_at_ms(record);
        let runs = claimed_runs(record);
        let wake = self.wake.clone();
        let result = match &record.body {
            Some(record::Body::Result(result)) => Some((
                result.command_id.clone(),
                record.run.as_ref().map(run_key),
                result.encode_to_vec(),
            )),
            _ => None,
        };
        // Snapshots are live display data: receipt them so the spoke can
        // advance, but do not keep the bytes against the host's quota.
        let retain = !matches!(record.body, Some(record::Body::Capture(_)));
        let urgent = matches!(record.body, Some(record::Body::Proposal(_)));
        let cursor = blocking(&self.db, move |db| {
            let result_run = result.as_ref().and_then(|(_, run, _)| run.as_ref());
            let result_ref = result.as_ref().map(|(command_id, _, bytes)| SpokeIngressResult {
                command_id,
                run: result_run,
                bytes,
            });
            let through = db
                .commit_spoke_record(
                    SpokeIngressCommit::builder()
                        .host_id(host_id.as_str())
                        .generation(generation)
                        .journal_id(journal_id.as_str())
                        .sequence(sequence)
                        .frame(frame.as_slice())
                        .epoch(epoch)
                        .features(features.as_str())
                        .observed_at_ms(observed_at_ms)
                        .runs(runs.as_slice())
                        .maybe_result(result_ref)
                        .urgent(urgent)
                        .retain(retain)
                        .build(),
                )
                .map_err(store_error)?;
            Ok(Cursor { journal_id, through })
        })
        .await?;
        if let Some(wake) = wake {
            wake.notify_one();
        }
        Ok(cursor)
    }
}

#[tonic::async_trait]
impl CommandOutbox for WorkDbOutbox {
    async fn resume(&self, host: &str, receipt: Option<&Cursor>) -> Result<CommandJournal, StoreError> {
        let host = host.to_owned();
        let receipt = receipt.map(|cursor| (cursor.journal_id.clone(), cursor.through));
        blocking(&self.db, move |db| {
            let (journal_id, received) = db
                .resume_host_command_journal(&host, receipt.as_ref().map(|(id, through)| (id.as_str(), *through)))
                .map_err(store_error)?;
            Ok(CommandJournal { journal_id, received })
        })
        .await
    }

    async fn append(&self, host: &str, admission: CommandAdmission, command: Command) -> Result<Command, StoreError> {
        let host = host.to_owned();
        blocking(&self.db, move |db| {
            if host.is_empty() || command.command_id.is_empty() {
                return Err(StoreError::Denied);
            }
            let mut request = command.clone();
            request.sequence = 0;
            let request = request.encode_to_vec();
            let run = command.run.as_ref().map(run_key);
            let admission = run_admission(admission);
            let stored = db
                .append_host_command(
                    &host,
                    &command.command_id,
                    run.as_ref(),
                    admission,
                    &request,
                    &|sequence| {
                        let mut stored = command.clone();
                        stored.sequence = sequence;
                        stored.encode_to_vec()
                    },
                )
                .map_err(store_error)?;
            decode_command(&stored.wire)
        })
        .await
    }

    async fn next(&self, host: &str, after: u64) -> Result<Option<PendingCommand>, StoreError> {
        let host = host.to_owned();
        blocking(&self.db, move |db| {
            let stored = db.host_command_after(&host, after).map_err(store_error)?;
            Ok(stored.map(|command| PendingCommand {
                sequence: command.sequence,
                bytes: command.wire,
            }))
        })
        .await
    }

    async fn reject(&self, host: &str, sequence: u64, reason: &str) -> Result<CommandResult, StoreError> {
        let host = host.to_owned();
        let reason = reason.to_owned();
        blocking(&self.db, move |db| {
            let (_id, bytes) = db
                .reject_host_command(
                    &host,
                    sequence,
                    &|command_id| {
                        CommandResult {
                            command_id: command_id.to_owned(),
                            code: ResultCode::Rejected.into(),
                            reason: reason.clone(),
                            ..Default::default()
                        }
                        .encode_to_vec()
                    },
                    &|wire, sequence| {
                        let mut command = Command::decode(wire)
                            .map_err(|error| anyhow::anyhow!("unusable command bytes: {error}"))?;
                        command.sequence = sequence;
                        Ok(command.encode_to_vec())
                    },
                )
                .map_err(store_error)?;
            decode_result(&bytes)
        })
        .await
    }

    async fn receipt(&self, host: &str, receipt: &Cursor) -> Result<(), StoreError> {
        let host = host.to_owned();
        let journal_id = receipt.journal_id.clone();
        let through = receipt.through;
        blocking(&self.db, move |db| {
            db.record_host_command_receipt(&host, &journal_id, through)
                .map_err(store_error)
        })
        .await
    }

    async fn command(&self, host: &str, command_id: &str) -> Result<Option<Command>, StoreError> {
        let host = host.to_owned();
        let command_id = command_id.to_owned();
        blocking(&self.db, move |db| {
            match db.host_command_wire(&host, &command_id).map_err(store_error)? {
                Some(wire) => decode_command(&wire).map(Some),
                None => Ok(None),
            }
        })
        .await
    }

    async fn completed(&self, host: &str, command_id: &str) -> Result<Option<CommandResult>, StoreError> {
        let host = host.to_owned();
        let command_id = command_id.to_owned();
        blocking(&self.db, move |db| {
            match db.host_command_result(&host, &command_id).map_err(store_error)? {
                Some(bytes) => decode_result(&bytes).map(Some),
                None => Ok(None),
            }
        })
        .await
    }
}
