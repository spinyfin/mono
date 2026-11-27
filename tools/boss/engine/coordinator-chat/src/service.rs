//! The engine's chat service for one coordinator generation.
//!
//! The service attaches to the agent host's socket, verifies who is listening,
//! and runs the conversation reducer live over the host's journal records. It
//! drives the turn queue (one `session/prompt` at a time, engine prompts ahead
//! of the operator's), writes the intent log before it tells the app a message
//! was accepted, publishes completed items and offset-carrying streaming
//! deltas, and answers fetches from the reducer. After an engine restart it
//! recovers from the generation's files (snapshot, wire journal, intent log)
//! and reconnects, replaying only what it has not seen.
//!
//! What it does not do: start or restart the coordinator, set up the ACP
//! session (`initialize`, `session/new`, model and mode), or deliver the
//! handoff. A service over a host whose session is not yet established accepts
//! and queues messages and sends none until the reducer sees a session. Nothing
//! in the engine creates a service yet.
//!
//! # Wire contract with the app
//!
//! [`ChatService::subscribe`] yields what to publish on the
//! `coordinator.chat` topic ([`ChatPush`] becomes a
//! [`boss_protocol::TopicEventPayload::CoordinatorChat`]). Publishing is
//! lossy by design: the channel is bounded, and a subscriber that falls behind
//! is told how many it missed. The app recovers with [`ChatService::fetch`],
//! which is why deltas carry their offset. A push is never the only way to
//! learn something.

mod actor;
mod host_link;
mod lines;
mod push;

use std::collections::{HashMap, HashSet};
use std::io;
use std::time::Duration;

use async_trait::async_trait;
use boss_protocol::{CoordinatorChatFetchOutcome, CoordinatorChatOperation, CoordinatorChatOutcome};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use self::actor::{Actor, ActorInit, Command};
pub use self::host_link::HostEndpoint;
pub use self::push::{ChatPush, DELTA_PUBLISH_INTERVAL};
use crate::intent::{IntentRecordKind, read_intent_file_if_exists};
use crate::redact::CredentialRedactor;
use crate::reducer::DEFAULT_TOOL_TEXT_LIMIT;
use crate::retention::Generation;
use crate::snapshot::{
    DEFAULT_SNAPSHOT_INTERVAL_RECORDS, SnapshotLoadStatus, UnsentDispatchPolicy, recover_from_files,
};

/// How many pushes a subscriber may fall behind by before it is told it missed
/// some. At the publish interval and a busy stream this is minutes of slack;
/// a subscriber that is further behind fetches instead.
const PUSH_CHANNEL_CAPACITY: usize = 1024;

/// Commands waiting for the task that owns the generation.
const COMMAND_CHANNEL_CAPACITY: usize = 64;

/// Timings the service runs on. The defaults are what the engine uses; tests
/// shorten them.
#[derive(Debug, Clone)]
pub struct ServiceTuning {
    /// How long streamed text is held and merged before it is published.
    pub delta_publish_interval: Duration,
    /// Wire records between reducer snapshots.
    pub snapshot_interval_records: u64,
    /// First delay before reconnecting to a host that went away, doubling to
    /// [`Self::reconnect_max_delay`]. The engine's shared backoff; not a
    /// measured limit.
    pub reconnect_initial_delay: Duration,
    pub reconnect_max_delay: Duration,
}

impl Default for ServiceTuning {
    fn default() -> Self {
        Self {
            delta_publish_interval: DELTA_PUBLISH_INTERVAL,
            snapshot_interval_records: DEFAULT_SNAPSHOT_INTERVAL_RECORDS,
            reconnect_initial_delay: Duration::from_millis(100),
            reconnect_max_delay: Duration::from_secs(2),
        }
    }
}

/// What a service needs to start.
pub struct ServiceConfig {
    /// The generation's directory, created by
    /// [`crate::retention::GenerationStore::create`]. The agent host writes
    /// its journal at [`Generation::wire_path`].
    pub generation: Generation,
    /// Where the host listens and which process must be listening.
    pub endpoint: HostEndpoint,
    /// The generation's write credential. Removed from everything the service
    /// publishes, returns or says in an error.
    pub redactor: CredentialRedactor,
    pub tuning: ServiceTuning,
}

/// Where the service's connection to the agent host stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// Not yet attached to the host.
    Connecting,
    /// Attached and replaying the records it has not seen. Messages are
    /// accepted; none are sent until it has caught up.
    CatchingUp,
    /// Attached and current.
    Live,
    /// The connection to the host was lost; retrying. Messages are still
    /// accepted into the intent log.
    Reconnecting { reason: String },
    /// The service is finished and will not recover: it was shut down, the
    /// thing at the host's socket was refused, or the intent log could not be
    /// written. Every operation answers [`ChatError::NotRunning`].
    Stopped { reason: String },
}

/// Why an operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    /// The service has stopped. The app shows the reason and offers the
    /// terminal coordinator.
    #[error("the chat backend is not running: {0}")]
    NotRunning(String),
    /// The request cannot be honoured as it stands (an empty message, an
    /// attachment, a permission request that is not waiting).
    #[error("{0}")]
    Rejected(String),
    /// The agent host is not connected right now. Nothing was changed; the
    /// operation can be tried again.
    #[error("the agent host is not connected right now")]
    Disconnected,
    /// The intent log could not be written. The service has stopped.
    #[error("{0}")]
    Storage(String),
}

/// A queued operator message that Stop took back out of the queue so the app
/// can put it in the composer again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnedDraft {
    pub client_message_id: Option<String>,
    /// The message text, with the generation's credential removed.
    pub text: String,
}

/// What Stop did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelOutcome {
    /// A turn was running and `session/cancel` was sent for it.
    pub cancelled_turn: bool,
    /// Operator messages that were still queued and were not sent.
    pub returned_to_composer: Vec<ReturnedDraft>,
}

/// What the RPC handler talks to: the chat operations of the app protocol.
#[async_trait]
pub trait ChatBackend: Send + Sync {
    /// Runs one operation for the app. A [`ChatError::NotRunning`] is the
    /// "chat backend is not running" outcome; any other error is a request
    /// that failed.
    async fn handle(&self, operation: CoordinatorChatOperation) -> Result<CoordinatorChatOutcome, ChatError>;
}

/// A handle to the task that serves one generation. Cheap to clone; the task
/// stops when the last handle is dropped or [`ChatService::shutdown`] is called.
#[derive(Clone)]
pub struct ChatService {
    generation_id: std::sync::Arc<str>,
    commands: mpsc::Sender<Command>,
    pushes: broadcast::Sender<ChatPush>,
    status: watch::Receiver<ConnectionStatus>,
}

impl ChatService {
    /// Recovers the generation from its files and starts attaching to the host.
    ///
    /// Returns once the state on disk is loaded; attaching and catching up
    /// happen on the service's task (watch [`Self::status`]). The generation's
    /// intent log is opened for appending, so only one service may run per
    /// generation.
    pub async fn start(config: ServiceConfig) -> io::Result<Self> {
        let ServiceConfig {
            generation,
            endpoint,
            redactor,
            tuning,
        } = config;

        let load = {
            let generation = generation.clone();
            let redactor = redactor.clone();
            tokio::task::spawn_blocking(move || load_generation(&generation, redactor))
        };
        let Loaded {
            reducer,
            intent,
            client_message_ids,
        } = load.await.map_err(io::Error::other)??;

        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let (pushes, _) = broadcast::channel(PUSH_CHANNEL_CAPACITY);
        let (status_tx, status_rx) = watch::channel(ConnectionStatus::Connecting);
        let generation_id: std::sync::Arc<str> = generation.id().into();
        let actor = Actor::new(
            ActorInit::builder()
                .generation(generation)
                .endpoint(endpoint)
                .redactor(redactor)
                .tuning(tuning)
                .reducer(reducer)
                .intent(intent)
                .client_message_ids(client_message_ids)
                .commands(commands_rx)
                .pushes(pushes.clone())
                .status(status_tx)
                .build(),
        );
        tokio::spawn(actor.run());
        Ok(Self {
            generation_id,
            commands: commands_tx,
            pushes,
            status: status_rx,
        })
    }

    /// The generation this service serves.
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    /// Where the connection to the host stands.
    pub fn status(&self) -> watch::Receiver<ConnectionStatus> {
        self.status.clone()
    }

    /// What to publish on the `coordinator.chat` topic, from now on. A
    /// receiver that falls more than the channel's capacity behind gets
    /// [`broadcast::error::RecvError::Lagged`]; the app's recovery is a fetch.
    pub fn subscribe(&self) -> broadcast::Receiver<ChatPush> {
        self.pushes.subscribe()
    }

    /// Accepts an operator message. It is in the intent log, and so survives a
    /// restart, before this returns. Sending the same `client_message_id`
    /// again is answered without queueing a second copy.
    pub async fn send_operator_message(&self, text: String, client_message_id: String) -> Result<(), ChatError> {
        self.ask(|reply| Command::SendOperator {
            text,
            client_message_id,
            reply,
        })
        .await?
    }

    /// Queues a prompt from the engine (the brief, a reset request, an
    /// authorization). Engine prompts go ahead of queued operator messages and
    /// never interrupt a turn. Returns the prompt's intent id.
    pub async fn queue_engine_prompt(&self, text: String) -> Result<u64, ChatError> {
        self.ask(|reply| Command::QueueEngine { text, reply }).await?
    }

    /// Stop: see [`CancelOutcome`].
    pub async fn cancel(&self) -> Result<CancelOutcome, ChatError> {
        self.ask(|reply| Command::Cancel { reply }).await?
    }

    /// Answers one of the agent's permission requests by the id the request
    /// item carries and one of the options it offered.
    pub async fn answer_permission(&self, request_id: &str, option_id: &str) -> Result<(), ChatError> {
        self.ask(|reply| Command::AnswerPermission {
            request_id: request_id.to_owned(),
            option_id: option_id.to_owned(),
            reply,
        })
        .await?
    }

    /// The conversation after journal record `after_seq`: every item created
    /// or changed since, the connection state, and the message being
    /// streamed. Always answers, even while the host is away.
    pub async fn fetch(&self, after_seq: u64) -> Result<Box<CoordinatorChatFetchOutcome>, ChatError> {
        self.ask(|reply| Command::Fetch { after_seq, reply }).await
    }

    /// Publishes what is waiting, writes a last snapshot and stops. Returns
    /// once that is done.
    pub async fn shutdown(&self) {
        let (reply, done) = oneshot::channel();
        if self.commands.send(Command::Shutdown { reply }).await.is_ok() {
            let _ = done.await;
        }
    }

    async fn ask<T>(&self, command: impl FnOnce(oneshot::Sender<T>) -> Command) -> Result<T, ChatError> {
        let (reply, answer) = oneshot::channel();
        if self.commands.send(command(reply)).await.is_err() {
            return Err(self.not_running());
        }
        answer.await.map_err(|_| self.not_running())
    }

    fn not_running(&self) -> ChatError {
        let reason = match &*self.status.borrow() {
            ConnectionStatus::Stopped { reason } => reason.clone(),
            _ => "the chat service is no longer running".to_owned(),
        };
        ChatError::NotRunning(reason)
    }
}

#[async_trait]
impl ChatBackend for ChatService {
    async fn handle(&self, operation: CoordinatorChatOperation) -> Result<CoordinatorChatOutcome, ChatError> {
        match operation {
            CoordinatorChatOperation::Send {
                text,
                client_message_id,
                attachments,
            } => {
                if !attachments.is_empty() {
                    return Err(ChatError::Rejected(
                        "attachments are not supported by the chat backend yet".to_owned(),
                    ));
                }
                self.send_operator_message(text, client_message_id.clone()).await?;
                Ok(CoordinatorChatOutcome::Sent { client_message_id })
            }
            CoordinatorChatOperation::Cancel => {
                self.cancel().await?;
                Ok(CoordinatorChatOutcome::Cancelled)
            }
            CoordinatorChatOperation::AnswerPermission { request_id, option_id } => {
                self.answer_permission(&request_id, &option_id).await?;
                Ok(CoordinatorChatOutcome::PermissionAnswered { request_id })
            }
            CoordinatorChatOperation::Fetch { after_seq } => {
                Ok(CoordinatorChatOutcome::Fetch(self.fetch(after_seq).await?))
            }
        }
    }
}

/// A generation's state as it is on disk.
struct Loaded {
    reducer: crate::reducer::ConversationReducer,
    intent: crate::intent::IntentLog,
    client_message_ids: HashSet<String>,
}

fn load_generation(generation: &Generation, redactor: CredentialRedactor) -> io::Result<Loaded> {
    // Opening the intent log first truncates a record cut off by a crash, so
    // what recovery reads is what appends continue from.
    let intent = crate::intent::IntentLog::open(&generation.intent_path())?;
    let recovery = recover_from_files(
        &generation.wire_path(),
        Some(&generation.intent_path()),
        Some(&generation.snapshot_path()),
        redactor,
        DEFAULT_TOOL_TEXT_LIMIT,
        // The host may still be appending: a prompt the log calls sent and the
        // journal has not shown yet may only be in flight.
        UnsentDispatchPolicy::DeferUntilLiveBoundary,
    )?;
    match &recovery.snapshot_status {
        SnapshotLoadStatus::Hit { .. } | SnapshotLoadStatus::Missing | SnapshotLoadStatus::NotConfigured => {}
        problem => tracing::warn!(
            generation = generation.id(),
            ?problem,
            "the reducer snapshot was not used; replayed the generation from its start"
        ),
    }
    tracing::info!(
        generation = generation.id(),
        wire_records = recovery.wire_records_replayed,
        intent_records = recovery.intent_records_replayed,
        "recovered the coordinator chat generation"
    );
    Ok(Loaded {
        reducer: recovery.reducer,
        intent,
        client_message_ids: accepted_client_message_ids(&generation.intent_path())?,
    })
}

/// The client message ids of the operator messages the intent log accepted and
/// has not withdrawn.
fn accepted_client_message_ids(intent_path: &std::path::Path) -> io::Result<HashSet<String>> {
    let mut by_intent: HashMap<u64, String> = HashMap::new();
    for record in read_intent_file_if_exists(intent_path)? {
        match record.kind {
            IntentRecordKind::Queued {
                intent_id,
                client_message_id: Some(client_message_id),
                ..
            } => {
                by_intent.insert(intent_id, client_message_id);
            }
            IntentRecordKind::Withdrawn { intent_id, .. } => {
                by_intent.remove(&intent_id);
            }
            _ => {}
        }
    }
    Ok(by_intent.into_values().collect())
}
