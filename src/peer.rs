use anyhow::Context;
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ByteOrder};
use futures::future::{self, join_all, Remote};
use futures::StreamExt;
use http_req::tls::Conn;
use lazy_static::lazy_static;
use log::trace;
use log::warn;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde::{Deserialize, Deserializer};
use serde_bytes::ByteBuf;
use sha1::digest::typenum::Bit;
use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr};
use std::{collections::VecDeque, net::SocketAddr, ops::Range, sync::Arc, time::Duration};
use tokio::sync::mpsc::Receiver;
use tokio::sync::{mpsc, Mutex, Notify};

use log::{error, info};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::Sender,
    time::timeout,
};

use crate::bittorrent::BitTorrent;
use crate::msg::InternalMessage;
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConnectionInfo {
    ip: Ipv4Addr,
    port: u16,
}

fn ip_to_ipv4(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V4(ipv4) => Some(ipv4),
        IpAddr::V6(_) => None,
    }
}

impl ConnectionInfo {
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

pub fn deserialize_peers<'de, D>(deserializer: D) -> Result<Vec<ConnectionInfo>, D::Error>
where
    D: Deserializer<'de>,
{
    let bytes: ByteBuf = Deserialize::deserialize(deserializer)?;
    let mut peers = vec![];
    for curr_chunk in bytes.chunks(6) {
        if curr_chunk.len() == 6 {
            let ip = Ipv4Addr::new(curr_chunk[0], curr_chunk[1], curr_chunk[2], curr_chunk[3]);
            let port = u16::from_be_bytes([curr_chunk[4], curr_chunk[5]]);
            peers.push(ConnectionInfo::new(ip, port))
        }
    }
    Ok(peers)
}

struct PeerSession {
    // pertaining to connection
    conn: Arc<Mutex<TcpStream>>,
    sender: mpsc::Sender<InternalMessage>,
    recv_msg: Arc<Mutex<mpsc::Receiver<Message>>>,
    peer: ConnectionInfo,
}

impl PeerSession {
    async fn new_session(
        sender: mpsc::Sender<InternalMessage>,
        recv_msg: mpsc::Receiver<Message>,
        peer: ConnectionInfo,
        info_hash: &[u8; 20],
    ) -> anyhow::Result<Self> {
        let conn = Self::peer_handshake(peer.clone(), info_hash).await?;
        let conn = Arc::new(Mutex::new(conn));
        Ok(Self {
            conn,
            sender,
            peer,
            recv_msg: Arc::new(Mutex::new(recv_msg)),
        })
    }

    pub async fn peer_handshake(peer: ConnectionInfo, info_hash: &[u8; 20]) -> Result<TcpStream> {
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

    pub async fn start_listening(&mut self) {
        loop {
            let mut recv_msg_lock = self.recv_msg.lock().await;
            tokio::select! {
                Some(msg) = self.receive_message() => {
                    let msg = InternalMessage(msg, self.peer.clone());
                    info!("Sending msg {:#?}", msg);
                    self.sender.try_send(msg).unwrap();
                }
                Some(msg) = recv_msg_lock.recv() => {
                    self.send_message(msg).await;
                }
            }
        }
    }

    pub async fn send_message(&self, msg: Message) {
        self.conn
            .lock()
            .await
            .write_all(&msg.serialize())
            .await
            .unwrap();
    }

    pub async fn receive_message(&self) -> Option<Message> {
        let mut len_buf = [0u8; 4];
        let mut conn = self.conn.lock().await;

        conn.read_exact(&mut len_buf).await.ok();

        let msg_length = slice_to_u32_msb(&len_buf);

        if msg_length > 0 {
            let mut id_buf = [0u8; 1];
            conn.read_exact(&mut id_buf).await.ok()?;

            let mut payload_buf = vec![0u8; msg_length as usize];
            conn.read_exact(&mut payload_buf).await.ok()?;

            let incoming_msg = MessageType::from_id(Some(id_buf[0])).build_msg(payload_buf);

            return Some(incoming_msg);
        } else {
            Some(MessageType::KeepAlive.build_msg(vec![]))
        }
    }
}

pub struct RemotePeer {
    pub conn_info: ConnectionInfo,
    pub is_ready: Notify,
    pub piece_lookup: BitField,
    pub am_choking: bool,      // = 1
    pub am_interested: bool,   // = 0
    pub peer_choking: bool,    // has this peer choked us? = 1
    pub peer_interested: bool, // is this peer interested in us? = 0

    pipeline: Vec<PipelineEntry>,
    buffer: VecDeque<PipelineEntry>,
}

impl fmt::Debug for RemotePeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemotePeer")
            .field("peer", &self.conn_info)
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
    pub peer: ConnectionInfo,
}

impl RemotePeer {
    fn from_peer(peer: ConnectionInfo, num_pieces: u32) -> Self {
        Self {
            conn_info: peer,
            is_ready: Notify::new(),
            piece_lookup: BitField::new(num_pieces),
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            pipeline: Vec::new(),
            buffer: VecDeque::new(),
        }
    }
}

pub struct PeerManager {
    pub peers: Vec<RemotePeer>,
    piece_store: Arc<Mutex<PieceStore>>,

    // channels
    send_channels: HashMap<ConnectionInfo, Sender<Message>>,
    msg_tx: Sender<InternalMessage>, // only stored in case we want to add new peers dynamically
    unchoke_tx: Sender<UnchokeMessage>,

    pub notify_pipelines_empty: Notify,
}

impl PeerManager {
    // Initializes all the peers from the tracker response
    pub async fn connect_peers(
        peers_response: TrackerResponse,
        piece_store: Arc<Mutex<PieceStore>>,
        info_hash: &[u8; 20],
        num_pieces: u32,
        unchoke_tx: Sender<UnchokeMessage>,
    ) -> (PeerManager, Receiver<InternalMessage>) {
        info!(
            "Attempting to connect to {} peers",
            peers_response.peers.len()
        );

        // the channel for receiving messages from peer sessions
        let (msg_tx, msg_rx) = mpsc::channel(500);

        let mut send_channels: HashMap<ConnectionInfo, Sender<Message>> = HashMap::new();

        let peers: Vec<_> = peers_response
            .peers
            .into_iter()
            .map(|peer| RemotePeer::from_peer(peer, num_pieces))
            .collect();

        info!("{:#?}", peers);

        for remote_peer in &peers {
            let info_hash_clone = info_hash.clone();
            let conn_info = remote_peer.conn_info.clone();
            let tx_clone = msg_tx.clone();

            // the channel for sending messages to this peer session
            let (send_msg, recv_msg) = mpsc::channel(500);
            send_channels.insert(conn_info.clone(), send_msg);

            tokio::spawn(async move {
                let session =
                    PeerSession::new_session(tx_clone, recv_msg, conn_info, &info_hash_clone).await;
                if let Ok(mut session) = session {
                    session.start_listening().await;
                }
            });
        }

        (
            Self {
                peers,
                send_channels,
                msg_tx,
                unchoke_tx,
                piece_store,
                notify_pipelines_empty: Notify::new(),
            },
            msg_rx,
        )
    }

    pub fn check_empty_pipelines(&self) {
        if self
            .peers
            .iter()
            .all(|p| p.buffer.is_empty() && p.pipeline.is_empty())
        {
            self.notify_pipelines_empty.notify_one();
        }
    }

    pub async fn with_piece(&self, piece_idx: u32) -> Vec<ConnectionInfo> {
        let mut result = Vec::new();
        for peer in &self.peers {
            info!("Trying to lock peer");
            info!("Successfully locked peer");
            if peer.piece_lookup.piece_exists(piece_idx) {
                result.push(peer.conn_info.clone());
            }
        }
        result
    }

    pub async fn find_or_create(
        &mut self,
        addr: SocketAddr,
        num_pieces: u32,
        info_hash: [u8; 20],
    ) -> &RemotePeer {
        if !self.peers.iter().any(|p| p.conn_info.addr() == addr) {
            let new_peer = RemotePeer::from_peer(ConnectionInfo::from_addr(addr), num_pieces);
            // launch new session

            // the channel for sending messages to this peer session
            let (send_msg, recv_msg) = mpsc::channel(500);

            self.send_channels
                .insert(new_peer.conn_info.clone(), send_msg);

            let conn_info = new_peer.conn_info.clone();
            let tx_clone = self.msg_tx.clone();

            tokio::spawn(async move {
                let session =
                    PeerSession::new_session(tx_clone, recv_msg, conn_info, &info_hash).await;
                if let Ok(mut session) = session {
                    session.start_listening().await;
                }
            });

            self.peers.push(new_peer);

            self.peers.last().unwrap()
        } else {
            self.find_peer(&ConnectionInfo::from_addr(addr))
        }
    }

    pub fn find_peer(&self, peer: &ConnectionInfo) -> &RemotePeer {
        self.peers
            .iter()
            .find(|curr_peer| curr_peer.conn_info == *peer)
            .unwrap()
    }

    pub fn find_peer_mut(&mut self, peer: &ConnectionInfo) -> &mut RemotePeer {
        self.peers
            .iter_mut()
            .find(|curr_peer| curr_peer.conn_info == *peer)
            .unwrap()
    }

    pub async fn queue_blocks(
        &mut self,
        conn_info: &ConnectionInfo,
        piece_id: u32,
        blocks: Range<u32>,
    ) {
        info!("Queuing blocks {:#?} for {piece_id}", blocks);
        let peer = self.find_peer_mut(conn_info);
        for block_id in blocks {
            peer.buffer.push_back(PipelineEntry { piece_id, block_id });
        }
    }

    pub async fn peer_has_piece(&self, peer: &ConnectionInfo, piece_idx: u32) -> bool {
        let peer = self.find_peer(peer);
        peer.piece_lookup.piece_exists(piece_idx)
    }

    pub async fn show_interest_in_peer(&mut self, conn_info: &ConnectionInfo) {
        {
            let peer = self.find_peer_mut(conn_info);
            peer.am_interested = true;
        }
        self.send_message(conn_info, MessageType::Interested.build_msg(vec![]));
    }

    pub fn send_message(&mut self, conn_info: &ConnectionInfo, msg: Message) {
        self.send_channels[conn_info].send(msg);
    }

    async fn pipeline_enqueue(&mut self, entry: PipelineEntry, conn_info: &ConnectionInfo) {
        let peer = self.find_peer_mut(conn_info);
        peer.pipeline.push(entry.clone());
        self.request_block(entry, conn_info).await;
    }

    async fn request_block(&mut self, entry: PipelineEntry, conn_info: &ConnectionInfo) {
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
        self.send_channels[&conn_info.clone()]
            .send(msg)
            .await
            .unwrap();
    }

    pub async fn wait_for_peers_ready(&self) {
        let futures: Vec<_> = self.peers.iter().map(|s| s.is_ready.notified()).collect();
        join_all(futures).await;
    }

    pub async fn handle_msg(&mut self, bt_msg: &Message, conn_info: &ConnectionInfo) {
        let peer = self.find_peer_mut(conn_info);
        match bt_msg.msg_type {
            MessageType::KeepAlive => {
                // close connection after 2 min of inactivity (no commands)
                // keepalive is just a dummy msg to reset that timer
                // info!("Received keep alive")
            }
            MessageType::Choke => {
                // peer has choked us
                peer.peer_choking = true;
                info!("Peer {:#?} has choked us", peer.conn_info);
            }
            MessageType::UnChoke => {
                // peer has unchoked us
                peer.peer_choking = false;

                self.unchoke_tx
                    .send(UnchokeMessage {
                        peer: conn_info.clone(),
                    })
                    .await
                    .unwrap();

                info!("Peer {:#?} has unchoked us", conn_info);
            }
            MessageType::Interested => {
                // peer is interested in us
                peer.peer_interested = true;
                info!("Peer {:#?} is interested in us", peer.conn_info);
            }
            MessageType::NotInterested => {
                // peer is not interested in us
                peer.peer_interested = false;
                info!("Peer {:#?} is uninterested in us", peer.conn_info);
            }
            MessageType::Have => {
                // peer has piece <piece_index>
                // sent after piece is downloaded and verified
                let piece_index = slice_to_u32_msb(&bt_msg.payload[0..4]);
                peer.piece_lookup.mark_piece(piece_index);

                info!(
                    "Peer {:#?} has confirmed that they have piece with piece-index {}",
                    peer.conn_info, piece_index
                );
            }
            MessageType::Bitfield => {
                // info about which pieces peer has
                // only sent right after handshake, and before any other msg (so optional)
                peer.piece_lookup = BitField(bt_msg.payload.clone());
                peer.is_ready.notify_one();
                info!(
                    "Peer {:#?} has informed us that is has pieces {}",
                    peer.conn_info,
                    peer.piece_lookup.return_piece_indexes()
                );
            }
            MessageType::Request => {
                // requests a piece - (index, begin byte offset, length)
                let piece_idx = slice_to_u32_msb(&bt_msg.payload[0..4]);
                info!(
                    "Peer {:#?} has requested a piece with index {}",
                    peer.conn_info, piece_idx
                );

                if !peer.am_choking && peer.piece_lookup.piece_exists(piece_idx) {
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

                    self.send_channels[&conn_info.clone()]
                        .send(MessageType::Piece.build_msg(pay_load))
                        .await
                        .unwrap();
                } else if peer.am_choking {
                    info!("Currently choking peer {:#?} so we cannot fulfill its request of piece with index {}", peer.conn_info, piece_idx);
                } else {
                    info!(
                        "Do not have the piece with index {} that peer {:#?} has requested",
                        piece_idx, conn_info
                    );
                }
            }
            MessageType::Piece => {
                // in response to Request, returns piece data
                // index, begin, block data
                let piece_idx = slice_to_u32_msb(&bt_msg.payload[0..4]);
                let block_offset = slice_to_u32_msb(&bt_msg.payload[4..8]);
                let block_data = &bt_msg.payload[8..];

                info!(
                    "Peer {:#?} has sent us piece {} starting at offset {}",
                    conn_info, piece_idx, block_offset
                );

                let block_id =
                    ((block_offset as usize / &bt_msg.payload.len() - 8) as f32).floor() as u32;

                peer.pipeline
                    .retain(|p| p.block_id != block_id && p.piece_id != piece_idx);

                info!(
                    "Block {} from Piece {} removed from peer {:#?}'s pipeline",
                    block_id, piece_idx, conn_info
                );

                let entry = peer.buffer.pop_front();

                if let Some(entry) = entry {
                    self.pipeline_enqueue(entry, conn_info).await;
                } else {
                    self.check_empty_pipelines();
                }

                self.piece_store.lock().await.pieces[piece_idx as usize].store_block(
                    block_offset as usize,
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
                // let mut peer = self.find_peer_mut(conn_info);
                warn!("Invalid message from peer {:#?}", conn_info);
            }
        }
    }
}
