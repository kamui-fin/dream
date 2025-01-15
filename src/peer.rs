use std::{
    collections::{HashMap, VecDeque},
    fmt,
    hash::Hash,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Range,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use lazy_static::lazy_static;
use log::{error, info, trace, warn};
use rand::{distributions::Alphanumeric, rngs::OsRng, thread_rng, Rng};
use serde::{Deserialize, Deserializer};
use serde_bytes::ByteBuf;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, mpsc::Sender, Mutex, Notify},
    time::{sleep, timeout, Instant},
};
use tokio_util::codec::Framed;

use crate::{
    bittorrent::TorrentState,
    msg::{BitTorrentCodec, InternalMessage, Message, MessageType},
    piece::{BitField, PieceStore, BLOCK_SIZE, KB_PER_BLOCK},
    tracker::TrackerResponse,
    utils::{slice_to_u32_msb, Notifier},
};

lazy_static! {
    pub static ref DREAM_ID: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(20)
        .map(char::from)
        .collect();
}

const MAX_PIPELINE_SIZE: usize = 4;
const STATS_WINDOW: usize = 5;

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

struct RequestInfo;

pub struct RequestTracker {
    timeout_sender: mpsc::Sender<InternalMessage>,
    requests: Arc<std::sync::Mutex<HashMap<PipelineEntry, RequestInfo>>>,
}

impl RequestTracker {
    pub fn new(timeout_sender: mpsc::Sender<InternalMessage>) -> Self {
        Self {
            requests: Arc::new(std::sync::Mutex::new(HashMap::new())),
            timeout_sender,
        }
    }

    pub fn register_request(&self, entry: PipelineEntry, conn_info: ConnectionInfo) {
        self.requests
            .lock()
            .unwrap()
            .insert(entry.clone(), RequestInfo);
        info!(
            "Registering request for {:?} ({:x})",
            entry,
            entry.block_id * BLOCK_SIZE
        );

        let sender_clone = self.timeout_sender.clone();

        let requests_clone = self.requests.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15)).await;

            if requests_clone.lock().unwrap().remove(&entry).is_some() {
                info!("Request {:?} timed out from peer: {:#?}", entry, conn_info);
                if sender_clone
                    .send(InternalMessage::MigrateWork)
                    .await
                    .is_err()
                {
                    error!("Failed to send timeout message");
                }
            }
        });
    }

    pub fn resolve_request(&self, entry: PipelineEntry) {
        let mut requests_guard = self.requests.lock().unwrap();
        if requests_guard.remove(&entry).is_some() {
            info!("Request resolved: {:#?}", entry);
        } else {
            error!("[RESOLVE] Request not found: {:#?}", entry);
        }
    }

    pub fn reset(&mut self) {
        self.requests.lock().unwrap().clear();
    }
}

pub struct PeerStats {
    // track the sum of the speeds to calculate kbps every n-seconds
    pub current_speeds_sum: f32,
    // track kb downloaded in current window
    pub current_blocks_downloaded: u32,

    // tracks the overall download speed across ALL previous windows (to be used for choking algo)
    pub total_avg_kbps: f32,
    // tracks the total number downloaded across ALL previous windows
    pub total_kb: u32,
}

impl PeerStats {
    pub fn init_stats() -> Self {
        return Self {
            current_speeds_sum: 0.0,
            current_blocks_downloaded: 0,
            total_avg_kbps: 0.0,
            total_kb: 0,
        };
    }

    pub fn add_new_speed(&mut self, new_speed: &f32) {
        self.current_speeds_sum += new_speed;
        self.current_blocks_downloaded += 1;
    }

    pub fn update_overalls(&mut self) {
        // get the average time it took for one kb (sec per kb)
        let average_time = self.current_speeds_sum / (self.current_blocks_downloaded as f32);

        // convert secs per block to kbps
        let current_window_kbps = (KB_PER_BLOCK) as f32 / average_time;

        // take weighted moving average and weigh the current kbps more than the historical average
        self.total_avg_kbps = (0.125 * current_window_kbps) + (0.875 * self.total_avg_kbps);
        info!("total_avg_kbps: {:#?}", self.total_avg_kbps);

        // track historical kb downloaded
        self.total_kb += self.current_blocks_downloaded * KB_PER_BLOCK;

        // reset current window
        self.current_speeds_sum = 0.0;
        self.current_blocks_downloaded = 0;
    }
}

struct PeerSession {
    // pertaining to connection
    // conn: Arc<Mutex<TcpStream>>,
    // conn: TcpStream,
    framed: Framed<TcpStream, BitTorrentCodec>,
    forwarder: mpsc::Sender<InternalMessage>,
    peer: ConnectionInfo,
    request_tracker: HashMap<PipelineEntry, Instant>,
}

impl PeerSession {
    async fn new_session(
        forwarder: mpsc::Sender<InternalMessage>,
        peer: ConnectionInfo,
        info_hash: &[u8; 20],
    ) -> anyhow::Result<Self> {
        let conn = Self::peer_handshake(peer.clone(), info_hash).await?;
        let framed = Framed::new(conn, BitTorrentCodec);
        Ok(Self {
            framed,
            forwarder,
            peer,
            request_tracker: HashMap::new(),
        })
    }

    async fn from_session(
        mut conn: TcpStream,
        forwarder: mpsc::Sender<InternalMessage>,
        peer: ConnectionInfo,
        info_hash: &[u8; 20],
        bitfield: BitField,
    ) -> anyhow::Result<Self> {
        // respond back with handshake
        let mut handshake = [0u8; 68];
        handshake[0] = 19;
        handshake[1..20].copy_from_slice(b"BitTorrent protocol");
        handshake[28..48].copy_from_slice(info_hash);
        handshake[48..68].copy_from_slice(DREAM_ID.as_bytes());
        conn.write_all(&handshake).await?;

        let mut framed = Framed::new(conn, BitTorrentCodec);
        // send bitfield
        let bitfield_msg = MessageType::Bitfield.build_msg(bitfield.0);
        framed.send(bitfield_msg).await?;

        Ok(Self {
            framed,
            forwarder,
            peer,
            request_tracker: HashMap::new(),
        })
    }

    pub async fn peer_handshake(peer: ConnectionInfo, info_hash: &[u8; 20]) -> Result<TcpStream> {
        let mut handshake = [0u8; 68];
        handshake[0] = 19;
        handshake[1..20].copy_from_slice(b"BitTorrent protocol");
        handshake[28..48].copy_from_slice(info_hash);
        handshake[48..68].copy_from_slice(DREAM_ID.as_bytes());

        let connect_timeout = Duration::from_secs(3);
        let mut stream = match timeout(connect_timeout, TcpStream::connect(peer.addr())).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                error!("Failed to connect to {:?}: {}", peer, e);
                return Err(anyhow!(e));
            }
            Err(_) => {
                error!("Connection attempt timed out with {:?}", peer);
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

    pub fn get_pipeline_entry(bt_msg: Message) -> PipelineEntry {
        let piece_id = slice_to_u32_msb(&bt_msg.payload[0..4]);
        let block_offset = slice_to_u32_msb(&bt_msg.payload[4..8]);
        let block_id = ((block_offset as usize / BLOCK_SIZE as usize) as f32).floor() as u32;

        return PipelineEntry { piece_id, block_id };
    }

    pub async fn start_listening(
        &mut self,
        mut send_jobs: mpsc::Receiver<(Message, Option<PipelineEntry>)>,
    ) {
        loop {
            tokio::select! {
                Some(msg) = self.framed.next() => {
                    match msg {
                        Ok(msg) => {
                            if msg.msg_type == MessageType::KeepAlive {
                                continue;
                            } else {
                                if msg.msg_type == MessageType::Piece {
                                    // get the pipeline entry to track request
                                    let corresponding_entry = Self::get_pipeline_entry(msg.clone());
                                    let new_speed = Instant::now() - self.request_tracker[&corresponding_entry];

                                    // remove the entry once the request has been fulfilled
                                    self.request_tracker.remove(&corresponding_entry);

                                    // send a speed update message to add the new speed to the peer's stats
                                    let speed_updater = InternalMessage::UpdateSpeed{conn_info: self.peer.clone(), speed: new_speed.as_secs_f32()};
                                    self.forwarder.send(speed_updater).await.unwrap();
                                    info!("Block {:#?} came in with speed {:#?}", corresponding_entry.block_id, new_speed);
                                }
                                // send normal piece message
                                let msg = InternalMessage::ForwardMessage {
                                    msg,
                                    conn_info: self.peer.clone(),
                                };
                                trace!("MPSC len: {}", self.forwarder.capacity());
                                self.forwarder.send(msg).await.unwrap();
                            }
                        }
                        Err(e) => {
                            // doesn't matter what msg
                            error!(
                                "Encountered malformed data from peer {:#?}: {:#?}",
                                self.peer,
                                e
                            );
                            let _ = self.framed.close().await;
                            let close_msg = InternalMessage::CloseConnection;
                            self.forwarder.send(close_msg).await.unwrap();
                            return;
                        }
                    }
                }
                Some(msg) = send_jobs.recv() => {
                    if let Err(_) = self.send_message(msg.0.clone(), msg.1).await {
                        let close_msg = InternalMessage::CloseConnection;
                        self.forwarder.send(close_msg).await.unwrap();
                        return;
                    }
                }
                // else => {
                //     break;
                // }
            }
        }
    }

    pub async fn send_message(
        &mut self,
        msg: Message,
        request_entry: Option<PipelineEntry>,
    ) -> anyhow::Result<()> {
        if let Some(entry) = request_entry {
            self.request_tracker.insert(entry, Instant::now());
        }

        let result = self.framed.send(msg).await;

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                error!(
                    "Unable to send msg {e}, Closing connection to peer: {:#?}",
                    self.peer
                );
                self.framed.close().await?;
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
    pub optimistic_unchoke: bool,

    pub is_ready: bool,

    pub pipeline: Vec<PipelineEntry>,
    pub buffer: VecDeque<PipelineEntry>,
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

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct PipelineEntry {
    piece_id: u32,
    block_id: u32,
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
            optimistic_unchoke: false,
        }
    }
}

pub struct PeerManager {
    pub peers: Vec<RemotePeer>,
    piece_store: Arc<Mutex<PieceStore>>,

    // channels
    send_channels: HashMap<ConnectionInfo, Sender<(Message, Option<PipelineEntry>)>>,
    msg_tx: Sender<InternalMessage>, // only stored in case we want to add new peers dynamically

    num_not_ready_peers: Arc<AtomicUsize>,
    notify_all_ready: Arc<Notify>,
    notify_pipelines_empty: Arc<Notifier>,

    pub request_tracker: RequestTracker,
    pub stats_tracker: Arc<Mutex<HashMap<ConnectionInfo, PeerStats>>>,
}

impl PeerManager {
    // Initializes all the peers from the tracker response
    pub async fn connect_peers(
        peers_response: TrackerResponse,
        piece_store: Arc<Mutex<PieceStore>>,
        info_hash: &[u8; 20],
        num_pieces: u32,
        msg_tx: Sender<InternalMessage>,
        notify_all_ready: Arc<Notify>,
        notify_pipelines_empty: Arc<Notifier>,
    ) -> PeerManager {
        let request_tracker = RequestTracker::new(msg_tx.clone());
        let mut send_channels: HashMap<ConnectionInfo, Sender<(Message, Option<PipelineEntry>)>> =
            HashMap::new();

        let peers: Vec<_> = peers_response
            .peers
            .into_iter()
            .map(|peer| RemotePeer::from_peer(peer, num_pieces))
            .collect();

        let num_peers = peers.len();

        let num_not_ready_peers = Arc::new(AtomicUsize::new(num_peers));
        let stats_tracker = Arc::new(Mutex::new(HashMap::new()));

        for remote_peer in &peers {
            let info_hash_clone = info_hash.clone();
            let conn_info = remote_peer.conn_info.clone();
            let tx_clone = msg_tx.clone();

            let not_ready_peers_clone = Arc::clone(&num_not_ready_peers);
            let notify_all_ready_clone = notify_all_ready.clone();

            // initialize each peer's stats
            stats_tracker
                .lock()
                .await
                .insert(conn_info.clone(), PeerStats::init_stats());
            let stats_tracker_clone = stats_tracker.clone();
            let target_conn_info = conn_info.clone();

            // create a task per peer that waits 5 seconds then updates the historical average of that peer
            tokio::spawn(async move {
                loop {
                    sleep(Duration::from_secs(STATS_WINDOW as u64)).await;
                    let mut tracker_guard = stats_tracker_clone.lock().await;
                    if tracker_guard.contains_key(&target_conn_info) {
                        let curr_stats = tracker_guard.get_mut(&target_conn_info).unwrap();
                        curr_stats.update_overalls();
                        // info!("5 seconds up, peer {:#?} has new kbps of {:#?}", target_conn_info, curr_stats.total_avg_kbps);
                    } else {
                        break;
                    }
                }
            });

            // the channel for sending messages to this peer session
            let (send_msg, recv_msg) = mpsc::channel(2000);
            send_channels.insert(conn_info.clone(), send_msg);

            tokio::spawn(async move {
                let session = PeerSession::new_session(tx_clone, conn_info, &info_hash_clone).await;
                if let Ok(mut session) = session {
                    session.start_listening(recv_msg).await;
                } else {
                    let old = not_ready_peers_clone.fetch_sub(1, Ordering::SeqCst);
                    info!("Counter: {}", old - 1);
                    if not_ready_peers_clone.load(Ordering::SeqCst) <= 5 {
                        notify_all_ready_clone.notify_one();
                    }
                }
            });
        }

        Self {
            peers,
            send_channels,
            msg_tx,
            piece_store,
            notify_pipelines_empty,
            notify_all_ready,
            num_not_ready_peers,
            request_tracker,
            stats_tracker,
        }
    }

    pub async fn sync_peers(&mut self, peers: TrackerResponse) {
        for peer in peers.peers {
            if self.find_peer(&peer).is_none() {
                self.create_peer(peer.clone()).await;

                let tx_clone = self.msg_tx.clone();
                // the channel for sending messages to this peer session
                let (send_msg, recv_msg) = mpsc::channel(2000);
                self.send_channels.insert(peer.clone(), send_msg);

                let info_hash_clone = self
                    .piece_store
                    .lock()
                    .await
                    .meta_file
                    .get_info_hash()
                    .clone();

                tokio::spawn(async move {
                    let session = PeerSession::new_session(tx_clone, peer, &info_hash_clone).await;
                    if let Ok(mut session) = session {
                        session.start_listening(recv_msg).await;
                    }
                });
            }
        }
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

    pub async fn init_session(
        &mut self,
        stream: TcpStream,
        conn_info: ConnectionInfo,
        info_hash: [u8; 20],
    ) {
        let bitfield = self.piece_store.lock().await.get_status_bitfield().clone();

        let tx_clone = self.msg_tx.clone();
        // the channel for sending messages to this peer session
        let (send_msg, recv_msg) = mpsc::channel(2000);
        self.send_channels.insert(conn_info.clone(), send_msg);

        tokio::spawn(async move {
            let session =
                PeerSession::from_session(stream, tx_clone, conn_info, &info_hash, bitfield).await;
            if let Ok(mut session) = session {
                session.start_listening(recv_msg).await;
            }
        });
    }

    pub async fn create_peer(&mut self, addr: ConnectionInfo) {
        let num_pieces = self.piece_store.lock().await.num_pieces;
        if !self.peers.iter().any(|p| p.conn_info == addr) {
            let new_peer = RemotePeer::from_peer(addr, num_pieces);
            self.peers.push(new_peer);
        }
    }

    pub fn find_peer(&self, peer: &ConnectionInfo) -> Option<&RemotePeer> {
        self.peers
            .iter()
            .find(|curr_peer| curr_peer.conn_info == *peer)
    }

    pub fn find_peer_mut(&mut self, peer: &ConnectionInfo) -> Option<&mut RemotePeer> {
        self.peers
            .iter_mut()
            .find(|curr_peer| curr_peer.conn_info == *peer)
    }

    pub async fn queue_blocks(
        &mut self,
        conn_info: &ConnectionInfo,
        piece_id: u32,
        blocks: Range<u32>,
    ) {
        info!("Queuing blocks {:#?} for {piece_id}", blocks);
        for block_id in blocks {
            let entry = PipelineEntry { piece_id, block_id };
            let peer = self.find_peer_mut(conn_info);
            if let Some(peer) = peer {
                if peer.pipeline.len() < MAX_PIPELINE_SIZE {
                    peer.pipeline.push(entry.clone());
                    self.request_block(entry, conn_info).await;
                } else {
                    peer.buffer.push_back(entry);
                }
            } else {
                error!("[QUEUE_BLOCKS] Peer {:#?} not found", conn_info);
            }
        }

        let peer = self.find_peer(conn_info);
        if let Some(peer) = peer {
            info!(
                "New pipeline: {:?} | New buffer: {:?}",
                peer.pipeline, peer.buffer
            );
        }
    }

    pub async fn peer_ready_for_piece(&self, peer: &ConnectionInfo, piece_idx: u32) -> bool {
        let peer = self.find_peer(peer);
        if let Some(peer) = peer {
            peer.piece_lookup.piece_exists(piece_idx) && !peer.peer_choking
        } else {
            warn!("[HAS_PIECE] Peer {:#?} not found", peer);
            false
        }
    }

    pub async fn broadcast_have(&mut self, piece_idx: u32) {
        let conns: Vec<_> = self.send_channels.keys().cloned().collect();
        for connection in conns {
            self.send_message(
                &connection,
                MessageType::Have.build_msg(piece_idx.to_be_bytes().to_vec()),
            )
            .await;
        }
    }

    pub async fn unchoke(&mut self, conn_info: &ConnectionInfo) {
        self.send_message(conn_info, MessageType::UnChoke.build_empty())
            .await;
    }

    pub async fn choke(&mut self, conn_info: &ConnectionInfo) {
        self.send_message(conn_info, MessageType::Choke.build_empty())
            .await;
    }

    pub async fn show_interest_in_peer(&mut self, conn_info: &ConnectionInfo) {
        info!("Showing interest in peer {:#?}", conn_info);
        {
            let peer = self.find_peer_mut(conn_info);
            if let Some(peer) = peer {
                peer.am_interested = true;
            } else {
                warn!("[INTEREST] Peer {:#?} not found", conn_info);
                return;
            }
        }
        self.send_message(conn_info, MessageType::Interested.build_msg(vec![]))
            .await;
    }

    pub async fn send_message(&mut self, conn_info: &ConnectionInfo, msg: Message) {
        if let Err(_) = self.send_channels[conn_info].send((msg, None)).await {
            error!("Lost connection to peer {:#?}", conn_info);
            self.remove_peer(conn_info).await;
        }
    }

    async fn pipeline_enqueue(&mut self, entry: PipelineEntry, conn_info: &ConnectionInfo) {
        let peer = self.find_peer_mut(conn_info);
        if let Some(peer) = peer {
            peer.pipeline.push(entry.clone());
            self.request_block(entry, conn_info).await;
        } else {
            warn!("[PIPELINE_ADD] Peer {:#?} not found", conn_info);
        }
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
            "Sent a request for block {} of piece {} to peer {:?}",
            block_id, piece_id, conn_info
        );
        let msg = MessageType::Request.build_msg(payload);
        self.send_channels[&conn_info.clone()]
            .send((msg, Some(entry.clone())))
            .await
            .unwrap();

        self.request_tracker
            .register_request(entry, conn_info.clone());
    }

    fn mark_peer_ready(&self) {
        trace!(
            "Counter: {}",
            self.num_not_ready_peers.load(Ordering::SeqCst)
        );
        self.num_not_ready_peers.fetch_sub(1, Ordering::SeqCst);
        self.check_peers_ready();
    }

    fn check_peers_ready(&self) {
        if self.num_not_ready_peers.load(Ordering::SeqCst) <= 5 {
            self.notify_all_ready.notify_one();
        }
    }

    pub async fn redistribute_work(
        &mut self,
        conn_info: &ConnectionInfo,
        mut work: Vec<PipelineEntry>,
    ) {
        if !work.is_empty() {
            let peers_with_piece: Vec<&mut RemotePeer> = self
                .peers
                .iter_mut()
                .filter(|p| p.piece_lookup.piece_exists(work[0].piece_id))
                .filter(|p| p.conn_info != *conn_info && !p.peer_choking)
                .collect();

            if peers_with_piece.is_empty() {
                panic!(); // if there is not a single peer with ALL pieces, this breaks our assumption!
            }

            let blocks_per_peer = std::cmp::max(1, (work.len() / peers_with_piece.len()) as u32);
            let mut batch_request = vec![]; // we have to request later due to borrow checker

            info!(
                "Redistributing work of {} to {} peers ({} each)",
                work.len(),
                peers_with_piece.len(),
                blocks_per_peer
            );

            for peer in peers_with_piece {
                let end_index = if work.len() < blocks_per_peer as usize {
                    work.len()
                } else {
                    blocks_per_peer as usize
                };
                for entry in work.drain(0..end_index) {
                    if peer.pipeline.len() < MAX_PIPELINE_SIZE {
                        peer.pipeline.push(entry.clone());
                        batch_request.push((entry, peer.conn_info.clone()));
                    } else {
                        peer.buffer.push_back(entry);
                    }
                }
            }

            for (entry, conn) in batch_request {
                self.request_block(entry, &conn).await;
            }
        }
    }

    pub async fn remove_peer(&mut self, conn_info: &ConnectionInfo) {
        let peer = self.find_peer(conn_info);
        if let Some(peer) = peer {
            error!(
                "Removing peer {:#?} with pipeline: {:#?} and buffer: {:#?}",
                conn_info, peer.pipeline, peer.buffer
            );
            if !peer.is_ready {
                self.mark_peer_ready();
            }
        } else {
            warn!("[DETETE_PEER] Peer {:#?} not found", conn_info);
            return;
        }

        self.send_channels.remove(conn_info);

        let index = self
            .peers
            .iter()
            .position(|p| &p.conn_info == conn_info)
            .unwrap();

        let removed_peer = self.peers.swap_remove(index);
        let mut work = removed_peer.pipeline;
        work.extend(removed_peer.buffer);

        self.redistribute_work(conn_info, work).await;
    }

    pub async fn recompute_choke_list(&mut self, torrent_state: TorrentState) {
        // move optimistic unchoke peer to the end so we don't count it in our list of top peers
        let optimistic_unchoke = self
            .peers
            .iter()
            .position(|item| item.optimistic_unchoke)
            .map(|pos| self.peers.remove(pos));

        // sort peers by average speed depending on torrent state
        // self.peers.sort_by_key(|p| match torrent_state {
        //     TorrentState::Seeder => p.stats.avg_download_speed,
        //     TorrentState::Leecher => p.stats.avg_upload_speed,
        // });
        self.peers.reverse();

        // select top peers (up to 4)
        let top_count = self.peers.len().min(4);
        let top_peers: Vec<_> = self.peers[..top_count]
            .iter()
            .map(|p| p.conn_info.clone())
            .collect();
        let rest_of_peers: Vec<_> = self.peers[top_count..]
            .iter()
            .map(|p| p.conn_info.clone())
            .collect();

        // unchoke top peers
        for peer in top_peers {
            self.unchoke(&peer).await;
        }

        // choke the remaining peers
        for peer in rest_of_peers {
            self.choke(&peer).await;
        }

        // restore the optimistic unchoke peer if it exists
        if let Some(peer) = optimistic_unchoke {
            self.peers.push(peer);
        }
    }

    pub async fn optimistic_unchoke(&mut self) {
        if self.peers.len() > 4 {
            let mut rng = OsRng;
            let random_index = rng.gen_range(4..self.peers.len());
            let random_peer = &mut self.peers[random_index];
            random_peer.optimistic_unchoke = true;
            let random_peer_conn = random_peer.conn_info.clone();
            self.unchoke(&random_peer_conn).await;
        }
    }

    pub async fn handle_msg(&mut self, fw_msg: &InternalMessage, conn_info: &ConnectionInfo) {
        let peer = self.find_peer_mut(conn_info);
        if peer.is_none() {
            warn!("Peer {:#?} not found", conn_info);
            return;
        }

        let peer = peer.unwrap();
        let was_ready = peer.is_ready;
        if !was_ready {
            peer.is_ready = true;
        }
        match fw_msg {
            InternalMessage::ForwardMessage { msg, conn_info } => {
                match msg.msg_type {
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
                        let piece_index = slice_to_u32_msb(&msg.payload[0..4]);
                        peer.piece_lookup.mark_piece(piece_index);

                        info!(
                            "Peer {:#?} has confirmed that they have piece with piece-index {}",
                            peer.conn_info, piece_index
                        );
                    }
                    MessageType::Bitfield => {
                        // info about which pieces peer has
                        // only sent right after handshake, and before any other msg (so optional)
                        peer.piece_lookup = BitField(msg.payload.clone());
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
                        let piece_idx = slice_to_u32_msb(&msg.payload[0..4]);
                        info!(
                            "Peer {:#?} has requested a piece with index {}",
                            peer.conn_info, piece_idx
                        );

                        if !peer.am_choking && peer.piece_lookup.piece_exists(piece_idx) {
                            let byte_offset = slice_to_u32_msb(&msg.payload[4..8]);
                            let length = slice_to_u32_msb(&msg.payload[8..12]);

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
                                .send((MessageType::Piece.build_msg(pay_load), None))
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
                        let piece_idx = slice_to_u32_msb(&msg.payload[0..4]);
                        let block_offset = slice_to_u32_msb(&msg.payload[4..8]);
                        let block_data = &msg.payload[8..];
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

                        peer.pipeline.retain(|p| p.block_id != block_id);

                        info!(
                            "AFTER: Peer {:?} has pipeline: {:?} and buffer: {:?}",
                            peer, peer.pipeline, peer.buffer
                        );

                        let entry = peer.buffer.pop_front();

                        self.request_tracker.resolve_request(PipelineEntry {
                            piece_id: piece_idx,
                            block_id,
                        });

                        if let Some(entry) = entry {
                            self.pipeline_enqueue(entry, conn_info).await;
                        }

                        if self.piece_store.lock().await.pieces[piece_idx as usize].store_block(
                            block_offset as usize,
                            block_data.len(),
                            block_data,
                        ) {
                            info!("Notifying that we're finished with the piece...");
                            self.notify_pipelines_empty.notify_one();
                        }
                    }
                    MessageType::Cancel => {}
                    MessageType::Port => {}
                }
            }
            InternalMessage::CloseConnection => {
                self.remove_peer(conn_info).await;
            }

            InternalMessage::MigrateWork => {
                // pop all items from peer's pipeline and buffer
                let work: Vec<PipelineEntry> = {
                    let peer = self.find_peer_mut(conn_info).unwrap();
                    let mut work: Vec<PipelineEntry> = peer.pipeline.drain(..).collect();
                    work.extend(peer.buffer.drain(..));
                    work
                };
                info!("Migrating work {:?}", work);
                self.redistribute_work(conn_info, work).await;
            }

            InternalMessage::UpdateSpeed { conn_info, speed } => {
                // update speed to the corresponding peer
                if let Some(curr_stats) =
                    self.stats_tracker.lock().await.get_mut(&conn_info.clone())
                {
                    curr_stats.add_new_speed(speed);
                    info!("Added new speed {:#?} to peer {:#?}", speed, conn_info);
                } else {
                    warn!("Invalid update message found");
                }
            }
        }

        if !was_ready {
            self.mark_peer_ready();
        }
    }
}
