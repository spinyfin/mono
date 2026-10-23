use anyhow::{Result, bail, ensure};
use boss_protocol_spoke::{
    Validate,
    limits::{MAX_FRAME_BYTES, MAX_INFLIGHT_BYTES},
    v1::{self, hub_frame, spoke_frame},
};
use prost::Message;
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

/// One bounded session, also used by the real tonic fault tests. Outbound records
/// have one outstanding receipt at a time; transport flow control is not receipt.
pub async fn connect_once(
    journal: &SharedJournal,
    endpoint: Endpoint,
    nonce: &[u8],
    stamp: &impl Fn() -> Result<v1::Stamp>,
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
    let hello = hello(&*locked(journal)?, &stamp()?.boot_id, nonce)?;
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
        Some(hub_frame::Body::Reject(reject)) => bail!("hub rejected spoke: {}: {}", reject.code, reject.diagnostic),
        _ => bail!("expected Welcome as first hub frame"),
    };
    contracts().confirm(&welcome)?;
    ensure!(
        first.frame.connection_generation == welcome.connection_generation,
        "Welcome generation mismatch"
    );
    locked(journal)?.welcome(&welcome)?;
    let frame_limit = downward(welcome.max_frame_bytes, MAX_FRAME_BYTES);
    let credit = downward(welcome.max_inflight_bytes, MAX_INFLIGHT_BYTES);
    let mut acknowledged = welcome.received_records.as_ref().expect("validated receipt").through;
    let mut sent = acknowledged;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut last_read = tokio::time::Instant::now();
    loop {
        tokio::select! {
            message = inbound.message() => {
                let message = message.map_err(status)?.ok_or_else(|| network("hub closed stream"))?;
                ensure!(message.size <= frame_limit, "hub exceeded negotiated frame limit");
                ensure!(message.frame.connection_generation == welcome.connection_generation, "stale stream generation");
                message.frame.validate()?;
                last_read = tokio::time::Instant::now();
                match message.frame.body.expect("validated body") {
                    hub_frame::Body::RecordReceipt(cursor) => {
                        ensure!(cursor.through >= acknowledged && cursor.through <= sent, "receipt outside sent prefix");
                        locked(journal)?.receipt(&cursor)?;
                        acknowledged = cursor.through;
                    }
                    hub_frame::Body::Command(_) => {
                        let cursor = {
                            let mut journal = locked(journal)?;
                            journal.receive_command(&message.command.expect("decoded command bytes"))?;
                            journal.received_commands()?.expect("bound command journal")
                        };
                        send(&tx, control(spoke_frame::Body::CommandReceipt(cursor)), frame_limit).await?;
                    }
                    hub_frame::Body::Pulse(mut pulse) => {
                        pulse.stamp = Some(stamp()?);
                        send(&tx, control(spoke_frame::Body::PulseReply(pulse)), frame_limit).await?;
                    }
                    _ => bail!("unexpected handshake inside established stream"),
                }
            }
            _ = tick.tick() => {
                if last_read.elapsed() > DEADLINE { return Err(network("hub read deadline expired")); }
                // Recovery runs even if the receipt's network write was lost.
                locked(journal)?.reject_pending(&stamp()?, &hello)?;
                if sent == acknowledged {
                    let next = locked(journal)?.next_record(sent)?;
                    if let Some(next) = next {
                        let bytes = record_frame(next.bytes);
                        ensure!(bytes.len() <= credit, "record exceeds negotiated send credit");
                        send(&tx, bytes, frame_limit).await?;
                        sent = next.sequence;
                    }
                }
            }
        }
    }
}

fn downward(peer: u32, local: u32) -> usize {
    (if peer == 0 { local } else { local.min(peer) }) as usize
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
