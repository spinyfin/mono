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
mod session;

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
    ensure!(
        endpoint.uri().scheme_str() == Some("https"),
        "spoke requires mTLS HTTPS"
    );
    let nonce = Uuid::new_v4().as_bytes().to_vec();
    let mut attempt = 0u32;
    loop {
        let started = std::time::Instant::now();
        if let Err(error) = connect_once(&journal, endpoint.clone(), &nonce, &stamp).await {
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
        | tonic::Code::ResourceExhausted => network(error),
        _ => error.into(),
    }
}
