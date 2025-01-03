use crate::{peer::ConnectionInfo, piece::BLOCK_SIZE};

use std::fmt;

use crate::piece::Piece;
use crate::utils::slice_to_u32_msb;
use std::fmt::{Display, Formatter};

use bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

const MAX_FRAME_SIZE: usize = 1 << 16;

pub struct RequestPayload {
    piece_id: u32,
    block_id: u32,
    offset: u32,
    length: u32,
}

pub struct PiecePayload<'a> {
    piece_id: u32,
    block_id: u32,
    offset: u32,
    data: &'a [u8],
}

#[derive(Debug, PartialEq, Clone)]
pub enum MessageType {
    KeepAlive,
    Choke,
    UnChoke,
    Interested,
    NotInterested,
    Have,
    Bitfield,
    Request,
    Piece,
    Cancel,
    Port,
}

impl RequestPayload {
    pub fn new(index: u32, offset: u32, length: u32) -> Self {
        let block_id = ((offset as usize / BLOCK_SIZE as usize) as f32).floor() as u32;
        Self {
            piece_id: index,
            offset,
            length,
            block_id,
        }
    }
}

impl fmt::Display for RequestPayload {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "request_payload: piece index: {:3}, offset: {:6}, length: {:5}",
            self.piece_id, self.offset, self.length
        )
    }
}

impl fmt::Display for PiecePayload<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "piece_payload: piece_index = {}, block_index = {} (offset =  {})",
            self.piece_id, self.block_id, self.offset
        )
    }
}

impl<'a> PiecePayload<'a> {
    fn new(index: u32, offset: u32, data: &'a [u8]) -> Self {
        let block_id = offset / BLOCK_SIZE;

        Self {
            piece_id: index,
            offset,
            data,
            block_id,
        }
    }
}

pub struct BitTorrentCodec;

impl Encoder for BitTorrentCodec {
    fn encode(&mut self, msg: Message, buffer: &mut BytesMut) {
        if msg.len > MAX_FRAME_SIZE - 4 {}
    }
}

impl Decoder for BitTorrentCodec {
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 4 {
            return Ok(None);
        }

        let len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&src[0..4]);
    }
}

impl MessageType {
    pub fn from_id(msg_id: Option<u8>) -> MessageType {
        if let Some(msg_id) = msg_id {
            match msg_id {
                0 => Self::Choke,
                1 => Self::UnChoke,
                2 => Self::Interested,
                3 => Self::NotInterested,
                4 => Self::Have,
                5 => Self::Bitfield,
                6 => Self::Request,
                7 => Self::Piece,
                8 => Self::Cancel,
                9 => Self::Port,
                _ => Self::KeepAlive, // TODO
            }
        } else {
            MessageType::KeepAlive
        }
    }

    pub fn to_id(&self) -> u8 {
        match self {
            Self::Choke => 0,
            Self::UnChoke => 1,
            Self::Interested => 2,
            Self::NotInterested => 3,
            Self::Have => 4,
            Self::Bitfield => 5,
            Self::Request => 6,
            Self::Piece => 7,
            Self::Cancel => 8,
            Self::Port => 9,
            Self::KeepAlive => 10,
        }
    }

    pub fn build_msg(self, payload: Vec<u8>) -> Message {
        Message {
            msg_type: self,
            payload,
        }
    }
}

#[derive(Clone)]
pub struct Message {
    pub len: u32,
    pub msg_type: MessageType,
    pub payload: Vec<u8>,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("msg_type", &self.msg_type)
            .finish()
    }
}

impl Message {
    pub fn parse(buf: &[u8]) -> Self {
        let msg_length = slice_to_u32_msb(&buf[0..4]);
        let msg_id = if msg_length == 0 { None } else { Some(buf[4]) };
        let msg_type = MessageType::from_id(msg_id);

        let payload_size = msg_length - 1;
        let payload = buf
            .get(5..(5 + payload_size as usize))
            .map(|b| b.to_owned())
            .unwrap_or_default();

        Message { msg_type, payload }
    }

    pub fn serialize(self) -> Vec<u8> {
        let msg_length = (self.payload.len() + 1) as u32;
        let msg_length = msg_length.to_be_bytes();
        let msg_id = self.msg_type.to_id();

        let mut msg_buf: Vec<u8> = vec![];
        msg_buf.extend(msg_length);
        msg_buf.push(msg_id);
        msg_buf.extend(self.payload);

        msg_buf
    }
}

#[derive(Debug)]
pub struct InternalMessage {
    pub msg: Message,
    pub conn_info: ConnectionInfo,
    pub should_close: bool,
}
