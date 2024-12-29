use crate::peer::ConnectionInfo;
use crate::peer::RemotePeer;
use crate::utils::slice_to_u32_msb;

#[derive(Debug)]
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
        Message {
            msg_type: self,
            payload,
        }
    }
}

#[derive(Debug)]
pub struct Message {
    pub msg_type: MessageType,
    pub payload: Vec<u8>,
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
pub struct InternalMessage(pub Message, pub ConnectionInfo);
