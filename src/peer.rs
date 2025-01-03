use anyhow::Context;
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ByteOrder};
use futures::future::{self, join_all, Join, Remote};
use futures::{SinkExt, StreamExt};
use http_req::tls::Conn;
use lazy_static::lazy_static;
use log::{debug, trace};
use log::warn;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde::{Deserialize, Deserializer};
use serde_bytes::ByteBuf;
use sha1::digest::typenum::Bit;
use tokio::time::{self, sleep};
use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr};
use std::ptr::read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{collections::VecDeque, net::SocketAddr, ops::Range, sync::Arc, time::Duration};
use tokio::sync::mpsc::Receiver;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
// use tokio::sync::{mpsc, Mutex, Notify};
use tokio_util::codec::Framed;

use log::{error, info};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::Sender,
    time::timeout,
};

use crate::bittorrent::BitTorrent;
use crate::msg::{BitTorrentCodec, InternalMessage};
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
    // conn: Arc<Mutex<TcpStream>>,
    // conn: TcpStream,
    framed: Framed<TcpStream, BitTorrentCodec>,
    sender: mpsc::Sender<InternalMessage>,
    peer: ConnectionInfo,
}

impl PeerSession {
    async fn new_session(
        sender: mpsc::Sender<InternalMessage>,
        peer: ConnectionInfo,
        info_hash: &[u8; 20],
    ) -> anyhow::Result<Self> {
        let conn = Self::peer_handshake(peer.clone(), info_hash).await?;
        let framed = Framed::new(conn, BitTorrentCodec);
        Ok(Self {
            framed,
            sender,
            peer,
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

    pub async fn start_listening(&mut self, mut recv_msg: mpsc::Receiver<Message>) {
        loop {
            tokio::select! {
                msg = self.framed.next() => {
                    match msg {
                        Some(msg) => match msg {
                            Ok(msg) => {
                                if msg.msg_type == MessageType::KeepAlive {
                                    // handle keep alives
                                } else {
                                    let msg = InternalMessage {
                                        msg,
                                        conn_info: self.peer.clone(),
                                        should_close: false,
                                    };
                                    self.sender.send(msg).await.unwrap();
                                }
                            }
                            Err(e) => {
                                // doesn't matter what msg
                                error!(
                                    "Encountered malformed data from peer {:#?}: {:#?}",
                                    self.peer,
                                    e
                                );
                                self.framed.close().await.unwrap();
                                let close_msg = InternalMessage {
                                    msg: MessageType::KeepAlive.build_msg(vec![]),
                                    conn_info: self.peer.clone(),
                                    should_close: true,
                                };
                                self.sender.send(close_msg).await.unwrap();
                                return;
                            }
                        }
                        None => {
                            error!("Stream has been exhausted (EOF) for peer {:?} ! Unrecoverable...", self.peer);
                            self.framed.close().await.unwrap();
                            let close_msg = InternalMessage {
                                msg: MessageType::KeepAlive.build_msg(vec![]),
                                conn_info: self.peer.clone(),
                                should_close: true,
                            };
                            self.sender.send(close_msg).await.unwrap();
                            return;
                        }
                    }
                }
                Some(msg) = recv_msg.recv() => {
                    if let Err(_) = self.send_message(msg.clone()).await {
                        let close_msg = InternalMessage { msg: MessageType::KeepAlive.build_msg(vec![]), conn_info: self.peer.clone(), should_close: true };
                        self.framed.close().await.unwrap();
                        self.sender.send(close_msg).await.unwrap();
                        return;
                    }
                    info!("PeerSession sent {:?} successfully", msg);
                }
                else => {
                    break;
                }
            }
        }
    }

    pub async fn send_message(&mut self, msg: Message) -> anyhow::Result<()> {
        let result = self.framed.send(msg).await;

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("Message send resulted in error: {:#?}", e);
                info!("Closing connection to peer: {:#?}", self.peer);
                self.framed.close().await.unwrap();
                Err(anyhow!(e))
            }
        }
    }
}

pub struct RemotePeer {
    pub conn_info: ConnectionInfo,
    pub piece_lookup: BitField,
    pub am_choking: bool,      // = 1
    pub am_interested: bool,   // = 0
    pub peer_choking: bool,    // has this peer choked us? = 1
    pub peer_interested: bool, // is this peer interested in us? = 0

    pub is_ready: bool,

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

#[derive(Clone, Debug)]
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
            piece_lookup: BitField::new(num_pieces),
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            pipeline: Vec::new(),
            buffer: VecDeque::new(),
            is_ready: false,
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

    num_not_ready_peers: Arc<AtomicUsize>,
    notify_all_ready: Arc<Notify>,
    notify_pipelines_empty: Arc<Notify>,
    keep_alive_senders: Arc<Mutex<HashMap<ConnectionInfo, JoinHandle<()>>>>,
    keep_alive_receivers: Arc<Mutex<HashMap<ConnectionInfo, JoinHandle<()>>>>

}

impl PeerManager {
    // Initializes all the peers from the tracker response
    pub async fn connect_peers(
        peers_response: TrackerResponse,
        piece_store: Arc<Mutex<PieceStore>>,
        info_hash: &[u8; 20],
        num_pieces: u32,
        unchoke_tx: Sender<UnchokeMessage>,
        msg_tx: Sender<InternalMessage>,
        notify_all_ready: Arc<Notify>,
        notify_pipelines_empty: Arc<Notify>,
    ) -> PeerManager {
        info!(
            "Attempting to connect to {} peers",
            peers_response.peers.len()
        );

        let mut send_channels: HashMap<ConnectionInfo, Sender<Message>> = HashMap::new();

        let peers: Vec<_> = peers_response
            .peers
            .into_iter()
            .map(|peer| RemotePeer::from_peer(peer, num_pieces))
            .collect();

        info!("{:#?}", peers);

        let num_peers = peers.len();
        let not_ready_peers = Arc::new(AtomicUsize::new(num_peers));

        let mut keep_alive_receivers: Arc<Mutex<HashMap<ConnectionInfo, JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

        let mut keep_alive_senders: HashMap<ConnectionInfo, JoinHandle<()>> =
        HashMap::new();

        for remote_peer in &peers {
            let info_hash_clone = info_hash.clone();
            let conn_info = remote_peer.conn_info.clone();
            let tx_clone = msg_tx.clone();

            let not_ready_peers_clone = Arc::clone(&not_ready_peers);
            let notify_all_ready_clone = notify_all_ready.clone();

            // the channel for sending messages to this peer session
            let (send_msg, recv_msg) = mpsc::channel(500);

            let sender_clone = send_msg.clone();
            send_channels.insert(conn_info.clone(), send_msg);
            

            keep_alive_senders.insert(
                conn_info.clone(),
                tokio::spawn(async move{
                    sleep(Duration::from_secs(60*2)).await;
                    sender_clone.send(MessageType::KeepAlive.build_msg(Vec::new())).await.unwrap();
                })
            );

            tokio::spawn(async move {
                let session = PeerSession::new_session(tx_clone, conn_info, &info_hash_clone).await;
                if let Ok(mut session) = session {
                    session.start_listening(recv_msg).await;
                } else {
                    let old = not_ready_peers_clone.fetch_sub(1, Ordering::SeqCst);
                    info!("Counter: {}", old - 1);
                    if not_ready_peers_clone.load(Ordering::SeqCst) == 5 {
                        notify_all_ready_clone.notify_one();
                    }
                }
            });
        }

        Self {
            peers,
            send_channels,
            msg_tx,
            unchoke_tx,
            piece_store,
            notify_pipelines_empty,
            notify_all_ready,
            num_not_ready_peers: not_ready_peers,
            keep_alive_receivers,
            keep_alive_senders: Arc::new(Mutex::new(keep_alive_senders))
        }
    }

    pub async fn reset_keep_alive(&self, conn_info: ConnectionInfo){
        let mut timers = self.keep_alive_senders.lock().await;
        let sender = self.send_channels[&conn_info].clone();
        let conn_clone = conn_info.clone();

        trace!("Resetting keepalive sender for peer {:#?}", conn_clone);
        if timers.contains_key(&conn_info){
            timers[&conn_info].abort();
        }

        timers.insert(
            conn_info,
            tokio::spawn(async move {
                sleep(Duration::from_secs(60*2)).await;
                warn!("Inactivity detected, sending keepalive to peer: {:#?}", conn_clone);
                let _ = sender.send(MessageType::KeepAlive.build_msg(Vec::new())).await;
            })
        );

    }

    pub async fn reset_timer(&self, conn_info: ConnectionInfo){
        let mut timers = self.keep_alive_receivers.lock().await;
        let sender = self.msg_tx.clone();
        let conn_clone = conn_info.clone();

        trace!("Resetting keepalive receiver for peer {:#?}", conn_clone);
        if timers.contains_key(&conn_info){
            timers[&conn_info].abort();
        }

        timers.insert(
            conn_info,
            tokio::spawn(async move {
                sleep(Duration::from_secs(60*2)).await;
                warn!("Detected inactivity for too long, closing conneciton with peer {:#?}", conn_clone);

                let close_msg = InternalMessage {
                    msg: MessageType::KeepAlive.build_msg(vec![]),
                    conn_info: conn_clone,
                    should_close: true,
                };
                sender.send(close_msg).await.unwrap();
            })
        );

    }

    pub fn check_empty_pipelines(&self) {
        if self
            .peers
            .iter()
            .all(|p| p.buffer.is_empty() && p.pipeline.is_empty())
        {
            info!("About to notify that we're done??");
            self.notify_pipelines_empty.notify_one();
        }
    }

    pub async fn with_piece(&self, piece_idx: u32) -> Vec<ConnectionInfo> {
        let mut result = Vec::new();
        for peer in &self.peers {
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
                let session = PeerSession::new_session(tx_clone, conn_info, &info_hash).await;
                if let Ok(mut session) = session {
                    session.start_listening(recv_msg).await;
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
        // info!("Queuing blocks {:#?} for {piece_id}", blocks);
        for block_id in blocks {
            let entry = PipelineEntry { piece_id, block_id };
            let peer = self.find_peer_mut(conn_info);
            if peer.pipeline.len() < MAX_PIPELINE_SIZE {
                peer.pipeline.push(entry.clone());
                self.request_block(entry, conn_info).await;
            } else {
                peer.buffer.push_back(entry);
            }
        }
        info!(
            "Peer {:?} pipeline: {:?} and buffer: {:?}",
            conn_info,
            self.find_peer(conn_info).pipeline,
            self.find_peer(conn_info).buffer
        )
    }

    pub async fn peer_has_piece(&self, peer: &ConnectionInfo, piece_idx: u32) -> bool {
        let peer = self.find_peer(peer);
        peer.piece_lookup.piece_exists(piece_idx)
    }

    pub async fn show_interest_in_peer(&mut self, conn_info: &ConnectionInfo) {
        info!("Showing interest in peer {:#?}", conn_info);
        {
            let peer = self.find_peer_mut(conn_info);
            peer.am_interested = true;
        }
        self.send_message(conn_info, MessageType::Interested.build_msg(vec![]))
            .await;
    }

    pub async fn send_message(&mut self, conn_info: &ConnectionInfo, msg: Message) {
        self.reset_keep_alive(conn_info.clone()).await;
        self.send_channels[conn_info].send(msg).await.unwrap();
    }

    async fn pipeline_enqueue(&mut self, entry: PipelineEntry, conn_info: &ConnectionInfo) {
        let peer = self.find_peer_mut(conn_info);
        peer.pipeline.push(entry.clone());
        self.request_block(entry, conn_info).await;
    }

    async fn request_block(&mut self, entry: PipelineEntry, conn_info: &ConnectionInfo) {
        let PipelineEntry { piece_id, block_id } = entry;
        let piece_id_bytes = piece_id.to_be_bytes();
        let block_offset_bytes = (block_id * BLOCK_SIZE).to_be_bytes();
        let block_size = BLOCK_SIZE.to_be_bytes();

        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&piece_id_bytes);
        payload.extend_from_slice(&block_offset_bytes);
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

    fn mark_peer_ready(&self) {
        info!(
            "Counter: {}",
            self.num_not_ready_peers.load(Ordering::SeqCst)
        );
        self.num_not_ready_peers.fetch_sub(1, Ordering::SeqCst);
        self.check_peers_ready();
    }

    fn check_peers_ready(&self) {
        if self.num_not_ready_peers.load(Ordering::SeqCst) == 5 {
            self.notify_all_ready.notify_one();
        }
    }

    pub fn remove_peer(&mut self, conn_info: &ConnectionInfo) {
        // TODO: handle pipeline & buffer merging
        error!(
            "Removing peer {:#?} with pipeline: {:#?} and buffer: {:#?}",
            conn_info,
            self.find_peer(conn_info).pipeline,
            self.find_peer(conn_info).buffer
        );
        if !self.find_peer(conn_info).is_ready {
            self.mark_peer_ready();
        }
        self.send_channels.remove(conn_info);
        self.peers.retain(|p| &p.conn_info != conn_info);
    }

    pub async fn handle_msg(&mut self, bt_msg: &Message, conn_info: &ConnectionInfo) {
        // reset the timers for that particular peer
        self.reset_timer(conn_info.clone()).await;
        self.reset_keep_alive(conn_info.clone()).await;

        let peer = self.find_peer_mut(conn_info);
        let was_ready = peer.is_ready;
        if !was_ready {
            peer.is_ready = true;
        }
        match bt_msg.msg_type {
            MessageType::KeepAlive => {
                // close connection after 2 min of inactivity (no commands)
                // keepalive is just a dummy msg to reset that timer
                // info!("Received keep alive")
                info!("Received keep alive from peer: {:#?}", peer);
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
                info!(
                    "Peer {:#?} has informed us that is has pieces {}",
                    peer.conn_info,
                    peer.piece_lookup.return_piece_indexes()
                );

                if !peer.is_ready {
                    peer.is_ready = true;
                    self.mark_peer_ready();
                }
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
                let block_id =
                    ((block_offset as usize / BLOCK_SIZE as usize) as f32).floor() as u32;

                info!(
                    "Peer {:#?} has sent us piece {} starting at offset {} with length {}. Determined block id = {}",
                    conn_info,
                    piece_idx,
                    block_offset,
                    block_data.len(),
                    block_id
                );

                info!(
                    "BEFORE: Peer {:?} has pipeline: {:?} and buffer: {:?}",
                    peer, peer.pipeline, peer.buffer
                );

                peer.pipeline.retain(|p| p.block_id != block_id);

                info!(
                    "AFTER: Peer {:?} has pipeline: {:?} and buffer: {:?}",
                    peer, peer.pipeline, peer.buffer
                );

                info!(
                    "Block {} from Piece {} removed from peer {:#?}'s pipeline",
                    block_id, piece_idx, conn_info
                );

                let entry = peer.buffer.pop_front();

                if let Some(entry) = entry {
                    self.pipeline_enqueue(entry, conn_info).await;
                }

                self.piece_store.lock().await.pieces[piece_idx as usize].store_block(
                    block_offset as usize,
                    block_data.len(),
                    block_data,
                );

                self.check_empty_pipelines();
            }
            MessageType::Cancel => {
                // informing us that block <index><begin><length> is not needed anymore
                // for endgame algo
            }
            MessageType::Port => {
                // port that their dht node is listening on
                // only for DHT extension
            }
            _ => {
                // let mut peer = self.find_peer_mut(conn_info);
                warn!("Invalid message from peer {:#?}", conn_info);
            }
        }

        if !was_ready {
            self.mark_peer_ready();
        }
    }
}
 