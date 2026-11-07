//! The hub's durable command outbox, backed by the work database.
//!
//! The stream manager owns no database: it persists every command through this
//! trait before writing it to a stream, and answers a retry of a command id from
//! the stored result. Backing the trait with [`WorkDb`] puts the outbox in the
//! same transaction boundary as assignment ownership, which is what lets a
//! command be refused unless the hub already recorded who owns its run.
//!
//! Every method offloads the blocking SQLite work, and none holds a transaction
//! across an `.await`.
use std::sync::Arc;

use boss_protocol_spoke::v1::{Command, CommandResult, Cursor, ResultCode};
use boss_spoke_stream::store::{CommandJournal, CommandOutbox, PendingCommand, StoreError};
use boss_work_db::{HostOutboxFault, SpokeRunKey, WorkDb};
use prost::Message;

#[derive(Clone)]
pub struct WorkDbOutbox {
    db: Arc<WorkDb>,
}

impl WorkDbOutbox {
    pub fn new(db: Arc<WorkDb>) -> Self {
        Self { db }
    }

    async fn blocking<T, F>(&self, work: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&WorkDb) -> anyhow::Result<T> + Send + 'static,
    {
        let db = Arc::clone(&self.db);
        match tokio::task::spawn_blocking(move || work(&db)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(store_error(&error)),
            Err(_) => Err(StoreError::Unavailable),
        }
    }
}

fn store_error(error: &anyhow::Error) -> StoreError {
    match error.downcast_ref::<HostOutboxFault>() {
        Some(HostOutboxFault::ReplayGap) => StoreError::ReplayGap,
        Some(HostOutboxFault::Denied) => StoreError::Denied,
        Some(HostOutboxFault::Full) => StoreError::Full,
        None => {
            tracing::warn!(error = %format!("{error:#}"), "spoke command outbox storage failed");
            StoreError::Unavailable
        }
    }
}

/// The run a command or result is bound to, in the form the database checks.
pub(crate) fn run_key(run: Option<&boss_protocol_spoke::v1::Run>) -> Option<SpokeRunKey> {
    run.map(|run| SpokeRunKey {
        execution_id: run.execution_id.clone(),
        assignment_generation: run.assignment_generation,
        run_id: run.run_id.clone(),
        spawn_token: run.spawn_token.clone(),
    })
}

/// A peer that never bound the journal sends no cursor, or an empty one.
fn receipt(cursor: Option<&Cursor>) -> Option<(&str, u64)> {
    cursor
        .filter(|cursor| !cursor.journal_id.is_empty())
        .map(|cursor| (cursor.journal_id.as_str(), cursor.through))
}

#[async_trait::async_trait]
impl CommandOutbox for WorkDbOutbox {
    async fn resume(&self, host: &str, cursor: Option<&Cursor>) -> Result<CommandJournal, StoreError> {
        let host = host.to_owned();
        let cursor = cursor.cloned();
        let (journal_id, received) = self
            .blocking(move |db| db.resume_host_command_journal(&host, receipt(cursor.as_ref())))
            .await?;
        Ok(CommandJournal { journal_id, received })
    }

    async fn append(&self, host: &str, command: Command) -> Result<Command, StoreError> {
        let host = host.to_owned();
        self.blocking(move |db| {
            let request = command.encode_to_vec();
            let encode = |sequence: u64| {
                let mut stamped = command.clone();
                stamped.sequence = sequence;
                stamped.encode_to_vec()
            };
            let stored = db.append_host_command(
                &host,
                &command.command_id,
                run_key(command.run.as_ref()).as_ref(),
                &request,
                &encode,
            )?;
            let mut stamped = command.clone();
            stamped.sequence = stored.sequence;
            Ok(stamped)
        })
        .await
    }

    async fn next(&self, host: &str, after: u64) -> Result<Option<PendingCommand>, StoreError> {
        let host = host.to_owned();
        self.blocking(move |db| {
            Ok(db.host_command_after(&host, after)?.map(|stored| PendingCommand {
                sequence: stored.sequence,
                bytes: stored.wire,
            }))
        })
        .await
    }

    async fn reject(&self, host: &str, sequence: u64, reason: &str) -> Result<CommandResult, StoreError> {
        let host = host.to_owned();
        let reason = reason.to_owned();
        self.blocking(move |db| {
            let result_for = |command_id: &str| {
                CommandResult {
                    command_id: command_id.to_owned(),
                    code: ResultCode::Rejected.into(),
                    reason: reason.clone(),
                    ..Default::default()
                }
                .encode_to_vec()
            };
            let reseq = |wire: &[u8], sequence: u64| {
                let mut command = Command::decode(wire)?;
                command.sequence = sequence;
                Ok(command.encode_to_vec())
            };
            let (_, stored) = db.reject_host_command(&host, sequence, &result_for, &reseq)?;
            Ok(CommandResult::decode(stored.as_slice())?)
        })
        .await
    }

    async fn receipt(&self, host: &str, cursor: &Cursor) -> Result<(), StoreError> {
        let host = host.to_owned();
        let cursor = cursor.clone();
        self.blocking(move |db| db.record_host_command_receipt(&host, &cursor.journal_id, cursor.through))
            .await
    }

    async fn command(&self, host: &str, command_id: &str) -> Result<Option<Command>, StoreError> {
        let (host, command_id) = (host.to_owned(), command_id.to_owned());
        self.blocking(move |db| {
            db.host_command_wire(&host, &command_id)?
                .map(|wire| Ok(Command::decode(wire.as_slice())?))
                .transpose()
        })
        .await
    }

    async fn completed(&self, host: &str, command_id: &str) -> Result<Option<CommandResult>, StoreError> {
        let (host, command_id) = (host.to_owned(), command_id.to_owned());
        self.blocking(move |db| {
            db.host_command_result(&host, &command_id)?
                .map(|bytes| Ok(CommandResult::decode(bytes.as_slice())?))
                .transpose()
        })
        .await
    }
}
