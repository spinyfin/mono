//! Preserve original nested protobuf bytes across journal replay. The generated
//! client would decode/re-encode them and discard unknown fields.
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
