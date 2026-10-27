//! Outbound-only journal transport. Network failures retry; identity, storage,
//! protocol and replay failures stop for reconciliation without losing evidence.
/// Preserves original nested protobuf bytes across journal replay. The generated
/// client would decode/re-encode them and discard unknown fields.
mod codec {
    use boss_protocol_spoke::v1::HubFrame;
    use prost::{
        Message,
        bytes::{Buf, BufMut},
    };
    use tonic::{
        Status,
        codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    };

    pub struct RawCodec;
    pub struct RawEncoder;
    pub struct RawDecoder;
    pub struct Received {
        pub frame: HubFrame,
        pub command: Option<Vec<u8>>,
        pub size: usize,
    }

    #[derive(Message)]
    struct CommandBytes {
        #[prost(bytes = "vec", optional, tag = "3")]
        command: Option<Vec<u8>>,
    }

    #[derive(Message)]
    struct RecordBytes {
        #[prost(bytes = "vec", optional, tag = "2")]
        record: Option<Vec<u8>>,
    }

    pub fn record_frame(bytes: Vec<u8>) -> Vec<u8> {
        RecordBytes { record: Some(bytes) }.encode_to_vec()
    }

    impl Codec for RawCodec {
        type Encode = Vec<u8>;
        type Decode = Received;
        type Encoder = RawEncoder;
        type Decoder = RawDecoder;
        fn encoder(&mut self) -> RawEncoder {
            RawEncoder
        }
        fn decoder(&mut self) -> RawDecoder {
            RawDecoder
        }
    }

    impl Encoder for RawEncoder {
        type Item = Vec<u8>;
        type Error = Status;
        fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
            dst.put_slice(&item);
            Ok(())
        }
    }

    impl Decoder for RawDecoder {
        type Item = Received;
        type Error = Status;
        fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Received>, Status> {
            let bytes = src.copy_to_bytes(src.remaining());
            let frame = HubFrame::decode(bytes.clone()).map_err(|e| Status::data_loss(e.to_string()))?;
            let command = CommandBytes::decode(bytes.clone())
                .map_err(|e| Status::data_loss(e.to_string()))?
                .command;
            Ok(Some(Received {
                frame,
                command,
                size: bytes.len(),
            }))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use boss_protocol_spoke::v1::{Command, Record};

        #[test]
        fn unknown_fields_survive_both_journal_envelopes() {
            let mut raw = Record {
                sequence: 7,
                ..Default::default()
            }
            .encode_to_vec();
            raw.extend_from_slice(&[0xa0, 0x06, 0x07]);
            assert_ne!(Record::decode(raw.as_slice()).unwrap().encode_to_vec(), raw);
            let frame = record_frame(raw.clone());
            assert_eq!(RecordBytes::decode(frame.as_slice()).unwrap().record.unwrap(), raw);

            let mut raw = Command {
                sequence: 3,
                ..Default::default()
            }
            .encode_to_vec();
            raw.extend_from_slice(&[0xa0, 0x06, 0x07]);
            let frame = CommandBytes {
                command: Some(raw.clone()),
            }
            .encode_to_vec();
            assert!(HubFrame::decode(frame.as_slice()).unwrap().body.is_some());
            assert_eq!(CommandBytes::decode(frame.as_slice()).unwrap().command.unwrap(), raw);
        }
    }
}
mod session {
    use anyhow::{Result, bail, ensure};
    use boss_protocol_spoke::{
        Validate,
        limits::{Limits, MAX_FRAME_BYTES},
        v1::{self, hub_frame, spoke_frame},
    };
    use boss_spoke_journal::Fault;
    use prost::Message;
    use std::collections::VecDeque;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::{Request, Streaming, client::Grpc, transport::Endpoint};

    use super::{
        SharedJournal,
        codec::{RawCodec, Received, record_frame},
        contracts, hello, locked, network, status,
    };

    const DEADLINE: Duration = Duration::from_secs(20);

    /// Records sent and not yet receipted, bounded by the negotiated byte credit.
    /// Transport flow control is not receipt.
    struct Window {
        acknowledged: u64,
        sent: u64,
        outstanding: usize,
        frames: VecDeque<(u64, usize)>,
    }

    impl Window {
        fn new(acknowledged: u64) -> Self {
            Self {
                acknowledged,
                sent: acknowledged,
                outstanding: 0,
                frames: VecDeque::new(),
            }
        }

        fn sent(&mut self, sequence: u64, bytes: usize) {
            self.sent = sequence;
            self.outstanding += bytes;
            self.frames.push_back((sequence, bytes));
        }

        /// Release the credit of every record the hub has now receipted.
        fn receipted(&mut self, through: u64) {
            self.acknowledged = through;
            while let Some(&(sequence, bytes)) = self.frames.front() {
                if sequence > through {
                    break;
                }
                self.outstanding -= bytes;
                self.frames.pop_front();
            }
        }
    }

    /// One bounded session, also used by the real tonic fault tests. Records are
    /// pipelined up to the negotiated byte credit and released by receipts.
    pub async fn connect_once(
        journal: &SharedJournal,
        endpoint: Endpoint,
        nonce: &[u8],
        stamp: &impl Fn() -> Result<v1::Stamp>,
    ) -> Result<()> {
        connect_with_executor(journal, endpoint, nonce, stamp, false).await
    }

    /// External execution runs independently from connection liveness.
    pub async fn connect_with_executor(
        journal: &SharedJournal,
        endpoint: Endpoint,
        nonce: &[u8],
        stamp: &impl Fn() -> Result<v1::Stamp>,
        external_executor: bool,
    ) -> Result<()> {
        ensure!(
            endpoint.uri().scheme_str() == Some("https"),
            "spoke requires mTLS HTTPS"
        );
        let channel = tokio::time::timeout(DEADLINE, endpoint.connect())
            .await
            .map_err(network)?
            .map_err(network)?;
        let mut grpc = Grpc::new(channel)
            .max_decoding_message_size(MAX_FRAME_BYTES as usize)
            .max_encoding_message_size(MAX_FRAME_BYTES as usize);
        grpc.ready().await.map_err(network)?;
        let (tx, rx) = mpsc::channel(2);
        let mut hello = hello(&*locked(journal)?, &stamp()?.boot_id, nonce)?;
        let mut selected_contracts = contracts();
        if external_executor {
            selected_contracts.features.push(v1::Feature {
                name: "workspace".into(),
                revision: 1,
            });
            selected_contracts.required.push("workspace".into());
            hello.features = selected_contracts.features.clone();
        }
        send(
            &tx,
            control(spoke_frame::Body::Hello(hello.clone())),
            MAX_FRAME_BYTES as usize,
        )
        .await?;
        let path = tonic::codegen::http::uri::PathAndQuery::from_static("/boss.spoke.v1.SpokeHub/Connect");
        let response = tokio::time::timeout(
            DEADLINE,
            grpc.streaming(Request::new(ReceiverStream::new(rx)), path, RawCodec),
        )
        .await
        .map_err(network)?
        .map_err(status)?;
        let mut inbound = response.into_inner();
        let first = receive(&mut inbound).await?;
        let welcome = match first.frame.body {
            Some(hub_frame::Body::Welcome(welcome)) => welcome,
            Some(hub_frame::Body::Reject(reject)) => {
                bail!("hub rejected spoke: {}: {}", reject.code, reject.diagnostic)
            }
            _ => bail!("expected Welcome as first hub frame"),
        };
        selected_contracts.confirm(&welcome)?;
        ensure!(
            first.frame.connection_generation == welcome.connection_generation,
            "Welcome generation mismatch"
        );
        ensure!(
            welcome.received_records.as_ref().expect("validated receipt").through <= hello.last_record,
            "replay_gap: hub receipt exceeds advertised journal"
        );
        locked(journal)?.welcome(&welcome).map_err(|error| {
            if matches!(error.downcast_ref::<Fault>(), Some(Fault::TransportLimit)) {
                network(error)
            } else {
                error
            }
        })?;
        let limits = Limits::default().negotiated(welcome.max_frame_bytes, welcome.max_inflight_bytes);
        let frame_limit = limits.max_frame_bytes as usize;
        let credit = limits.max_inflight_bytes as usize;
        let mut window = Window::new(welcome.received_records.as_ref().expect("validated receipt").through);
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut last_read = tokio::time::Instant::now();
        let mut pending_command = None;
        let mut controls: VecDeque<Vec<u8>> = VecDeque::new();
        loop {
            // Waiting for transport capacity must not stop inbound receipts or pulses.
            let next = next_record(journal, &window, frame_limit, credit)?;
            if controls.len() > 32 {
                return Err(network("outbound control queue full"));
            }
            tokio::select! {
                permit = tx.reserve(), if !controls.is_empty() || next.is_some() => {
                    let permit = permit.map_err(network)?;
                    if let Some(bytes) = controls.pop_front() {
                        ensure!(bytes.len() <= frame_limit, "outbound frame exceeds negotiated limit");
                        permit.send(bytes);
                    } else if let Some((sequence, bytes)) = next {
                        window.sent(sequence, bytes.len());
                        permit.send(bytes);
                    }
                }
                message = inbound.message() => {
                    let message = message.map_err(status)?.ok_or_else(|| network("hub closed stream"))?;
                    ensure!(message.size <= frame_limit, "hub exceeded negotiated frame limit");
                    ensure!(message.frame.connection_generation == welcome.connection_generation, "stale stream generation");
                    message.frame.validate()?;
                    last_read = tokio::time::Instant::now();
                    match message.frame.body.expect("validated body") {
                        hub_frame::Body::RecordReceipt(cursor) => {
                            ensure!(cursor.through >= window.acknowledged && cursor.through <= window.sent, "receipt outside sent prefix");
                            locked(journal)?.receipt(&cursor)?;
                            window.receipted(cursor.through);
                        }
                        hub_frame::Body::Command(_) => {
                            if pending_command.is_some() {
                                return Err(network("hub pipelined an unreceipted command"));
                            }
                            // The hub waits for this receipt on a live connection.
                            // Retain raw bytes on Full and retry after compaction.
                            pending_command = message.command;
                            admit_command(journal, &mut pending_command, &mut controls)?;
                        }
                        hub_frame::Body::Pulse(mut pulse) => {
                            pulse.stamp = Some(stamp()?);
                            controls.push_back(control(spoke_frame::Body::PulseReply(pulse)));
                        }
                        _ => bail!("unexpected handshake inside established stream"),
                    }
                }
                _ = tick.tick() => {
                    if last_read.elapsed() > DEADLINE { return Err(network("hub read deadline expired")); }
                    // Recovery runs even if the receipt's network write was lost. A full
                    // spool leaves the command pending: replay and receipts below carry
                    // on, and compaction makes room for the result on a later tick.
                    let observed = stamp()?;
                    if pending_command.is_some() {
                        locked(journal)?.compact(observed.wall_unix_ms)?;
                        admit_command(journal, &mut pending_command, &mut controls)?;
                    }
                    let rejected = if external_executor { Ok(false) } else { locked(journal)?.reject_pending(&observed) };
                    if let Err(error) = rejected {
                        if !Fault::is_full(&error) {
                            return Err(error);
                        }
                        tracing::debug!("spool full; command result deferred");
                        locked(journal)?.compact(observed.wall_unix_ms)?;
                    }
                }
            }
        }
    }

    fn admit_command(
        journal: &SharedJournal,
        pending: &mut Option<Vec<u8>>,
        controls: &mut VecDeque<Vec<u8>>,
    ) -> Result<()> {
        let Some(bytes) = pending.as_ref() else {
            return Ok(());
        };
        let mut journal = locked(journal)?;
        match journal.receive_command(bytes) {
            Ok(_) => {
                let cursor = journal.received_commands()?.expect("bound command journal");
                controls.push_back(control(spoke_frame::Body::CommandReceipt(cursor)));
                *pending = None;
            }
            Err(error) if Fault::is_full(&error) => tracing::debug!("spool full; command admission deferred"),
            Err(error) if matches!(error.downcast_ref::<Fault>(), Some(Fault::TransportLimit)) => {
                return Err(network(error));
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    /// One record eligible under the negotiated byte credit, without awaiting I/O.
    fn next_record(
        journal: &SharedJournal,
        window: &Window,
        frame_limit: usize,
        credit: usize,
    ) -> Result<Option<(u64, Vec<u8>)>> {
        let Some(next) = locked(journal)?.next_record(window.sent)? else {
            return Ok(None);
        };
        let bytes = record_frame(next.bytes);
        if bytes.len() > frame_limit {
            return Err(network(
                "retained record exceeds negotiated frame limit; hub must raise limit",
            ));
        }
        // The credit bounds bytes in flight, not the size of one record. Journal
        // admission accepts records up to the frame limit, and nothing may skip the
        // head of replay, so a lone oversized record is sent once nothing else is
        // outstanding rather than wedging the session.
        let fits = window.outstanding == 0 || window.outstanding + bytes.len() <= credit;
        Ok(fits.then_some((next.sequence, bytes)))
    }

    fn control(body: spoke_frame::Body) -> Vec<u8> {
        v1::SpokeFrame { body: Some(body) }.encode_to_vec()
    }
    async fn send(tx: &mpsc::Sender<Vec<u8>>, bytes: Vec<u8>, limit: usize) -> Result<()> {
        ensure!(bytes.len() <= limit, "outbound frame exceeds negotiated limit");
        tokio::time::timeout(DEADLINE, tx.send(bytes))
            .await
            .map_err(network)?
            .map_err(network)?;
        Ok(())
    }
    async fn receive(stream: &mut Streaming<Received>) -> Result<Received> {
        tokio::time::timeout(DEADLINE, stream.message())
            .await
            .map_err(network)?
            .map_err(status)?
            .ok_or_else(|| network("hub closed handshake"))
    }
}

use anyhow::{Result, ensure};
use boss_protocol_spoke::{
    negotiate::{Contracts, EPOCH},
    v1::{Feature, Hello, Stamp},
};
use boss_spoke_journal::Journal;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tonic::transport::Endpoint;
use uuid::Uuid;

pub type SharedJournal = Arc<Mutex<Journal>>;
pub use session::connect_once;

pub fn contracts() -> Contracts {
    Contracts {
        min_epoch: EPOCH,
        max_epoch: EPOCH,
        features: vec![Feature {
            name: "journal".into(),
            revision: 1,
        }],
        required: vec!["journal".into()],
    }
}

pub fn hello(journal: &Journal, boot_id: &str, nonce: &[u8]) -> Result<Hello> {
    let (host_id, journal_id) = journal.identity()?;
    let (first_retained, last_record) = journal.record_bounds()?;
    Ok(Hello {
        host_id,
        journal_id,
        boot_id: boot_id.into(),
        min_epoch: EPOCH,
        max_epoch: EPOCH,
        features: contracts().features,
        build: env!("CARGO_PKG_VERSION").into(),
        received_commands: journal.received_commands()?,
        first_retained,
        last_record,
        instance_nonce: nonce.to_vec(),
        inventory: None,
    })
}

#[derive(Debug, thiserror::Error)]
#[error("spoke transport unavailable: {0}")]
struct Retry(String);

pub async fn run(journal: SharedJournal, endpoint: Endpoint, stamp: impl Fn() -> Result<Stamp>) -> Result<()> {
    run_with_executor(journal, endpoint, stamp, false).await
}

pub async fn run_with_executor(
    journal: SharedJournal,
    endpoint: Endpoint,
    stamp: impl Fn() -> Result<Stamp>,
    external_executor: bool,
) -> Result<()> {
    ensure!(
        endpoint.uri().scheme_str() == Some("https"),
        "spoke requires mTLS HTTPS"
    );
    let nonce = Uuid::new_v4().as_bytes().to_vec();
    let mut attempt = 0u32;
    loop {
        let started = std::time::Instant::now();
        if let Err(error) =
            session::connect_with_executor(&journal, endpoint.clone(), &nonce, &stamp, external_executor).await
        {
            if error.downcast_ref::<Retry>().is_none() {
                return Err(error);
            }
            tracing::warn!(error = %error, "spoke disconnected; retaining journal");
        }
        if started.elapsed() >= Duration::from_secs(60) {
            attempt = 0;
        }
        let delay = boss_backoff::equal_jitter(boss_backoff::exponential_delay(
            Duration::from_millis(250),
            Duration::from_secs(30),
            attempt,
        ));
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(delay).await;
    }
}

pub(crate) fn locked(journal: &SharedJournal) -> Result<std::sync::MutexGuard<'_, Journal>> {
    journal.lock().map_err(|_| anyhow::anyhow!("journal owner panicked"))
}

pub(crate) fn network(error: impl std::fmt::Display) -> anyhow::Error {
    Retry(error.to_string()).into()
}

pub(crate) fn status(error: tonic::Status) -> anyhow::Error {
    match error.code() {
        // ResourceExhausted is the hub shedding load (connection, host or quota
        // limits), which passes; identity, protocol and replay failures do not.
        tonic::Code::Unavailable
        | tonic::Code::Cancelled
        | tonic::Code::DeadlineExceeded
        | tonic::Code::Unknown
        | tonic::Code::ResourceExhausted
        // Admission races the hub expects a contender to retry: a changed
        // connection ("reconnect") and a pending host challenge. A genuine duplicate
        // daemon is refused on every attempt and never displaces the live stream.
        | tonic::Code::Aborted
        | tonic::Code::AlreadyExists => network(error),
        _ => error.into(),
    }
}
