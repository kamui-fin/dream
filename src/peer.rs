use anyhow::Context;
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ByteOrder};
use futures::future;
use futures::StreamExt;
use lazy_static::lazy_static;
use log::trace;
use log::warn;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde::{Deserialize, Deserializer};
use serde_bytes::ByteBuf;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::{collections::VecDeque, net::SocketAddr, ops::Range, sync::Arc, time::Duration};
use tokio::sync::Mutex;

use log::{error, info};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::Sender,
    time::timeout,
};

use crate::bittorrent::BitTorrent;
use crate::piece;
use crate::piece::PieceStore;
use crate::tracker::TrackerResponse;
use crate::{
    msg::{Message, MessageType},
    piece::{BitField, BLOCK_SIZE},
    utils::slice_to_u32_msb,
};

lazy_static! {
    pub static ref DREAM_ID: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(20)
        .map(char::from)
        .collect();
}

const MAX_PIPELINE_SIZE: usize = 4;

#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    ip: Ipv4Addr,
    port: u16,
}

fn ip_to_ipv4(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V4(ipv4) => Some(ipv4),
        IpAddr::V6(_) => None,
    }
}

impl Peer {
    pub fn new(ip: Ipv4Addr, port: u16) -> Self {
        Self { ip, port }
    }

    pub fn addr(&self) -> SocketAddr {
        SocketAddr::new(std::net::IpAddr::V4(self.ip), self.port)
    }

    pub fn from_addr(addr: SocketAddr) -> Self {
        Self {
            ip: ip_to_ipv4(addr.ip()).unwrap(),
            port: addr.port(),
        }
    }
}

pub fn deserialize_peers<'de, D>(deserializer: D) -> Result<Vec<Peer>, D::Error>
where
    D: Deserializer<'de>,
{
    let bytes: ByteBuf = Deserialize::deserialize(deserializer)?;
    let mut peers = vec![];
    for curr_chunk in bytes.chunks(6) {
        if curr_chunk.len() == 6 {
            let ip = Ipv4Addr::new(curr_chunk[0], curr_chunk[1], curr_chunk[2], curr_chunk[3]);
            let port = u16::from_be_bytes([curr_chunk[4], curr_chunk[5]]);
            peers.push(Peer::new(ip, port))
        }
    }
    Ok(peers)
}

pub struct RemotePeer {
    pub peer: Peer,
    pub piece_lookup: BitField,
    pub am_choking: bool,      // = 1
    pub am_interested: bool,   // = 0
    pub peer_choking: bool,    // has this peer choked us? = 1
    pub peer_interested: bool, // is this peer interested in us? = 0
    piece_store: Arc<Mutex<PieceStore>>,

    // pertaining to connection
    conn: Arc<Mutex<TcpStream>>,
    pipeline: Vec<PipelineEntry>,
    buffer: VecDeque<PipelineEntry>,

    unchoke_tx: Sender<UnchokeMessage>,
}

impl fmt::Debug for RemotePeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemotePeer")
            .field("peer", &self.peer)
            // .field("pieces", &self.piece_lookup)
            // .field("am_interested", &self.am_interested)
            // .field("am_choking", &self.am_choking)
            // .field("peer_interested", &self.peer_interested)
            // .field("am_interested", &self.am_interested)
            // .field("queue", &self.buffer)
            .finish()
    }
}

#[derive(Clone)]
pub struct PipelineEntry {
    piece_id: u32,
    block_id: u32,
}

pub struct UnchokeMessage {
    pub peer: Peer,
}

impl RemotePeer {
    async fn from_peer(
        peer: Peer,
        piece_store: Arc<Mutex<PieceStore>>,
        info_hash: &[u8; 20],
        unchoke_channel: Sender<UnchokeMessage>,
    ) -> anyhow::Result<Self> {
        let conn = Self::peer_handshake(peer.clone(), info_hash).await?;
        let conn = Arc::new(Mutex::new(conn));
        let bitfield = Self::receive_bitfield(conn.clone()).await?;

        let pipeline = Vec::new();

        Ok(Self {
            peer,
            piece_lookup: BitField::new(0),
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            piece_store,
            unchoke_tx: unchoke_channel,
            conn,
            pipeline,
            buffer: VecDeque::new(),
        })
    }

    pub async fn peer_handshake(peer: Peer, info_hash: &[u8; 20]) -> Result<TcpStream> {
        info!("Initiating peer handshake with {:#?}", peer);

        let mut handshake = [0u8; 68];
        handshake[0] = 19;
        handshake[1..20].copy_from_slice(b"BitTorrent protocol");
        handshake[28..48].copy_from_slice(info_hash);
        handshake[48..68].copy_from_slice(DREAM_ID.as_bytes());

        let connect_timeout = Duration::from_secs(1);
        let mut stream = match timeout(connect_timeout, TcpStream::connect(peer.addr())).await {
            Ok(Ok(stream)) => {
                info!("Connection established");
                stream
            }
            Ok(Err(e)) => {
                error!("Failed to connect: {}", e);
                return Err(anyhow!(e));
            }
            Err(_) => {
                error!("Connection attempt timed out");
                return Err(anyhow!("Connection timed out"));
            }
        };

        stream.write_all(&handshake).await?;

        let mut res = [0u8; 68];
        stream.read_exact(&mut res).await?;

        if res[0..20] == handshake[0..20] && res[28..48] == handshake[28..48] {
            trace!("Handshake successful with peer: {:#?}", peer);
            Ok(stream)
        } else {
            stream.shutdown().await?;
            error!("Handshake mismatch");
            Err(anyhow!("Handshake failed"))
        }
    }

    pub async fn send_message(&self, msg: Message) {
        trace!("Sending message \"{:#?}\" to peer: {:#?}", self.peer, msg);
        self.conn.lock().await.write_all(&msg.serialize()).await;
    }

    pub async fn handle_msg(&mut self, bt_msg: &Message) {
        match bt_msg.msg_type {
            MessageType::KeepAlive => {
                // close connection after 2 min of inactivity (no commands)
                // keepalive is just a dummy msg to reset that timer
            }
            MessageType::Choke => {
                // peer has choked us
                self.peer_choking = true;
                info!("Peer {:#?} has choked us", self.peer);
            }
            MessageType::UnChoke => {
                // peer has unchoked us
                self.unchoke_us().await;
                info!("Peer {:#?} has unchoked us", self.peer);
            }
            MessageType::Interested => {
                // peer is interested in us
                self.peer_interested = true;
                info!("Peer {:#?} is interested in us", self.peer);
            }
            MessageType::NotInterested => {
                // peer is not interested in us
                self.peer_interested = false;
                info!("Peer {:#?} is uninterested in us", self.peer);
            }
            MessageType::Have => {
                // peer has piece <piece_index>
                // sent after piece is downloaded and verified
                let piece_index = slice_to_u32_msb(&bt_msg.payload[0..4]);
                self.piece_lookup.mark_piece(piece_index);

                info!(
                    "Peer {:#?} has confirmed that they have piece with piece-index {}",
                    self.peer, piece_index
                );
            }
            MessageType::Bitfield => {
                // info about which pieces peer has
                // only sent right after handshake, and before any other msg (so optional)
                self.piece_lookup = BitField(bt_msg.payload.clone());
                info!(
                    "Peer {:#?} has given us its bitfield: {:#?}",
                    self.peer, self.piece_lookup
                );
            }
            MessageType::Request => {
                // requests a piece - (index, begin byte offset, length)
                let piece_idx = slice_to_u32_msb(&bt_msg.payload[0..4]);
                info!(
                    "Peer {:#?} has requested a piece with index {}",
                    self.peer, piece_idx
                );

                if !self.am_choking && self.piece_lookup.piece_exists(piece_idx) {
                    let byte_offset = slice_to_u32_msb(&bt_msg.payload[4..8]);
                    let length = slice_to_u32_msb(&bt_msg.payload[8..12]);

                    let mut piece_store_guard = self.piece_store.lock().await;
                    let target_block = match piece_store_guard.pieces[piece_idx as usize]
                        .retrieve_block(byte_offset as usize, length as usize)
                    {
                        Some(block) => block,
                        None => {
                            error!(
                                "Piece Retrieval failed for blockoffset {} of piece {}",
                                byte_offset, piece_idx
                            );
                            return;
                        }
                    };

                    info!(
                        "Piece {} with Block offset {} retrieved",
                        piece_idx, byte_offset
                    );
                    let mut pay_load = Vec::with_capacity(target_block.len() + 8);

                    pay_load.extend_from_slice(&piece_idx.to_be_bytes());
                    pay_load.extend_from_slice(&byte_offset.to_be_bytes());
                    pay_load.extend_from_slice(&target_block);

                    info!("Payload in response to request ready to send");

                    self.send_message(MessageType::Piece.build_msg(pay_load));
                } else if self.am_choking {
                    info!("Currently choking peer {:#?} so we cannot fulfill its request of piece with index {}", self.peer, piece_idx);
                } else {
                    info!(
                        "Do not have the piece with index {} that peer {:#?} has requested",
                        piece_idx, self.peer
                    );
                }
            }
            MessageType::Piece => {
                // in response to Request, returns piece data
                // index, begin, block data
                let piece_idx = slice_to_u32_msb(&bt_msg.payload[0..4]);
                let begin_offset = slice_to_u32_msb(&bt_msg.payload[4..8]);
                let block_data = &bt_msg.payload[8..];

                info!(
                    "Peer {:#?} has sent us piece {} starting at offset {}",
                    self.peer, piece_idx, begin_offset
                );

                self.piece_store.lock().await.pieces[piece_idx as usize].store_block(
                    begin_offset as usize,
                    block_data.len(),
                    block_data,
                );
            }
            MessageType::Cancel => {
                // informing us that block <index><begin><length> is not needed anymore
                // for endgame algo
                todo!();
            }
            MessageType::Port => {
                // port that their dht node is listening on
                // only for DHT extension
                todo!();
            }
            _ => {
                warn!("Invalid message from peer {:#?}", self.peer);
            }
        }
    }

    pub async fn flush_pipeline(&mut self) {
        trace!("Begginning flush of peer {:#?}'s pipeline", self.peer);
        while !self.pipeline.is_empty() || !self.buffer.is_empty() {
            let msg = self.receive_message().await;
            if let Some(msg) = msg {
                if let MessageType::Piece = msg.msg_type {
                    let piece_idx = slice_to_u32_msb(&msg.payload[0..4]);
                    let block_offset = slice_to_u32_msb(&msg.payload[4..8]);
                    let block_id =
                        ((block_offset as usize / &msg.payload.len() - 8) as f32).floor() as u32;

                    self.pipeline
                        .retain(|p| p.block_id != block_id && p.piece_id != piece_idx);

                    info!(
                        "Block {} from Piece {} removed from peer {:#?}'s pipeline",
                        block_id, piece_idx, self.peer
                    );
                    if let Some(entry) = self.buffer.pop_front() {
                        self.pipeline_enqueue(entry);
                    }
                }
            }
        }
        info!("Pipeline of peer {:#?} flushed", self.peer);
    }

    pub async fn receive_message(&mut self) -> Option<Message> {
        let mut conn = self.conn.lock().await;
        let mut len_buf = [0u8; 4];
        conn.read_exact(&mut len_buf).await.ok();

        let msg_length = slice_to_u32_msb(&len_buf);

        if msg_length > 0 {
            let mut id_buf = [0u8; 1];
            conn.read_exact(&mut id_buf).await.ok()?;

            let mut payload_buf = vec![0u8; msg_length as usize];
            conn.read_exact(&mut payload_buf).await.ok()?;

            let return_msg = MessageType::from_id(Some(id_buf[0])).build_msg(payload_buf);
            drop(conn);
            self.handle_msg(&return_msg).await;

            info!("Received msg {:#?} from peer {:#?}", return_msg, &self);

            return Some(return_msg);
        } else {
            // get rid of the guard if it's useless so you don't deadlock
            warn!("Message from peer {:#?} has no data", self.peer);
            drop(conn);
            None
        }
    }

    async fn request_block(&mut self, entry: PipelineEntry) {
        let PipelineEntry { piece_id, block_id } = entry;
        let piece_id_bytes = piece_id.to_be_bytes();
        let block_id_bytes = block_id.to_be_bytes();
        let block_size = BLOCK_SIZE.to_be_bytes();

        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&piece_id_bytes);
        payload.extend_from_slice(&block_id_bytes);
        payload.extend_from_slice(&block_size);

        info!(
            "Sent a request for block {} of piece {}",
            block_id, piece_id
        );
        let msg = MessageType::Request.build_msg(payload);
        self.send_message(msg).await;
    }

    pub async fn queue_blocks(&mut self, piece_id: u32, blocks: Range<u32>) {
        info!("Queuing blocks {:#?} for {piece_id}", blocks);
        for block_id in blocks {
            self.buffer.push_back(PipelineEntry { piece_id, block_id });
        }

        self.refresh_pipeline().await;
    }

    async fn refresh_pipeline(&mut self) {
        for _ in 0..(MAX_PIPELINE_SIZE - self.pipeline.len()) {
            if let Some(entry) = self.buffer.pop_front() {
                self.pipeline_enqueue(entry).await;
            }
        }
    }

    async fn pipeline_enqueue(&mut self, entry: PipelineEntry) {
        self.pipeline.push(entry.clone());

        self.request_block(entry).await;
    }

    pub async fn unchoke_us(&mut self) {
        self.peer_choking = false;
        self.unchoke_tx
            .send(UnchokeMessage {
                peer: self.peer.clone(),
            })
            .await;
    }
}

pub struct PeerManager {
    pub swarm: Vec<RemotePeer>,
}

impl PeerManager {
    // Initializes all the peers from the tracker response
    pub async fn connect_peers(
        peers_response: TrackerResponse,
        piece_store: Arc<Mutex<PieceStore>>,
        info_hash: &[u8; 20],
        unchoke_channel: &Sender<UnchokeMessage>,
    ) -> PeerManager {
        info!(
            "Attempting to connect to {} peers",
            peers_response.peers.len()
        );
        let swarm: Vec<_> = peers_response
            .peers
            .into_iter()
            .map(|p| {
                RemotePeer::from_peer(p, piece_store.clone(), info_hash, unchoke_channel.clone())
            })
            .collect();
        let swarm = future::join_all(swarm).await;
        info!("{:#?}", swarm);
        let swarm: Vec<RemotePeer> = swarm.into_iter().filter_map(Result::ok).collect();

        Self { swarm }
    }

    pub fn with_piece(&self, piece_idx: u32) -> Vec<&RemotePeer> {
        self.swarm
            .iter()
            .filter(|p| p.piece_lookup.piece_exists(piece_idx))
            .collect()
    }

    pub async fn find_or_create(
        &mut self,
        addr: SocketAddr,
        piece_store: Arc<Mutex<PieceStore>>,
        info_hash: &[u8; 20],
        unchoke_channel: Sender<UnchokeMessage>,
    ) {
        if !self.swarm.iter().any(|p| p.peer.addr() == addr) {
            let new_peer = RemotePeer::from_peer(
                Peer::from_addr(addr),
                piece_store,
                info_hash,
                unchoke_channel,
            )
            .await;
            if let Ok(peer) = new_peer {
                self.swarm.push(peer); // will be at len - 1
            }
        }
    }

    pub fn find_peer(&self, peer: &Peer) -> Option<&RemotePeer> {
        self.swarm.iter().find(|p| p.peer == *peer)
    }

    pub fn find_peer_mut(&mut self, peer: &Peer) -> Option<&mut RemotePeer> {
        self.swarm.iter_mut().find(|p| p.peer == *peer)
    }

    pub async fn queue_blocks_for_peer(
        &mut self,
        peer: &Peer,
        piece_idx: u32,
        num_blocks: Range<u32>,
    ) {
        let peer = self.find_peer_mut(peer);
        if let Some(peer) = peer {
            peer.queue_blocks(piece_idx, num_blocks).await;
        }
    }

    pub fn peer_has_piece(&self, peer: &Peer, piece_idx: u32) -> bool {
        if let Some(peer) = self.find_peer(peer) {
            peer.piece_lookup.piece_exists(piece_idx)
        } else {
            false
        }
    }

    pub async fn flush_pipeline(&mut self) {
        for peer in self.swarm.iter_mut() {
            peer.flush_pipeline().await;
        }
    }
}
