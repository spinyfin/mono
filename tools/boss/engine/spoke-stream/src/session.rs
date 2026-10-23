use crate::{
    ChallengeOutcome, Host, StreamManager,
    codec::{Outbound, RawFrame},
    service::frame,
    status,
    store::{PendingCommand, Session},
};
use boss_host_enrollment::{AuthorizedHost, CredentialEvent, HostAuthorizer};
use boss_protocol_spoke::{
    Validate,
    limits::MAX_FRAME_BYTES,
    negotiate::feature,
    v1::{self, hub_frame, record, spoke_frame},
};
use prost::Message;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::SystemTime;
use tokio::sync::{Notify, OwnedSemaphorePermit, broadcast, mpsc, oneshot};
use tonic::{Status, Streaming};

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct Driver {
    manager: StreamManager,
    host: Host,
    session: Session,
    inbound: Streaming<RawFrame>,
    tx: mpsc::Sender<Result<Outbound, Status>>,
    active: Arc<AtomicBool>,
    terminal: Arc<std::sync::Mutex<Option<Status>>>,
    wake: Arc<Notify>,
    probes: mpsc::Receiver<oneshot::Sender<ChallengeOutcome>>,
    record_through: u64,
    command_journal: String,
    command_through: u64,
}

impl Driver {
    pub(crate) async fn run(
        mut self,
        mut identity: AuthorizedHost,
        mut events: broadcast::Receiver<CredentialEvent>,
        _permit: OwnedSemaphorePermit,
    ) {
        let auth = self.manager.0.auth.clone();
        let closed = self.tx.clone();
        let terminal = tokio::select! {
            // Credentials first: a revoked or expired identity must not get one
            // more command write or record commit from a ready transport branch.
            biased;
            _ = watch_credentials(auth, &mut identity, &mut events) => Some(Status::unauthenticated("credential no longer valid")),
            result = self.drive() => result.err().map(|status| *status),
            _ = closed.closed() => None,
        };
        // A fence publishes its own status first; do not overwrite it.
        {
            let mut slot = self.terminal.lock().expect("terminal lock");
            if slot.is_none() {
                *slot = terminal;
            }
        }
        self.active.store(false, Ordering::Release);
        // Wake an idle response body before waiting for a concurrent outbox
        // operation to release the host gate. A full queue is already readable.
        let _ = self.tx.try_send(Err(Status::cancelled("stream closed")));
        let mut gate = self.host.lock().await;
        if gate
            .as_ref()
            .is_some_and(|c| c.session.generation == self.session.generation)
        {
            gate.take();
        }
        // Dropping the sender wakes the response body even when fenced idle.
    }

    async fn send(&mut self, body: Outbound) -> Result<(), Box<Status>> {
        tokio::time::timeout(self.manager.0.config.pulse_timeout, self.tx.send(Ok(body)))
            .await
            .map_err(|_| Status::deadline_exceeded("stream write stalled"))?
            .map_err(|_| Box::new(Status::unavailable("stream closed")))
    }

    async fn drive(&mut self) -> Result<(), Box<Status>> {
        let mut ticks = tokio::time::interval(self.manager.0.config.pulse_interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut challenge: Option<Challenge> = None;
        let mut command_received = self.command_through;
        loop {
            if !self.active.load(Ordering::Acquire) {
                return Ok(());
            }
            // One unreceipted command per connection bounds bytes even for a
            // 512 KiB spawn. Results may remain pending independently.
            if self.command_through == command_received {
                let mut gate = self.host.lock().await;
                self.check(&gate)?;
                let next = self
                    .manager
                    .0
                    .storage
                    .outbox
                    .next(&self.session.host_id, self.command_through)
                    .await
                    .map_err(status)?;
                if let Some(pending) = next {
                    let decoded = v1::Command::decode(pending.bytes.as_slice())
                        .map_err(|_| "invalid durable command bytes")
                        .and_then(|command| {
                            command
                                .validate()
                                .map(|()| command)
                                .map_err(|_| "invalid durable command")
                        });
                    let reason = match &decoded {
                        Err(reason) => Some(*reason),
                        Ok(command) => self.replay_problem(command, &pending),
                    };
                    if let Some(reason) = reason {
                        // One stale command must never block the queue: settle
                        // it durably, tell any waiter, and move on to the next.
                        let id = decoded.as_ref().map_or("", |c| c.command_id.as_str());
                        let result = self
                            .manager
                            .0
                            .storage
                            .outbox
                            .reject(&self.session.host_id, pending.sequence, id, reason)
                            .await
                            .map_err(status)?;
                        if let Some(sender) = gate.as_mut().and_then(|c| c.waiters.remove(id)) {
                            let _ = sender.send(result);
                        }
                        self.command_through = self.command_through.max(pending.sequence);
                        command_received = self.command_through;
                        continue;
                    }
                    drop(gate);
                    self.command_through = pending.sequence;
                    self.send(Outbound::Command {
                        generation: self.session.generation,
                        bytes: pending.bytes,
                    })
                    .await?;
                }
            }
            let deadline = challenge
                .as_ref()
                .map(|c| c.until)
                .unwrap_or_else(|| tokio::time::Instant::now() + self.manager.0.config.pulse_timeout);
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline), if challenge.is_some() => {
                    if let Some(reply) = challenge.take().and_then(|c| c.reply) {
                        let _ = reply.send(ChallengeOutcome::Expired);
                    }
                    return Err(Status::deadline_exceeded("nonce challenge expired").into());
                },
                probe = self.probes.recv() => {
                    if let Some(reply) = probe {
                        match challenge.as_mut() {
                            // A contender's challenge is already outstanding. Refuse
                            // the additional one; do not displace its waiter.
                            Some(c) if c.reply.is_some() => {
                                let _ = reply.send(ChallengeOutcome::Pending);
                            }
                            // A routine pulse is in flight: its deadline decides.
                            Some(c) => c.reply = Some(reply),
                            None => challenge = Some(self.challenge(Some(reply)).await?),
                        }
                    }
                },
                _ = ticks.tick(), if challenge.is_none() => { challenge = Some(self.challenge(None).await?); },
                _ = self.wake.notified() => {},
                incoming = self.inbound.message() => {
                    let Some(raw) = incoming? else { return Ok(()); };
                    match raw.frame.body {
                        Some(spoke_frame::Body::PulseReply(pulse)) => {
                            pulse.validate().map_err(|_| Status::invalid_argument("invalid pulse"))?;
                            if challenge.as_ref().is_some_and(|c| c.nonce == pulse.nonce)
                                && let Some(reply) = challenge.take().and_then(|c| c.reply) {
                                let _ = reply.send(ChallengeOutcome::Answered);
                            }
                        },
                        Some(spoke_frame::Body::CommandReceipt(cursor)) => {
                            if cursor.journal_id != self.command_journal || cursor.through < command_received || cursor.through > self.command_through {
                                return Err(Status::failed_precondition("invalid command receipt").into());
                            }
                            let gate = self.host.lock().await;
                            self.check(&gate)?;
                            self.manager.0.storage.outbox.receipt(&self.session.host_id, &cursor).await.map_err(status)?;
                            command_received = cursor.through;
                        },
                        Some(spoke_frame::Body::Record(record)) => self.record(&raw.raw, record).await?,
                        _ => return Err(Status::invalid_argument("unexpected frame").into()),
                    }
                }
            }
        }
    }

    /// Why a durable command cannot be written to this connection, if so.
    fn replay_problem(&self, command: &v1::Command, pending: &PendingCommand) -> Option<&'static str> {
        if command.sequence != pending.sequence || pending.sequence <= self.command_through {
            Some("durable command sequence mismatch")
        } else if pending.bytes.len() + 32 > MAX_FRAME_BYTES as usize {
            Some("command exceeds the frame limit")
        } else if !crate::commands::supported(command, &self.session.selection) {
            Some("command incompatible with the negotiated contract")
        } else {
            None
        }
    }

    fn check(&self, connection: &Option<crate::Connection>) -> Result<(), Box<Status>> {
        if !self.active.load(Ordering::Acquire)
            || !connection
                .as_ref()
                .is_some_and(|c| c.session.generation == self.session.generation)
        {
            return Err(Status::aborted("fenced connection").into());
        }
        Ok(())
    }

    async fn record(&mut self, bytes: &[u8], record: v1::Record) -> Result<(), Box<Status>> {
        record
            .validate()
            .map_err(|_| Status::invalid_argument("invalid or unsupported record"))?;
        if record.sequence > self.record_through.saturating_add(1) {
            return Err(Status::failed_precondition("record sequence gap").into());
        }
        if matches!(record.body, Some(record::Body::Capture(_))) && !self.session.selection.has(feature::PANE_CAPTURE) {
            return Err(Status::failed_precondition("pane capture not negotiated").into());
        }
        let mut gate = self.host.lock().await;
        self.check(&gate)?;
        let outbox = &self.manager.0.storage.outbox;
        if let Some(record::Body::Result(result)) = &record.body {
            let command = outbox
                .command(&self.session.host_id, &result.command_id)
                .await
                .map_err(status)?;
            if !command.is_some_and(|c| c.run == record.run) {
                return Err(Status::permission_denied("result does not match command").into());
            }
        }
        let receipt = self
            .manager
            .0
            .storage
            .sink
            .commit(&self.session, bytes, &record)
            .await
            .map_err(status)?;
        if receipt.journal_id != self.session.journal_id || receipt.through != self.record_through.max(record.sequence)
        {
            return Err(Status::internal("sink returned invalid receipt").into());
        }
        self.record_through = receipt.through;
        if let Some(record::Body::Result(result)) = &record.body
            && let Some(sender) = gate.as_mut().and_then(|c| c.waiters.remove(&result.command_id))
        {
            let _ = sender.send(result.clone());
        }
        drop(gate);
        self.send(frame(self.session.generation, hub_frame::Body::RecordReceipt(receipt)))
            .await
    }

    async fn challenge(&mut self, reply: Option<oneshot::Sender<ChallengeOutcome>>) -> Result<Challenge, Box<Status>> {
        let nonce = rand::random::<[u8; 32]>().to_vec();
        self.send(frame(
            self.session.generation,
            hub_frame::Body::Pulse(v1::Pulse {
                nonce: nonce.clone(),
                stamp: None,
            }),
        ))
        .await?;
        Ok(Challenge {
            nonce,
            until: tokio::time::Instant::now() + self.manager.0.config.pulse_timeout,
            reply,
        })
    }
}

struct Challenge {
    nonce: Vec<u8>,
    until: tokio::time::Instant,
    reply: Option<oneshot::Sender<ChallengeOutcome>>,
}

/// Resolves once the credential is revoked, expired or can no longer be
/// validated. Cancellation-safe: `identity` and `events` keep their progress so
/// admission and the driver can share one view of the credential.
pub(crate) async fn watch_credentials(
    auth: Arc<dyn HostAuthorizer>,
    identity: &mut AuthorizedHost,
    events: &mut broadcast::Receiver<CredentialEvent>,
) {
    loop {
        let remaining = identity
            .valid_until
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(remaining) => return,
            event = events.recv() => match event {
                Ok(CredentialEvent::Revoked { host_id }) if host_id == identity.host_id => return,
                Ok(CredentialEvent::Superseded { host_id, serial, valid_until }) if host_id == identity.host_id && serial == identity.serial => {
                    identity.valid_until = identity.valid_until.min(valid_until);
                },
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let auth = auth.clone();
                    let previous = identity.clone();
                    match tokio::task::spawn_blocking(move || auth.revalidate(&previous)).await {
                        Ok(Ok(current)) => *identity = current,
                        _ => return,
                    }
                },
                _ => {},
            }
        }
    }
}
