use anyhow::{anyhow, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use lazy_static::lazy_static;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde::{Deserialize, Deserializer};
use serde_bytes::ByteBuf;
use std::net::{IpAddr, Ipv4Addr};
use std::{
    collections::VecDeque,
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    ops::Range,
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

use log::{error, info};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{unix::pipe::pipe, TcpStream},
    sync::mpsc::{Receiver, Sender},
    time::timeout,
};

use crate::{
    msg::{Message, MessageType},
    piece::{BitField, PieceStore, RequestStatus, BLOCK_SIZE},
    tracker::Metafile,
    utils::slice_to_u32_msb,
};

lazy_static! {
    static ref DREAM_ID: String = thread_rng()
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
    pub session: PeerSession,
    pub piece_lookup: BitField,
    pub am_choking: bool,      // = 1
    pub am_interested: bool,   // = 0
    pub peer_choking: bool,    // has this peer choked us? = 1
    pub peer_interested: bool, // is this peer interested in us? = 0
    unchoke_tx: Sender<UnchokeMessage>,
}

#[derive(Clone)]
pub struct PipelineEntry {
    piece_id: u32,
    block_id: u32,
}

pub struct PeerSession {
    conn: Arc<Mutex<TcpStream>>,
    piece_store: Arc<Mutex<PieceStore>>,
    // must always keep pipeline full with outgoing requests
    // -> therefore must have access to next block id
    // -> pass in closure to retrieve
    // entry must exit pipeline after block is received
    pipeline: Vec<PipelineEntry>,
    buffer: VecDeque<PipelineEntry>,
}

impl PeerSession {
    async fn connect_peer(peer: Peer, piece_store: Arc<Mutex<PieceStore>>) -> Self {
        let conn = Self::peer_handshake(peer, &piece_store.lock().await.meta_file.get_info_hash())
            .await
            .unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let pipeline = Vec::new();

        Self {
            conn,
            piece_store,
            pipeline,
            buffer: VecDeque::new(),
        }
    }

    pub async fn peer_handshake(peer: Peer, info_hash: &[u8; 20]) -> Result<TcpStream> {
        info!("Initiating peer handshake with {:#?}", peer);

        let mut handshake = [0u8; 68];
        handshake[0] = 19;
        handshake[1..20].copy_from_slice(b"BitTorrent protocol");
        handshake[28..48].copy_from_slice(info_hash);
        handshake[48..68].copy_from_slice(DREAM_ID.as_bytes());

        let connect_timeout = Duration::from_secs(5);
        let mut stream = match timeout(connect_timeout, TcpStream::connect(peer.addr())).await {
            Ok(Ok(stream)) => stream,
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
            Ok(stream)
        } else {
            stream.shutdown().await?;
            error!("Handshake mismatch");
            Err(anyhow!("Handshake failed"))
        }
    }

    pub async fn send_message(conn: Arc<Mutex<TcpStream>>, msg: Message) {
        conn.lock().await.write_all(&msg.serialize()).await;
    }

    pub async fn receive_message(conn: Arc<Mutex<TcpStream>>) -> Option<Message> {
        let mut conn = conn.lock().await;

        let mut len_buf = [0u8; 4];
        conn.read_exact(&mut len_buf).await.ok()?;

        let msg_length = slice_to_u32_msb(&len_buf);
        if msg_length > 0 {
            let mut id_buf = [0u8; 1];
            conn.read_exact(&mut id_buf).await.ok()?;

            let mut payload_buf = vec![0u8; msg_length as usize];
            conn.read_exact(&mut payload_buf).await.ok()?;

            Some(MessageType::from_id(Some(id_buf[0])).build_msg(payload_buf))
        } else {
            Some(MessageType::KeepAlive.build_msg(vec![]))
        }
    }

    async fn request_block(conn: Arc<Mutex<TcpStream>>, entry: PipelineEntry) {
        let PipelineEntry { piece_id, block_id } = entry;
        let piece_id_bytes = piece_id.to_be_bytes();
        let block_id_bytes = block_id.to_be_bytes();
        let block_size = BLOCK_SIZE.to_be_bytes();

        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&piece_id_bytes);
        payload.extend_from_slice(&block_id_bytes);
        payload.extend_from_slice(&block_size);

        let msg = MessageType::Request.build_msg(payload);
        Self::send_message(conn, msg).await;
    }

    pub fn queue_blocks(&mut self, piece_id: u32, blocks: Range<u32>) {
        for block_id in blocks {
            self.buffer.push_back(PipelineEntry { piece_id, block_id });
        }

        self.refresh_pipeline();
    }

    fn refresh_pipeline(&mut self) {
        for _ in 0..(MAX_PIPELINE_SIZE - self.pipeline.len()) {
            if let Some(entry) = self.buffer.pop_front() {
                self.pipeline_enqueue(entry);
            }
        }
    }

    async fn pipeline_enqueue(&mut self, entry: PipelineEntry) {
        self.pipeline.push(entry.clone());
        let conn = self.conn.clone();

        tokio::spawn(async move {
            Self::request_block(conn, entry).await;
        });
        // start request
    }
}

pub struct UnchokeMessage {
    pub peer: Peer,
}

impl RemotePeer {
    async fn from_peer(
        peer: Peer,
        piece_store: Arc<Mutex<PieceStore>>,
        unchoke_channel: Sender<UnchokeMessage>,
    ) -> Self {
        let session = PeerSession::connect_peer(peer.clone(), piece_store).await;

        Self {
            peer,
            session,
            piece_lookup: BitField::new(0),
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            unchoke_tx: unchoke_channel,
        }
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
        unchoke_channel: Sender<UnchokeMessage>,
    ) {
        if !self.swarm.iter().any(|p| p.peer.addr() == addr) {
            let new_peer =
                RemotePeer::from_peer(Peer::from_addr(addr), piece_store, unchoke_channel).await;
            self.swarm.push(new_peer); // will be at len - 1
        }
    }

    pub fn find_peer(&self, peer: &Peer) -> Option<&RemotePeer> {
        self.swarm.iter().find(|p| p.peer == *peer)
    }

    pub fn find_peer_mut(&mut self, peer: &Peer) -> Option<&mut RemotePeer> {
        self.swarm.iter_mut().find(|p| p.peer == *peer)
    }

    pub fn queue_blocks_for_peer(&mut self, peer: &Peer, piece_idx: u32, num_blocks: Range<u32>) {
        let peer = self.find_peer_mut(peer);
        if let Some(peer) = peer {
            peer.session.queue_blocks(piece_idx, num_blocks);
        }
    }

    pub fn peer_has_piece(&self, peer: &Peer, piece_idx: u32) -> bool {
        if let Some(peer) = self.find_peer(peer) {
            peer.piece_lookup.piece_exists(piece_idx)
        } else {
            false
        }
    }
}
