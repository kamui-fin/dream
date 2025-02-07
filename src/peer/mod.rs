use std::fmt;

use lazy_static::lazy_static;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use session::ConnectionInfo;

use crate::piece::BitField;

pub mod manager;
pub mod session;
pub mod stats;

const HANDSHAKE_LEN: usize = 68;
const PROTOCOL_STR_LEN: usize = 19;

lazy_static! {
    pub static ref DREAM_ID: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(20)
        .map(char::from)
        .collect();
}

pub struct RemotePeer {
    pub conn_info: ConnectionInfo,
    pub piece_lookup: BitField,
    pub am_choking: bool,      // = 1
    pub am_interested: bool,   // = 0
    pub peer_choking: bool,    // has this peer choked us? = 1
    pub peer_interested: bool, // is this peer interested in us? = 0
    pub optimistic_unchoke: bool,
    pub pipeline: Vec<PipelineEntry>,
}

impl fmt::Debug for RemotePeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemotePeer")
            .field("peer", &self.conn_info)
            .field("pieces", &self.piece_lookup)
            .field("am_interested", &self.am_interested)
            .field("am_choking", &self.am_choking)
            .field("peer_interested", &self.peer_interested)
            .field("am_interested", &self.am_interested)
            .finish()
    }
}

impl RemotePeer {
    fn from_peer(peer: ConnectionInfo, num_pieces: u32) -> Self {
        Self {
            conn_info: peer,
            piece_lookup: BitField::new(num_pieces),
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            pipeline: Vec::new(),
            optimistic_unchoke: false,
        }
    }
}

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct PipelineEntry {
    piece_id: u32,
    block_id: u32,
}
