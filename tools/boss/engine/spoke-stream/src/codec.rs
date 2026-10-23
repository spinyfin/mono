//! Preserve the original protobuf frame alongside its decoded view. The normal
//! generated server's prost codec discards unknown fields before the sink sees
//! them. The generated client and the wire schema are unchanged.
use boss_protocol_spoke::v1::{HubFrame, SpokeFrame};
use prost::{Message, bytes::Buf};
use tonic::{
    Status,
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
};

pub(crate) struct RawFrame {
    pub raw: Vec<u8>,
    pub frame: SpokeFrame,
}

pub(crate) enum Outbound {
    Frame(Box<HubFrame>),
    Command { generation: u64, bytes: Vec<u8> },
}

// The length-delimited command field has the same wire representation whether
// prost sees a message or bytes. Replay must not decode/re-encode persisted
// commands and discard optional fields written by a newer hub build.
#[derive(Message)]
struct RawCommandFrame {
    #[prost(bytes = "vec", tag = "3")]
    command: Vec<u8>,
    #[prost(uint64, tag = "6")]
    connection_generation: u64,
}

pub(crate) struct WireCodec;
pub(crate) struct WireDecoder;
pub(crate) struct WireEncoder;

impl Codec for WireCodec {
    type Encode = Outbound;
    type Decode = RawFrame;
    type Encoder = WireEncoder;
    type Decoder = WireDecoder;
    fn encoder(&mut self) -> Self::Encoder {
        WireEncoder
    }
    fn decoder(&mut self) -> Self::Decoder {
        WireDecoder
    }
}
impl Decoder for WireDecoder {
    type Item = RawFrame;
    type Error = Status;
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Status> {
        let raw = src.copy_to_bytes(src.remaining()).to_vec();
        let frame = SpokeFrame::decode(raw.as_slice()).map_err(|_| Status::invalid_argument("invalid protobuf"))?;
        Ok(Some(RawFrame { raw, frame }))
    }
}
impl Encoder for WireEncoder {
    type Item = Outbound;
    type Error = Status;
    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        let result = match item {
            Outbound::Frame(frame) => frame.encode(dst),
            Outbound::Command { generation, bytes } => RawCommandFrame {
                command: bytes,
                connection_generation: generation,
            }
            .encode(dst),
        };
        result.map_err(|_| Status::internal("frame encoding failed"))
    }
}
