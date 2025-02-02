use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use log::{error, info, warn};
use serde::{Deserialize, Deserializer};
use serde_bytes::ByteBuf;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    time::{sleep, timeout, Instant},
};
use tokio_util::codec::Framed;

use super::{PipelineEntry, DREAM_ID, HANDSHAKE_LEN, PROTOCOL_STR_LEN};
use crate::{
    config::BLOCK_SIZE,
    msg::{BitTorrentCodec, InternalMessage, InternalMessagePayload, Message, MessageType},
    piece::BitField,
    utils::slice_to_u32_msb,
};

const KEEPALIVE_RECEIVER_TIMEOUT: u64 = 60;
const KEEPALIVE_SENDER_TIMEOUT: u64 = 30;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConnectionInfo {
    pub ip: Ipv4Addr,
    pub port: u16,
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
    speed_tracker: HashMap<PipelineEntry, Instant>,
    request_history: HashSet<PipelineEntry>,
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
            speed_tracker: HashMap::new(),
            request_history: HashSet::new(),
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
            speed_tracker: HashMap::new(),
            request_history: HashSet::new(),
        })
    }

    async fn send_handshake(
        conn: &mut TcpStream,
        info_hash: &[u8; 20],
    ) -> anyhow::Result<[u8; HANDSHAKE_LEN]> {
        let mut handshake = [0u8; HANDSHAKE_LEN];
        handshake[0] = PROTOCOL_STR_LEN as u8;
        handshake[1..20].copy_from_slice(b"BitTorrent protocol");
        handshake[28..48].copy_from_slice(info_hash);
        handshake[48..68].copy_from_slice(DREAM_ID.as_bytes());
        conn.write_all(&handshake).await?;
        Ok(handshake)
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
        let handshake = Self::send_handshake(&mut stream, info_hash).await?;
        Self::verify_handshake(&mut stream, handshake).await?;
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

    async fn verify_handshake(
        stream: &mut TcpStream,
        handshake: [u8; HANDSHAKE_LEN],
    ) -> anyhow::Result<()> {
        let mut res = [0u8; HANDSHAKE_LEN];
        stream.read_exact(&mut res).await?;
        if res[0..20] == handshake[0..20] && res[28..48] == handshake[28..48] {
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

        PipelineEntry { piece_id, block_id }
    }

    pub async fn start_listening(
        &mut self,
        mut send_jobs: mpsc::Receiver<(Message, Option<PipelineEntry>)>,
    ) {
        let keepalive_receiver = sleep(Duration::from_secs(KEEPALIVE_RECEIVER_TIMEOUT));
        tokio::pin!(keepalive_receiver);

        let keepalive_sender = sleep(Duration::from_secs(KEEPALIVE_SENDER_TIMEOUT));
        tokio::pin!(keepalive_sender);

        loop {
            tokio::select! {
                _ = &mut keepalive_sender => {
                    warn!("No message sent in the past {} secs, sending keepalive to peer: {:#?}", KEEPALIVE_SENDER_TIMEOUT, self.peer);
                    let keepalive_msg =  MessageType::KeepAlive.build_msg(Vec::new());

                    if let Err(_) = self.send_message(keepalive_msg.clone(), None).await {
                        warn!("Keepalive send failed, closing connection to peeer {:#?}", self.peer);
                        let close_msg = InternalMessage { origin: self.peer.clone(), payload: InternalMessagePayload::CloseConnection };
                        self.framed.close().await.unwrap();
                        self.forwarder.send(close_msg).await.unwrap();
                        return;
                    }
                    info!("PeerSession sent {:?} successfully", keepalive_msg);
                    keepalive_sender.as_mut().reset(Instant::now() + Duration::from_secs(KEEPALIVE_SENDER_TIMEOUT));
                    info!("Timer reset for keepalive sender");
                }
                _ = &mut keepalive_receiver => {
                    warn!("No message received in the past {} seconds, closing connection to peer {:#?}", KEEPALIVE_RECEIVER_TIMEOUT, self.peer);
                    self.framed.close().await.unwrap();
                    let close_msg = InternalMessage { origin: self.peer.clone(), payload: InternalMessagePayload::CloseConnection };
                    self.forwarder.send(close_msg).await.unwrap();
                    return;
                }
                Some(msg) = self.framed.next() => {
                    // reset receiver
                    keepalive_receiver.as_mut().reset(Instant::now() + Duration::from_secs(KEEPALIVE_RECEIVER_TIMEOUT));
                    match msg {
                        Ok(msg) => {
                            if msg.msg_type == MessageType::KeepAlive {
                                continue;
                            } else {
                                if msg.msg_type == MessageType::Request{
                                    warn!("Request came in");
                                }
                                if msg.msg_type == MessageType::Piece {
                                    // get the pipeline entry to track request
                                    let corresponding_entry = Self::get_pipeline_entry(msg.clone());


                                    if let Some(speed) = self.speed_tracker.get(&corresponding_entry) {
                                        let new_speed = Instant::now() - *speed;

                                        // remove the entry once the request has been fulfilled
                                        self.speed_tracker.remove(&corresponding_entry);

                                        // send a speed update message to add the new speed to the peer's stats
                                        let speed_updater = InternalMessage {
                                            origin: self.peer.clone(),
                                            payload: InternalMessagePayload::UpdateDownloadSpeed{speed: new_speed.as_secs_f32()}
                                        };
                                        self.forwarder.send(speed_updater).await.unwrap();
                                        info!("Block {:#?} came in with speed {:#?}", corresponding_entry.block_id, new_speed);
                                    }
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
                            let _ = self.close().await;
                            return;
                        }
                    }
                }
                Some(msg) = send_jobs.recv() => {
                    if let Err(_) = self.send_message(msg.0.clone(), msg.1).await {
                        let _ = self.close().await;
                        return;
                    }
                    // reset sender since we sent a different msg
                    keepalive_sender.as_mut().reset(Instant::now() + Duration::from_secs(KEEPALIVE_SENDER_TIMEOUT));
                }
            }
        }
    }

    pub async fn close(&mut self) -> anyhow::Result<()> {
        let _ = self.framed.close().await;
        let close_msg = InternalMessage {
            origin: self.peer.clone(),
            payload: InternalMessagePayload::CloseConnection,
        };
        self.forwarder.send(close_msg).await.unwrap();
        Ok(())
    }

    pub async fn send_message(
        &mut self,
        msg: Message,
        request_entry: Option<PipelineEntry>,
    ) -> anyhow::Result<()> {
        if let Some(entry) = request_entry {
            if self.request_history.contains(&entry) {
                warn!("Duplicate request for {:#?}", entry);
                return Ok(());
            }
            // info!("Storing request for {:#?}", entry);
            self.speed_tracker.insert(entry.clone(), Instant::now());
            self.request_history.insert(entry);
        }

        let start_time = Instant::now();
        let is_block = msg.msg_type == MessageType::Piece;
        let result = self.framed.send(msg).await;
        let total_time = Instant::now() - start_time;

        if is_block {
            let upload_speed_update = InternalMessage {
                origin: self.peer.clone(),
                payload: InternalMessagePayload::UpdateUploadSpeed {
                    speed: total_time.as_secs_f32(),
                },
            };
            warn!("Block sent with speed {:#?}", total_time);
            self.forwarder.send(upload_speed_update).await.unwrap();
        }
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
