use crate::{
    ChallengeOutcome, Connection, StreamManager,
    codec::{Outbound, RawFrame, WireCodec},
    contracts,
    session::{Driver, watch_credentials},
    status,
    store::Session,
};
use boss_protocol_spoke::{
    limits::MAX_FRAME_BYTES,
    v1::{self, hub_frame, spoke_frame},
};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming, codegen::*};

/// The same generated SpokeHub route, using a lossless inbound codec.
#[derive(Clone)]
pub struct StreamService(pub(crate) StreamManager);

impl tonic::server::NamedService for StreamService {
    const NAME: &'static str = "boss.spoke.v1.SpokeHub";
}

impl<B> Service<http::Request<B>> for StreamService
where
    B: Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::BoxBody>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let manager = self.0.clone();
        Box::pin(async move {
            if request.uri().path() != "/boss.spoke.v1.SpokeHub/Connect" {
                return Ok(Status::unimplemented("unknown method").into_http());
            }
            struct Connect(StreamManager);
            impl tonic::server::StreamingService<RawFrame> for Connect {
                type Response = Outbound;
                type ResponseStream = Output;
                type Future = BoxFuture<Response<Output>, Status>;
                fn call(&mut self, request: Request<Streaming<RawFrame>>) -> Self::Future {
                    let manager = self.0.clone();
                    Box::pin(async move { manager.connect(request).await.map_err(|status| *status) })
                }
            }
            // No compression is enabled in epoch 1. Size is checked before decode.
            Ok(tonic::server::Grpc::new(WireCodec)
                .max_decoding_message_size(MAX_FRAME_BYTES as usize)
                .max_encoding_message_size(MAX_FRAME_BYTES as usize)
                .streaming(Connect(manager), request)
                .await)
        })
    }
}

pub(crate) struct Output {
    rx: mpsc::Receiver<Result<Outbound, Status>>,
    active: Arc<AtomicBool>,
    terminal: Arc<std::sync::Mutex<Option<Status>>>,
}
impl Stream for Output {
    type Item = Result<Outbound, Status>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // A replaced stream cannot drain buffered commands after being fenced.
        if !self.active.load(Ordering::Acquire) {
            return Poll::Ready(self.terminal.lock().expect("terminal lock").take().map(Err));
        }
        self.rx.poll_recv(cx)
    }
}

impl StreamManager {
    async fn connect(&self, request: Request<Streaming<RawFrame>>) -> Result<Response<Output>, Box<Status>> {
        let permit = self
            .0
            .connections
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("connection limit"))?;
        let mut events = self.0.auth.subscribe(); // Before authorization: no revocation race.
        let chain = request
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let auth = self.0.auth.clone();
        let mut identity = tokio::task::spawn_blocking(move || auth.authorize(&chain))
            .await
            .map_err(|_| Status::internal("authorization failed"))?
            .map_err(|_| Status::unauthenticated("not permitted"))?;
        let mut inbound = request.into_inner();
        let first = tokio::time::timeout(self.0.config.hello_timeout, inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("Hello deadline"))??;
        let Some(spoke_frame::Body::Hello(hello)) = first.and_then(|f| f.frame.body) else {
            return Err(Status::invalid_argument("first frame must be Hello").into());
        };
        if hello.host_id != identity.host_id {
            return Err(Status::permission_denied("certificate host mismatch").into());
        }
        let (tx, rx) = mpsc::channel(1);
        let active = Arc::new(AtomicBool::new(true));
        let terminal = Arc::new(std::sync::Mutex::new(None));
        let selection = match contracts().select(&hello) {
            Ok(selection) => selection,
            Err(rejection) => {
                tx.try_send(Ok(frame(0, hub_frame::Body::Reject(rejection.to_reject(&contracts())))))
                    .expect("empty channel");
                return Ok(Response::new(Output { rx, active, terminal }));
            }
        };
        let host = self.host(&hello.host_id).await?;
        let mut gate = host.lock().await;
        if let Some(old) = gate
            .as_ref()
            .filter(|old| old.active.load(Ordering::Acquire) && old.nonce != hello.instance_nonce)
        {
            let generation = old.session.generation;
            let (reply, answer) = oneshot::channel();
            old.probe
                .try_send(reply)
                .map_err(|_| Status::already_exists("host challenge pending"))?;
            drop(gate);
            // The driver owns the challenge deadline: it starts when the Pulse is
            // actually sent, which can trail this request while the driver is in
            // storage or a stalled write. Replace only on its Expired outcome or
            // when the driver is gone. The overall bound covers one stalled write
            // plus one challenge; past it the contender retries and the live
            // stream is left alone, since slowness is not proof of death.
            match tokio::time::timeout(self.0.config.pulse_timeout.saturating_mul(3), answer).await {
                Ok(Ok(ChallengeOutcome::Answered)) => {
                    return Err(Status::already_exists("another daemon answered the challenge").into());
                }
                Ok(Ok(ChallengeOutcome::Pending)) => {
                    return Err(Status::already_exists("host challenge pending").into());
                }
                Err(_) => return Err(Status::unavailable("host challenge still in progress; retry").into()),
                Ok(Ok(ChallengeOutcome::Expired)) | Ok(Err(_)) => {}
            }
            gate = host.lock().await;
            if gate.as_ref().is_some_and(|c| c.session.generation != generation) {
                return Err(Status::aborted("connection changed; reconnect").into());
            }
        }
        // Recheck after Hello/challenge: expiry or revocation may have happened.
        let auth = self.0.auth.clone();
        identity = tokio::task::spawn_blocking(move || auth.revalidate(&identity))
            .await
            .map_err(|_| Status::internal("authorization failed"))?
            .map_err(|_| Status::unauthenticated("not permitted"))?;
        // Storage admission can be slow: watch the credential across it so a
        // revocation or expiry stops admission instead of fencing a live stream
        // for an identity that is already dead.
        let (journal, admission) = {
            let credentials = watch_credentials(self.0.auth.clone(), &mut identity, &mut events);
            tokio::pin!(credentials);
            let journal = tokio::select! {
                biased;
                _ = &mut credentials => return Err(Status::unauthenticated("credential no longer valid").into()),
                journal = self.0.storage.outbox.resume(&hello.host_id, hello.received_commands.as_ref()) => journal.map_err(status)?,
            };
            // If this RPC is cancelled while the durable generation is committing,
            // fence the old session even though we cannot know whether it committed.
            // A known rejection preserves the current stream.
            let mut claim = ClaimGuard {
                slot: &mut gate,
                armed: true,
            };
            let admission = tokio::select! {
                biased;
                _ = &mut credentials => return Err(Status::unauthenticated("credential no longer valid").into()),
                admission = self.0.storage.sink.open(&hello, &selection) => match admission {
                    Ok(admission) => admission,
                    Err(error) => {
                        claim.armed = false;
                        return Err(status(error).into());
                    }
                },
            };
            (journal, admission)
        };
        // The claim is committed and has fenced any old stream. Revalidate before
        // publishing: a credential lost during open must never get a Welcome.
        let auth = self.0.auth.clone();
        let revalidated = tokio::task::spawn_blocking({
            let previous = identity.clone();
            move || auth.revalidate(&previous)
        })
        .await;
        match revalidated {
            Ok(Ok(current)) => identity = current,
            _ => {
                gate.take();
                return Err(Status::unauthenticated("credential no longer valid").into());
            }
        }
        // Defense in depth: never advertise a fabricated receipt or bridge a gap.
        let receipt = &admission.received;
        if admission.generation == 0
            || receipt.journal_id != hello.journal_id
            || receipt.through > hello.last_record
            || hello.first_retained > receipt.through.saturating_add(1)
        {
            gate.take();
            return Err(Status::failed_precondition("replay_gap").into());
        }
        let session = Session {
            host_id: hello.host_id.clone(),
            journal_id: hello.journal_id.clone(),
            generation: admission.generation,
            selection,
        };
        let welcome = v1::Welcome {
            epoch: session.selection.epoch,
            features: session.selection.to_features(),
            hub_id: self.0.config.hub_id.clone(),
            authority_generation: self.0.config.authority_generation.clone(),
            connection_generation: session.generation,
            received_records: Some(admission.received.clone()),
            command_journal_id: journal.journal_id.clone(),
            max_frame_bytes: MAX_FRAME_BYTES,
            max_inflight_bytes: boss_protocol_spoke::limits::MAX_INFLIGHT_BYTES,
            build: self.0.config.build_fingerprint.clone(),
        };
        let (probe, probes) = mpsc::channel(1);
        let wake = Arc::new(Notify::new());
        *gate = Some(
            Connection::builder()
                .session(session.clone())
                .nonce(hello.instance_nonce)
                .active(active.clone())
                .probe(probe)
                .terminal(terminal.clone())
                .wake(wake.clone())
                .waiters(Default::default())
                .build(),
        );
        drop(gate);
        tx.try_send(Ok(frame(session.generation, hub_frame::Body::Welcome(welcome))))
            .expect("empty channel");
        let driver = Driver::builder()
            .manager(self.clone())
            .host(host)
            .session(session)
            .inbound(inbound)
            .tx(tx)
            .active(active.clone())
            .terminal(terminal.clone())
            .wake(wake)
            .probes(probes)
            .record_through(admission.received.through)
            .command_journal(journal.journal_id)
            .command_through(journal.received)
            .build();
        tokio::spawn(driver.run(identity, events, permit));
        Ok(Response::new(Output { rx, active, terminal }))
    }
}

struct ClaimGuard<'a> {
    slot: &'a mut Option<Connection>,
    armed: bool,
}
impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.slot.take();
        }
    }
}

pub(crate) fn frame(generation: u64, body: hub_frame::Body) -> Outbound {
    Outbound::Frame(Box::new(v1::HubFrame {
        connection_generation: generation,
        body: Some(body),
    }))
}
