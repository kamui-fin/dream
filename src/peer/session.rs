use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use log::{error, info};
use serde::{Deserialize, Deserializer};
use serde_bytes::ByteBuf;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    time::{timeout, Instant},
};
use tokio_util::codec::Framed;

use crate::{
    msg::{BitTorrentCodec, InternalMessage, InternalMessagePayload, Message, MessageType},
    piece::{BitField, BLOCK_SIZE},
    utils::slice_to_u32_msb,
};

use super::{PipelineEntry, DREAM_ID, HANDSHAKE_LEN, PROTOCOL_STR, PROTOCOL_STR_LEN};

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
                    .send(InternalMessage {
                        origin: conn_info,
                        payload: InternalMessagePayload::MigrateWork,
                    })
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

pub struct PeerSession {
    framed: Framed<TcpStream, BitTorrentCodec>,
    forwarder: mpsc::Sender<InternalMessage>,
    peer: ConnectionInfo,
    request_tracker: HashMap<PipelineEntry, Instant>,
}

impl PeerSession {
    pub async fn new_session(
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

    pub async fn from_stream(
        mut conn: TcpStream,
        forwarder: mpsc::Sender<InternalMessage>,
        peer: ConnectionInfo,
        info_hash: &[u8; 20],
        bitfield: BitField,
    ) -> anyhow::Result<Self> {
        Self::send_handshake(&mut conn, info_hash).await?;
        let mut framed = Framed::new(conn, BitTorrentCodec);
        Self::send_bitfield(&mut framed, bitfield).await?;
        Ok(Self {
            framed,
            forwarder,
            peer,
            request_tracker: HashMap::new(),
        })
    }

    async fn send_handshake(conn: &mut TcpStream, info_hash: &[u8; 20]) -> anyhow::Result<()> {
        let mut handshake = [0u8; HANDSHAKE_LEN];
        handshake[0] = PROTOCOL_STR_LEN as u8;
        handshake[1..20].copy_from_slice(PROTOCOL_STR);
        handshake[28..48].copy_from_slice(info_hash);
        handshake[48..68].copy_from_slice(DREAM_ID.as_bytes());
        conn.write_all(&handshake).await?;
        Ok(())
    }

    async fn send_bitfield(
        framed: &mut Framed<TcpStream, BitTorrentCodec>,
        bitfield: BitField,
    ) -> anyhow::Result<()> {
        let bitfield_msg = MessageType::Bitfield.build_msg(bitfield.0);
        framed.send(bitfield_msg).await?;
        Ok(())
    }

    pub async fn peer_handshake(
        peer: ConnectionInfo,
        info_hash: &[u8; 20],
    ) -> anyhow::Result<TcpStream> {
        let mut stream = Self::connect_to_peer(&peer).await?;
        Self::send_handshake(&mut stream, info_hash).await?;
        Self::verify_handshake(&mut stream, info_hash).await?;
        Ok(stream)
    }

    async fn connect_to_peer(peer: &ConnectionInfo) -> anyhow::Result<TcpStream> {
        let connect_timeout = Duration::from_secs(3);
        match timeout(connect_timeout, TcpStream::connect(peer.addr())).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(e)) => {
                error!("Failed to connect to {:?}: {}", peer, e);
                Err(anyhow!(e))
            }
            Err(_) => {
                error!("Connection attempt timed out with {:?}", peer);
                Err(anyhow!("Connection timed out"))
            }
        }
    }

    async fn verify_handshake(stream: &mut TcpStream, info_hash: &[u8; 20]) -> anyhow::Result<()> {
        let mut res = [0u8; HANDSHAKE_LEN];
        stream.read_exact(&mut res).await?;
        if &res[0..20] == PROTOCOL_STR && &res[28..48] == info_hash {
            Ok(())
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
                                    let speed_updater = InternalMessage {
                                        origin: self.peer.clone(),
                                        payload: InternalMessagePayload::UpdateSpeed{speed: new_speed.as_secs_f32()}
                                    };
                                    self.forwarder.send(speed_updater).await.unwrap();
                                    info!("Block {:#?} came in with speed {:#?}", corresponding_entry.block_id, new_speed);
                                }
                                // send normal piece message
                                let msg = InternalMessage {
                                    payload: (InternalMessagePayload::ForwardMessage {msg}),
                                    origin: (self.peer.clone())
                                };

                                self.forwarder.send(msg).await.unwrap();
                            }
                        }
                        Err(e) => {
                            error!(
                                "Encountered malformed data from peer {:#?}: {:#?}",
                                self.peer,
                                e
                            );
                            let _ = self.framed.close().await;
                            let close_msg = InternalMessage{origin: self.peer.clone(), payload: InternalMessagePayload::CloseConnection};
                            self.forwarder.send(close_msg).await.unwrap();
                            return;
                        }
                    }
                }
                Some(msg) = send_jobs.recv() => {
                    if let Err(_) = self.send_message(msg.0.clone(), msg.1).await {
                        let close_msg = InternalMessage{origin: self.peer.clone(), payload: InternalMessagePayload::CloseConnection};
                        self.forwarder.send(close_msg).await.unwrap();
                        return;
                    }
                }
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
