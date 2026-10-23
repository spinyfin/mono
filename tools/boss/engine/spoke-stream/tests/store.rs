use boss_protocol_spoke::{negotiate::Selection, v1::*};
use boss_spoke_stream::store::*;
use prost::Message;
use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
pub struct Memory {
    pub ingress: Mutex<Ingress>,
    pub outbox: Mutex<Outbox>,
    pub fail_commit: AtomicBool,
    pub pause_append: Mutex<Option<std::sync::Arc<Pause>>>,
}

#[derive(Default)]
pub struct Pause {
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}
#[derive(Default)]
pub struct Ingress {
    pub generation: u64,
    pub journal: Option<String>,
    pub records: BTreeMap<u64, Vec<u8>>,
}
#[derive(Default)]
pub struct Outbox {
    pub commands: Vec<Command>,
    pub encoded: BTreeMap<u64, Vec<u8>>,
    pub results: BTreeMap<String, CommandResult>,
    pub received: u64,
}

#[tonic::async_trait]
impl SpokeObservationSink for Memory {
    async fn open(&self, hello: &Hello, _: &Selection) -> Result<Admission, StoreError> {
        let mut state = self.ingress.lock().unwrap();
        let through = state.records.len() as u64;
        if state.journal.as_ref().is_some_and(|id| id != &hello.journal_id)
            || through > hello.last_record
            || hello.first_retained > through + 1
        {
            return Err(StoreError::ReplayGap);
        }
        state.journal = Some(hello.journal_id.clone());
        state.generation += 1;
        Ok(Admission {
            generation: state.generation,
            received: Cursor {
                journal_id: hello.journal_id.clone(),
                through,
            },
        })
    }
    async fn commit(&self, session: &Session, raw: &[u8], record: &Record) -> Result<Cursor, StoreError> {
        if self.fail_commit.load(Ordering::Acquire) {
            return Err(StoreError::Unavailable);
        }
        let mut state = self.ingress.lock().unwrap();
        if session.generation != state.generation || record.run.as_ref().is_some_and(|r| r != &super::run()) {
            return Err(StoreError::Denied);
        }
        let through = state.records.len() as u64;
        if let Some(previous) = state.records.get(&record.sequence) {
            if previous != raw {
                return Err(StoreError::Denied);
            }
        } else {
            if record.sequence != through + 1 {
                return Err(StoreError::ReplayGap);
            }
            // Model the one hub transaction shared by ingress and outbox.
            if let Some(record::Body::Result(result)) = &record.body {
                let mut outbox = self.outbox.lock().unwrap();
                if !outbox
                    .commands
                    .iter()
                    .any(|c| c.command_id == result.command_id && c.run == record.run)
                    || outbox.results.get(&result.command_id).is_some_and(|old| old != result)
                {
                    return Err(StoreError::Denied);
                }
                outbox.results.insert(result.command_id.clone(), result.clone());
            }
            state.records.insert(record.sequence, raw.to_vec());
        }
        Ok(Cursor {
            journal_id: session.journal_id.clone(),
            through: state.records.len() as u64,
        })
    }
}
#[tonic::async_trait]
impl CommandOutbox for Memory {
    async fn resume(&self, _: &str, receipt: Option<&Cursor>) -> Result<CommandJournal, StoreError> {
        let mut state = self.outbox.lock().unwrap();
        if let Some(receipt) = receipt {
            if receipt.journal_id != "commands"
                || receipt.through < state.received
                || receipt.through > state.commands.len() as u64
            {
                return Err(StoreError::ReplayGap);
            }
        } else if state.received != 0 {
            return Err(StoreError::ReplayGap);
        }
        if let Some(cursor) = receipt {
            state.received = cursor.through;
        }
        Ok(CommandJournal {
            journal_id: "commands".into(),
            received: state.received,
        })
    }
    async fn append(&self, _: &str, mut command: Command) -> Result<Command, StoreError> {
        let pause = { self.pause_append.lock().unwrap().clone() };
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
        let mut state = self.outbox.lock().unwrap();
        if let Some(existing) = state.commands.iter().find(|c| c.command_id == command.command_id) {
            command.sequence = existing.sequence;
            return if *existing == command {
                Ok(existing.clone())
            } else {
                Err(StoreError::Denied)
            };
        }
        command.sequence = state.commands.len() as u64 + 1;
        state.encoded.insert(command.sequence, command.encode_to_vec());
        state.commands.push(command.clone());
        Ok(command)
    }
    async fn next(&self, _: &str, after: u64) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .outbox
            .lock()
            .unwrap()
            .encoded
            .iter()
            .find(|(sequence, _)| **sequence > after)
            .map(|(_, bytes)| bytes.clone()))
    }
    async fn receipt(&self, _: &str, receipt: &Cursor) -> Result<(), StoreError> {
        let mut state = self.outbox.lock().unwrap();
        if receipt.journal_id != "commands"
            || receipt.through < state.received
            || receipt.through > state.commands.len() as u64
        {
            return Err(StoreError::ReplayGap);
        }
        state.received = receipt.through;
        Ok(())
    }
    async fn command(&self, _: &str, id: &str) -> Result<Option<Command>, StoreError> {
        Ok(self
            .outbox
            .lock()
            .unwrap()
            .commands
            .iter()
            .find(|c| c.command_id == id)
            .cloned())
    }
    async fn completed(&self, _: &str, id: &str) -> Result<Option<CommandResult>, StoreError> {
        Ok(self.outbox.lock().unwrap().results.get(id).cloned())
    }
}
