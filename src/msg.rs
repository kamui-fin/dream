use crate::{peer::ConnectionInfo, piece::BLOCK_SIZE};

use std::fmt;

use crate::piece::Piece;
use crate::utils::slice_to_u32_msb;
use std::fmt::{Display, Formatter};

use anyhow::anyhow;
use bytes::{Buf, BufMut, BytesMut};
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

impl Encoder<Message> for BitTorrentCodec {
    type Error = anyhow::Error;

    fn encode(&mut self, msg: Message, buffer: &mut BytesMut) -> Result<(), anyhow::Error> {
        if msg.len > (MAX_FRAME_SIZE - 4).try_into().unwrap() {
            return Err(anyhow!("Message isn't long enough".to_string()));
        }

        buffer.reserve((4 + msg.len).try_into().unwrap());

        let len_bytes = u32::to_be_bytes(msg.len);
        buffer.extend_from_slice(&len_bytes);

        buffer.put_u8(msg.msg_type.to_id());
        if msg.payload.len() > 0 {
            buffer.extend_from_slice(&msg.payload);
        }

        Ok(())
    }
}

impl Decoder for BitTorrentCodec {
    type Item = Message;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, anyhow::Error> {
        if src.len() < 4 {
            return Ok(None);
        }

        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&src[0..4]);
        let length = u32::from_be_bytes(len_bytes) as usize;

        if length == 0 {
            // keep alive
            src.advance(4);
            return Ok(Some(Message::new(None, vec![])));
        }

        if src.len() < 5 {
            return Ok(None);
        }

        if length > MAX_FRAME_SIZE - 4 {
            return Err(anyhow!("Length is way too large"));
        }

        // Note: we're doing length + 4 because length is only the length of msg payload + 1 byte for id
        if src.len() < length + 4 {
            // need to wait for more packets before we get full payload
            src.reserve(length + 4 - src.len());
            return Ok(None);
        }

        let id = src[4].try_into()?;
        let payload = if length > 1 {
            src[5..4 + length].to_vec()
        } else {
            vec![]
        };

        src.advance(4 + length);
        let msg = Message::new(id, payload);
        Ok(Some(msg))
    }
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
        let len = (payload.len() as u32) + 1;
        Message {
            msg_type: self,
            payload,
            len,
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
    pub fn new(id: Option<u8>, payload: Vec<u8>) -> Self {
        Self {
            msg_type: MessageType::from_id(id),
            len: 1 + payload.len() as u32,
            payload,
        }
    }
}

#[derive(Debug)]
pub struct InternalMessage {
    pub msg: Message,
    pub conn_info: ConnectionInfo,
    pub should_close: bool,
}
