use std::{
    fmt::{self, Formatter},
    io::Cursor,
    path::PathBuf,
};

use anyhow::anyhow;
use bytes::{Buf, BufMut, BytesMut};
use log::trace;
use tokio_util::codec::{Decoder, Encoder};

use crate::{config::CONFIG, peer::session::ConnectionInfo};

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
        let block_id =
            ((offset as usize / CONFIG.torrent.block_size as usize) as f32).floor() as u32;
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
        let block_id = offset / CONFIG.torrent.block_size;

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
        if !msg.payload.is_empty() {
            buffer.extend_from_slice(&msg.payload);
        }

        Ok(())
    }
}

impl Decoder for BitTorrentCodec {
    type Item = Message;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, anyhow::Error> {
        trace!("Decoder has {} byte(s) remaining", src.remaining());
        if src.remaining() < 4 {
            return Ok(None);
        }

        let mut tmp_buf = Cursor::new(&src);
        let length = tmp_buf.get_u32() as usize;

        tmp_buf.set_position(0);

        if src.remaining() >= 4 + length {
            src.advance(4);

            if length == 0 {
                return Ok(Some(Message::new(None, vec![])));
            }
        } else {
            return Ok(None);
        }

        let id = src.get_u8();
        let payload = if length > 1 {
            let mut data = vec![0u8; length - 1];
            src.copy_to_slice(&mut data);
            data
        } else {
            vec![]
        };
        let msg = Message::new(Some(id), payload);
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

    pub fn build_empty(self) -> Message {
        self.build_msg(vec![])
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

#[derive(Debug, Clone)]
pub enum InternalMessagePayload {
    CloseConnection,
    ForwardMessage { msg: Message },
    MigrateWork,
    UpdateDownloadSpeed { speed: f32 },
    UpdateUploadSpeed { speed: f32 },
}

#[derive(Clone, Debug)]
pub struct InternalMessage {
    pub payload: InternalMessagePayload,
    pub origin: ConnectionInfo,
}

#[derive(Debug)]
pub enum ServerMsg {
    // Requests
    AddExternalTorrent {
        input_data: Vec<u8>,
        output_dir: PathBuf,
        response_tx: tokio::sync::oneshot::Sender<(u64, String)>,
    },
    // Responses
    StreamRequestRange {
        start: u64,
        end: u64,
        info_hash: [u8; 20],
        response_tx: tokio::sync::mpsc::Sender<DataReady>,
    },
}

#[derive(Debug)]
pub struct DataReady {
    pub has_more: bool,
    pub data: Vec<u8>,
}
