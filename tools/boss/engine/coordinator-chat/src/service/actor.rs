//! The task that owns one generation's chat state.
//!
//! One task owns the reducer, the intent log and the connection to the agent
//! host, and everything else talks to it through messages. That gives the
//! orderings the design needs without a lock: a send is written to the intent
//! log before the caller is told it was accepted; a prompt is logged as sent
//! before its line is written; and the reducer sees the host's records, the
//! intent log's records and the service's own writes in the one order they
//! were applied.
//!
//! The reducer is fed only by the host's records. The service writes a line
//! for the agent, the host journals it and echoes it back, and the echo is
//! what moves the reducer. That is the same path replay takes, so a conversation
//! reduced live and the same conversation reduced after a restart are the same
//! conversation.

use std::collections::HashSet;
use std::future::pending;
use std::time::Instant;

use boss_acp_client::schema::RequestId;
use boss_agent_host::journal::WireRecord;
use boss_protocol::{
    CoordinatorChatConnectionPhase, CoordinatorChatConnectionState, CoordinatorChatFetchOutcome,
    CoordinatorChatItemKind, CoordinatorChatPermissionResolution,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant as TokioInstant, sleep_until};

use super::host_link::{ConnectError, HostConnection, HostEndpoint, LinkError, last_journal_seq};
use super::lines::{self, PermissionAnswer};
use super::push::{ChatPush, PushQueue};
use super::{CancelOutcome, ChatError, ConnectionStatus, ReturnedDraft, ServiceTuning};
use crate::intent::{IntentLog, WithdrawReason};
use crate::queue::PromptOrigin;
use crate::redact::CredentialRedactor;
use crate::reducer::ConversationReducer;
use crate::retention::Generation;
use crate::snapshot::save_snapshot;

/// What a handle asks the task to do. Each carries the channel the answer
/// goes back on.
pub(super) enum Command {
    SendOperator {
        text: String,
        client_message_id: String,
        reply: oneshot::Sender<Result<(), ChatError>>,
    },
    QueueEngine {
        text: String,
        reply: oneshot::Sender<Result<u64, ChatError>>,
    },
    Cancel {
        reply: oneshot::Sender<Result<CancelOutcome, ChatError>>,
    },
    AnswerPermission {
        request_id: String,
        option_id: String,
        reply: oneshot::Sender<Result<(), ChatError>>,
    },
    Fetch {
        after_seq: u64,
        reply: oneshot::Sender<Box<CoordinatorChatFetchOutcome>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

enum Link {
    /// Not connected; try again at `retry_at`.
    Down {
        retry_at: Instant,
    },
    Up(HostConnection),
}

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

pub(super) struct Actor {
    wiring: Wiring,
    convo: Conversation,
    publish: Publishing,
    link_state: LinkState,
    upkeep: Upkeep,
}

/// What the task was started with and listens on.
struct Wiring {
    generation: Generation,
    endpoint: HostEndpoint,
    redactor: CredentialRedactor,
    tuning: ServiceTuning,
    commands: mpsc::Receiver<Command>,
}

/// The conversation and what is needed to extend it.
struct Conversation {
    reducer: ConversationReducer,
    intent: IntentLog,
    /// Every client message id the intent log holds for this generation, so a
    /// retried send is answered, not queued twice.
    client_message_ids: HashSet<String>,
    /// Permission requests this service has already written an answer for
    /// whose echo has not yet cleared them from the reducer.
    answered_permissions: HashSet<String>,
}

/// How the conversation reaches the app.
struct Publishing {
    pushes: broadcast::Sender<ChatPush>,
    status: watch::Sender<ConnectionStatus>,
    queue: PushQueue,
    /// The state last seen, to publish only changes to it.
    published_state: CoordinatorChatConnectionState,
}

/// Bookkeeping for snapshots and shutdown.
struct Upkeep {
    records_since_snapshot: u64,
    /// The periodic snapshot being written, if one is. Logs its own failure.
    snapshot_task: Option<JoinHandle<()>>,
    /// Told once the task has published what it held and written its last
    /// snapshot.
    shutdown_reply: Option<oneshot::Sender<()>>,
}

/// Where the connection stands.
struct LinkState {
    link: Link,
    /// Whether every record the host held when this connection was made has
    /// been applied. Nothing is sent before that: the reducer does not yet
    /// know what the agent is doing.
    caught_up: bool,
    /// The last record the host's journal held when this connection was made.
    boundary_seq: u64,
    failures: u32,
}

/// Everything the task starts from.
#[derive(bon::Builder)]
pub(super) struct ActorInit {
    pub(super) generation: Generation,
    pub(super) endpoint: HostEndpoint,
    pub(super) redactor: CredentialRedactor,
    pub(super) tuning: ServiceTuning,
    pub(super) reducer: ConversationReducer,
    pub(super) intent: IntentLog,
    pub(super) client_message_ids: HashSet<String>,
    pub(super) commands: mpsc::Receiver<Command>,
    pub(super) pushes: broadcast::Sender<ChatPush>,
    pub(super) status: watch::Sender<ConnectionStatus>,
}

impl Actor {
    pub(super) fn new(init: ActorInit) -> Self {
        let published_state = init.reducer.state().clone();
        let queue = PushQueue::new(init.tuning.delta_publish_interval);
        Self {
            wiring: Wiring {
                generation: init.generation,
                endpoint: init.endpoint,
                redactor: init.redactor,
                tuning: init.tuning,
                commands: init.commands,
            },
            convo: Conversation {
                reducer: init.reducer,
                intent: init.intent,
                client_message_ids: init.client_message_ids,
                answered_permissions: HashSet::new(),
            },
            publish: Publishing {
                pushes: init.pushes,
                status: init.status,
                queue,
                published_state,
            },
            link_state: LinkState {
                link: Link::Down {
                    retry_at: Instant::now(),
                },
                caught_up: false,
                boundary_seq: 0,
                failures: 0,
            },
            upkeep: Upkeep {
                records_since_snapshot: 0,
                snapshot_task: None,
                shutdown_reply: None,
            },
        }
    }

    pub(super) async fn run(mut self) {
        self.set_status(ConnectionStatus::Connecting);
        loop {
            let flush_due = self.publish.queue.due();
            let retry_at = match &self.link_state.link {
                Link::Down { retry_at } => Some(*retry_at),
                Link::Up(_) => None,
            };
            tokio::select! {
                command = self.wiring.commands.recv() => {
                    // Every handle was dropped.
                    let Some(command) = command else { break };
                    if self.handle(command).await == Flow::Stop {
                        break;
                    }
                }
                records = read_link(&mut self.link_state.link) => match records {
                    Ok(records) => self.apply_records(records).await,
                    Err(error) => self.link_lost(&error),
                },
                () = until(retry_at) => self.connect().await,
                () = until(flush_due) => {}
            }
            if self.stopped().is_some() {
                break;
            }
            self.flush_if_due();
        }
        // Reached with no reason on record only when every handle was dropped.
        self.set_status(ConnectionStatus::Stopped {
            reason: "the chat service was dropped".to_owned(),
        });
        self.finish().await;
        if let Some(reply) = self.upkeep.shutdown_reply.take() {
            let _ = reply.send(());
        }
    }

    fn stopped(&self) -> Option<String> {
        match &*self.publish.status.borrow() {
            ConnectionStatus::Stopped { reason } => Some(reason.clone()),
            _ => None,
        }
    }

    // ---- commands ---------------------------------------------------------

    async fn handle(&mut self, command: Command) -> Flow {
        match command {
            Command::SendOperator {
                text,
                client_message_id,
                reply,
            } => {
                let _ = reply.send(self.send_operator(text, client_message_id).await);
            }
            Command::QueueEngine { text, reply } => {
                let _ = reply.send(self.queue_engine(text).await);
            }
            Command::Cancel { reply } => {
                let _ = reply.send(self.cancel().await);
            }
            Command::AnswerPermission {
                request_id,
                option_id,
                reply,
            } => {
                let _ = reply.send(self.answer_permission(&request_id, &option_id).await);
            }
            Command::Fetch { after_seq, reply } => {
                let _ = reply.send(Box::new(self.fetch(after_seq)));
            }
            Command::Shutdown { reply } => {
                self.set_status(ConnectionStatus::Stopped {
                    reason: "the chat service was shut down".to_owned(),
                });
                self.upkeep.shutdown_reply = Some(reply);
                return Flow::Stop;
            }
        }
        Flow::Continue
    }

    /// Accepts an operator message. The message is in the intent log, which
    /// is what survives a restart, before the caller hears it was accepted.
    async fn send_operator(&mut self, text: String, client_message_id: String) -> Result<(), ChatError> {
        if client_message_id.trim().is_empty() {
            return Err(ChatError::Rejected("a message needs a client message id".to_owned()));
        }
        if text.trim().is_empty() {
            return Err(ChatError::Rejected("the message is empty".to_owned()));
        }
        // A retry of a send that was already accepted.
        if self.convo.client_message_ids.contains(&client_message_id) {
            return Ok(());
        }
        let record = self
            .convo
            .intent
            .append_queued(
                PromptOrigin::Operator,
                text,
                Some(client_message_id.clone()),
                Vec::new(),
            )
            .map_err(|error| self.storage_failed(&error))?;
        self.convo.client_message_ids.insert(client_message_id);
        self.convo.reducer.apply_intent_record(&record);
        self.dispatch().await;
        Ok(())
    }

    async fn queue_engine(&mut self, text: String) -> Result<u64, ChatError> {
        let record = self
            .convo
            .intent
            .append_queued(PromptOrigin::Engine, text, None, Vec::new())
            .map_err(|error| self.storage_failed(&error))?;
        let intent_id = record.intent_id();
        self.convo.reducer.apply_intent_record(&record);
        self.dispatch().await;
        Ok(intent_id)
    }

    /// Stop: returns queued operator messages to the composer, answers every
    /// pending permission request as cancelled (the protocol requires it
    /// before the cancel itself), then cancels the turn. Queued engine
    /// prompts stay queued and run when the turn ends.
    async fn cancel(&mut self) -> Result<CancelOutcome, ChatError> {
        let plan = self.convo.reducer.queue().plan_stop();
        if plan.send_cancel && !matches!(self.link_state.link, Link::Up(_)) {
            // Nothing has been changed yet, so the caller can simply try again.
            return Err(ChatError::Disconnected);
        }
        let mut returned = Vec::with_capacity(plan.returned_operator_prompts.len());
        for prompt in &plan.returned_operator_prompts {
            let record = self
                .convo
                .intent
                .append_withdrawn(prompt.intent_id, WithdrawReason::StopReturnedToComposer)
                .map_err(|error| self.storage_failed(&error))?;
            self.convo.reducer.apply_intent_record(&record);
            if let Some(id) = &prompt.client_message_id {
                // The composer may send the same draft again under its id.
                self.convo.client_message_ids.remove(id);
            }
            returned.push(ReturnedDraft {
                client_message_id: prompt.client_message_id.clone(),
                text: self.wiring.redactor.redact_str(prompt.text()),
            });
        }
        if plan.send_cancel {
            for id in self.convo.reducer.pending_permission_request_ids() {
                self.write(&lines::permission_response_line(&id, PermissionAnswer::Cancelled))
                    .await;
            }
            if let Some(session_id) = self.convo.reducer.state().session_id.clone() {
                self.write(&lines::cancel_line(&session_id)).await;
            }
        }
        Ok(CancelOutcome {
            cancelled_turn: plan.send_cancel,
            returned_to_composer: returned,
        })
    }

    /// Answers a permission request the agent is waiting on.
    ///
    /// `request_id` names the agent's request, and only the agent's: it is
    /// looked up among the requests the agent made, never among the ones this
    /// service sent. The agent numbers its requests from 0, so its request 1
    /// and the client's request 1 are unrelated, and a response is matched to
    /// its request by direction as well as by id.
    async fn answer_permission(&mut self, request_id: &str, option_id: &str) -> Result<(), ChatError> {
        if !matches!(self.link_state.link, Link::Up(_)) {
            return Err(ChatError::Disconnected);
        }
        let pending = self.convo.reducer.pending_permission_request_ids();
        // Anything no longer pending has been answered and echoed.
        self.convo
            .answered_permissions
            .retain(|answered| pending.iter().any(|id| id.to_string() == *answered));
        let Some(id) = pending.into_iter().find(|id| id.to_string() == request_id) else {
            return Err(ChatError::Rejected(format!(
                "there is no permission request {} waiting for an answer; it may already have been answered or withdrawn",
                self.wiring.redactor.redact_str(request_id)
            )));
        };
        if self.convo.answered_permissions.contains(request_id) {
            return Err(ChatError::Rejected(format!(
                "permission request {} was already answered",
                self.wiring.redactor.redact_str(request_id)
            )));
        }
        let offered = self
            .convo
            .reducer
            .items()
            .iter()
            .rev()
            .find_map(|item| match &item.kind {
                CoordinatorChatItemKind::PermissionRequest(request)
                    if request.request_id == request_id
                        && request.resolution == CoordinatorChatPermissionResolution::Pending =>
                {
                    Some(request.options.iter().any(|option| option.option_id == option_id))
                }
                _ => None,
            });
        if offered != Some(true) {
            return Err(ChatError::Rejected(format!(
                "{} is not one of the options the agent offered for permission request {}",
                self.wiring.redactor.redact_str(option_id),
                self.wiring.redactor.redact_str(request_id)
            )));
        }
        self.convo.answered_permissions.insert(request_id.to_owned());
        self.write(&lines::permission_response_line(
            &id,
            PermissionAnswer::Selected(option_id),
        ))
        .await;
        Ok(())
    }

    /// The conversation after `after_seq`.
    ///
    /// Items are selected by the last record that changed them, not the one
    /// that created them: a tool call created at record 30 and finished at
    /// record 40 is news to a client that has seen only up to record 35, and
    /// a client that missed the push for the update needs it here.
    fn fetch(&self, after_seq: u64) -> CoordinatorChatFetchOutcome {
        CoordinatorChatFetchOutcome {
            generation: self.wiring.generation.id().to_owned(),
            items: self
                .convo
                .reducer
                .items()
                .iter()
                .filter(|item| item.updated_seq > after_seq)
                .cloned()
                .collect(),
            last_seq: self.convo.reducer.last_wire_seq(),
            state: self.convo.reducer.state().clone(),
            in_progress: self.convo.reducer.in_progress_message(),
        }
    }

    // ---- the host's records -------------------------------------------------

    async fn apply_records(&mut self, records: Vec<WireRecord>) {
        let now = Instant::now();
        for record in records {
            // The host replays from the sequence number asked for, so this is
            // only a guard against applying a record twice.
            if record.seq <= self.convo.reducer.last_wire_seq() {
                continue;
            }
            let delta = self.convo.reducer.apply_record(&record);
            for id in &delta.upserted_item_ids {
                if let Some(item) = self.convo.reducer.items().iter().rev().find(|item| item.id == *id) {
                    self.publish.queue.push_item(item.clone(), now);
                }
            }
            if let Some(message_delta) = delta.message_delta {
                self.publish.queue.push_delta(message_delta, now);
            }
            self.publish_state_if_changed(now);
            self.upkeep.records_since_snapshot += 1;
        }
        self.finish_catch_up();
        self.maybe_snapshot();
        self.dispatch().await;
    }

    fn publish_state_if_changed(&mut self, now: Instant) {
        let state = self.convo.reducer.state();
        // Every record moves `last_seq`; that alone is not a change worth a push.
        let mut comparable = state.clone();
        comparable.last_seq = self.publish.published_state.last_seq;
        if comparable != self.publish.published_state {
            self.publish.queue.push_state(state.clone(), now);
        }
        self.publish.published_state = state.clone();
    }

    /// Once the host's records up to the boundary are applied, a prompt the
    /// intent log says was sent but that never reached the journal (the
    /// previous engine died between the two, or the line was cut short) is
    /// sent again, under a new request id.
    fn finish_catch_up(&mut self) {
        if self.link_state.caught_up || self.convo.reducer.last_wire_seq() < self.link_state.boundary_seq {
            return;
        }
        self.link_state.caught_up = true;
        for prompt in self.convo.reducer.reconcile_unsent_dispatches() {
            match self.convo.intent.append_requeued(prompt.intent_id) {
                Ok(record) => {
                    self.convo.reducer.apply_intent_record(&record);
                }
                Err(error) => {
                    self.storage_failed(&error);
                    return;
                }
            }
        }
        self.set_status(ConnectionStatus::Live);
    }

    /// Sends the next queued prompt, if the agent is idle and the session is
    /// ready for one.
    async fn dispatch(&mut self) {
        let ready = matches!(self.link_state.link, Link::Up(_))
            && self.link_state.caught_up
            && !matches!(
                self.convo.reducer.state().phase,
                CoordinatorChatConnectionPhase::Failed { .. }
            );
        let Some(session_id) = self.convo.reducer.state().session_id.clone() else {
            return;
        };
        if !ready {
            return;
        }
        let Some(next) = self.convo.reducer.queue().peek_next_dispatchable().cloned() else {
            return;
        };
        let id = self
            .convo
            .reducer
            .highest_numeric_client_request_id()
            .map_or(1, |highest| highest + 1);
        // Logged as sent before the line is written. A crash in between leaves
        // a prompt the log says was sent and the journal never saw, which
        // recovery sends again; the other order could send a prompt twice.
        let record = match self.convo.intent.append_sent(next.intent_id, RequestId::Number(id)) {
            Ok(record) => record,
            Err(error) => {
                self.storage_failed(&error);
                return;
            }
        };
        self.convo.reducer.apply_intent_record(&record);
        self.write(&lines::prompt_line(id, &session_id, next.text())).await;
    }

    async fn write(&mut self, line: &str) {
        let Link::Up(connection) = &mut self.link_state.link else {
            return;
        };
        if let Err(error) = connection.write_line(line).await {
            self.link_lost(&error);
        }
    }

    // ---- the connection -----------------------------------------------------

    async fn connect(&mut self) {
        let replay_from = self.convo.reducer.last_wire_seq() + 1;
        match HostConnection::connect(&self.wiring.endpoint, replay_from).await {
            Ok(connection) => {
                let wire_path = self.wiring.generation.wire_path();
                let boundary = tokio::task::spawn_blocking(move || last_journal_seq(&wire_path)).await;
                let boundary = match boundary {
                    Ok(Ok(seq)) => seq,
                    Ok(Err(error)) => return self.connect_failed(format!("reading the host's journal: {error}")),
                    Err(error) => return self.connect_failed(format!("reading the host's journal: {error}")),
                };
                tracing::info!(
                    generation = self.wiring.generation.id(),
                    replay_from,
                    boundary,
                    "attached to the agent host"
                );
                self.link_state = LinkState {
                    link: Link::Up(connection),
                    caught_up: false,
                    boundary_seq: boundary,
                    failures: 0,
                };
                self.set_status(ConnectionStatus::CatchingUp);
                self.finish_catch_up();
                self.dispatch().await;
            }
            Err(ConnectError::Refused(reason)) => {
                tracing::error!(generation = self.wiring.generation.id(), %reason, "refusing the agent host socket");
                self.set_status(ConnectionStatus::Stopped { reason });
            }
            Err(ConnectError::Io(error)) => self.connect_failed(error.to_string()),
        }
    }

    fn connect_failed(&mut self, reason: String) {
        self.go_down(&reason);
    }

    fn link_lost(&mut self, error: &LinkError) {
        tracing::warn!(generation = self.wiring.generation.id(), %error, "lost the agent host");
        self.go_down(&error.to_string());
    }

    fn go_down(&mut self, reason: &str) {
        let failures = self.link_state.failures;
        // Capped doubling from the engine's shared backoff, with the usual
        // jitter. Not a measured limit: only a way not to spin on a host that
        // is not there.
        let delay = boss_backoff::equal_jitter(boss_backoff::exponential_delay(
            self.wiring.tuning.reconnect_initial_delay,
            self.wiring.tuning.reconnect_max_delay,
            failures,
        ));
        self.link_state = LinkState {
            link: Link::Down {
                retry_at: Instant::now() + delay,
            },
            caught_up: false,
            boundary_seq: 0,
            failures: failures.saturating_add(1),
        };
        self.set_status(ConnectionStatus::Reconnecting {
            reason: self.wiring.redactor.redact_str(reason),
        });
    }

    fn set_status(&mut self, status: ConnectionStatus) {
        self.publish.status.send_if_modified(|current| {
            // Stopped is final.
            if matches!(current, ConnectionStatus::Stopped { .. }) || *current == status {
                return false;
            }
            *current = status;
            true
        });
    }

    // ---- publishing and persistence -----------------------------------------

    fn flush_if_due(&mut self) {
        if self.publish.queue.due().is_some_and(|due| due <= Instant::now()) {
            self.flush();
        }
    }

    fn flush(&mut self) {
        for update in self.publish.queue.take() {
            // Nobody subscribed is not an error: the app fetches when it attaches.
            let _ = self.publish.pushes.send(ChatPush {
                generation: self.wiring.generation.id().to_owned(),
                update,
            });
        }
    }

    fn maybe_snapshot(&mut self) {
        if self
            .upkeep
            .snapshot_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        self.upkeep.snapshot_task = None;
        if self.upkeep.records_since_snapshot < self.wiring.tuning.snapshot_interval_records {
            return;
        }
        self.upkeep.records_since_snapshot = 0;
        let snapshot = self.convo.reducer.snapshot();
        let path = self.wiring.generation.snapshot_path();
        self.upkeep.snapshot_task = Some(tokio::task::spawn_blocking(move || {
            if let Err(error) = save_snapshot(&path, &snapshot) {
                tracing::warn!(%error, "could not write a reducer snapshot");
            }
        }));
    }

    /// Records the failure of a write to the intent log. A log that cannot be
    /// written means a message could be accepted and then lost, so the
    /// service stops accepting rather than carry on.
    fn storage_failed(&mut self, error: &std::io::Error) -> ChatError {
        let reason = self
            .wiring
            .redactor
            .redact_str(&format!("the intent log could not be written: {error}"));
        tracing::error!(generation = self.wiring.generation.id(), %reason, "stopping the chat service");
        self.set_status(ConnectionStatus::Stopped { reason: reason.clone() });
        ChatError::Storage(reason)
    }

    /// Publishes what is waiting and writes a last snapshot.
    async fn finish(&mut self) {
        self.flush();
        // An earlier snapshot still being written must not land after this one.
        if let Some(task) = self.upkeep.snapshot_task.take() {
            let _ = task.await;
        }
        let snapshot = self.convo.reducer.snapshot();
        let path = self.wiring.generation.snapshot_path();
        match tokio::task::spawn_blocking(move || save_snapshot(&path, &snapshot)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not write the final reducer snapshot"),
            Err(error) => tracing::warn!(%error, "the final reducer snapshot task failed"),
        }
        // Closing the connection and the command channel is the drop of `self`.
    }
}

async fn read_link(link: &mut Link) -> Result<Vec<WireRecord>, LinkError> {
    match link {
        Link::Up(connection) => connection.read_records().await,
        Link::Down { .. } => pending().await,
    }
}

/// Resolves at `deadline`, or never.
async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(TokioInstant::from_std(deadline)).await,
        None => pending::<()>().await,
    }
}
