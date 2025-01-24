use std::{collections::HashMap, ops::Range, sync::Arc, time::Duration};

use log::{error, info, warn};
use rand::{rngs::OsRng, Rng};
use tokio::{
    net::TcpStream,
    sync::{
        mpsc::{self, Sender},
        Mutex,
    },
    time::sleep,
};

use super::{
    session::{ConnectionInfo, PeerSession, RequestTracker},
    stats::{PeerStats, STATS_WINDOW_SEC},
    PipelineEntry, RemotePeer,
};
use crate::{
    bittorrent::TorrentState,
    msg::{InternalMessage, InternalMessagePayload, Message, MessageType},
    peer::MAX_PIPELINE_SIZE,
    piece::{BitField, PieceStore, BLOCK_SIZE},
    tracker::TrackerResponse,
    utils::{slice_to_u32_msb, Notifier},
};

pub struct GlobalStats{
    pub num_pieces_pending: u32,
    pub num_pieces_downloaded: u32,
}

pub struct PeerManager {
    pub peers: Vec<RemotePeer>,
    piece_store: Arc<Mutex<PieceStore>>,

    // channels
    send_channels: HashMap<ConnectionInfo, Sender<(Message, Option<PipelineEntry>)>>,
    msg_tx: Sender<InternalMessage>, // for adding new peers dynamically

    notify_finished_piece: Arc<Notifier>,

    pub request_tracker: RequestTracker,
    pub stats_tracker: Arc<std::sync::Mutex<HashMap<ConnectionInfo, PeerStats>>>,
    pub global_stats: GlobalStats
}

impl PeerManager {
    /// Construct a peer manager given a response from the tracker (50 peers)
    /// For each peer, initialize a peer session and start listening for messages
    pub async fn connect_peers(
        peers_response: TrackerResponse,
        piece_store: Arc<Mutex<PieceStore>>,
        info_hash: &[u8; 20],
        num_pieces: u32,
        msg_tx: Sender<InternalMessage>,
        notify_finished_piece: Arc<Notifier>,
    ) -> PeerManager {
        let request_tracker = RequestTracker::new(msg_tx.clone());
        let mut send_channels: HashMap<ConnectionInfo, Sender<(Message, Option<PipelineEntry>)>> =
            HashMap::new();

        let peers: Vec<_> = peers_response
            .peers
            .into_iter()
            .map(|peer| RemotePeer::from_peer(peer, num_pieces))
            .collect();

        let stats_tracker = Arc::new(std::sync::Mutex::new(HashMap::new()));

        for remote_peer in &peers {
            Self::init_peer(
                remote_peer,
                info_hash,
                msg_tx.clone(),
                stats_tracker.clone(),
                &mut send_channels,
            )
            .await;
        }

        PeerManager {
            peers,
            send_channels,
            msg_tx,
            piece_store,
            notify_finished_piece,
            request_tracker,
            stats_tracker,
            global_stats: GlobalStats{num_pieces_pending: 0, num_pieces_downloaded: 0}
        }
    }

    async fn init_peer(
        remote_peer: &RemotePeer,
        info_hash: &[u8; 20],
        msg_tx: Sender<InternalMessage>,
        stats_tracker: Arc<std::sync::Mutex<HashMap<ConnectionInfo, PeerStats>>>,
        send_channels: &mut HashMap<ConnectionInfo, Sender<(Message, Option<PipelineEntry>)>>,
    ) {
        let conn_info = remote_peer.conn_info.clone();
        let tx_clone = msg_tx.clone();

        // initialize each peer's stats
        stats_tracker
            .lock()
            .unwrap()
            .insert(conn_info.clone(), PeerStats::init_stats());
        let stats_tracker_clone = stats_tracker.clone();
        let target_conn_info = conn_info.clone();

        // create a task per peer that waits 5 seconds then updates the historical average of that peer
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(STATS_WINDOW_SEC as u64)).await;
                let mut tracker_guard = stats_tracker_clone.lock().unwrap();
                if tracker_guard.contains_key(&target_conn_info) {
                    let curr_stats = tracker_guard.get_mut(&target_conn_info).unwrap();

                    // replace previous window with current window data and start a new window
                    curr_stats.download.update_overalls();
                    curr_stats.upload.update_overalls();
                    info!(
                        "5 seconds up, peer {:#?} has new DOWNLOAD kbps of {:#?}",
                        target_conn_info, curr_stats.download.total_avg_kbps
                    );
                    info!(
                        "5 seconds up, peer {:#?} has new UPLOAD kbps of {:#?}",
                        target_conn_info, curr_stats.upload.total_avg_kbps
                    );
                } else {
                    break;
                }
            }
        });

        Self::init_session(send_channels, tx_clone, conn_info, *info_hash, None, None).await;
    }

    /// As we announce to the tracker ever 30 minutes, we get a new list of peers. Sync PeerManager to the new list, connecting to any new peers
    pub async fn sync_peers(&mut self, peers: TrackerResponse) {
        for peer in peers.peers {
            if self.find_peer(&peer).is_none() {
                self.create_peer(peer.clone()).await;

                let tx_clone = self.msg_tx.clone();
                let info_hash_clone = self.piece_store.lock().await.meta_file.get_info_hash();

                Self::init_session(
                    &mut self.send_channels,
                    tx_clone,
                    peer,
                    info_hash_clone,
                    None,
                    None,
                )
                .await;
            }
        }
    }

    /// Wrapper over init_session to build from tcp stream
    pub async fn init_session_from_stream(
        &mut self,
        stream: TcpStream,
        conn_info: ConnectionInfo,
        info_hash: [u8; 20],
    ) {
        let bitfield = self.piece_store.lock().await.get_status_bitfield().clone();
        let tx_clone = self.msg_tx.clone();

        Self::init_session(
            &mut self.send_channels,
            tx_clone,
            conn_info.clone(),
            info_hash,
            Some(bitfield),
            Some(stream),
        )
        .await;
    }

    /// Either create a new peer session from a stream or from scratch
    async fn init_session(
        send_channels: &mut HashMap<ConnectionInfo, Sender<(Message, Option<PipelineEntry>)>>,
        tx_clone: Sender<InternalMessage>,
        conn_info: ConnectionInfo,
        info_hash: [u8; 20],
        bitfield: Option<BitField>,
        stream: Option<TcpStream>,
    ) {
        let (send_msg, recv_msg) = mpsc::channel(2000);
        send_channels.insert(conn_info.clone(), send_msg);

        tokio::spawn(async move {
            let session = match stream {
                Some(stream) => {
                    PeerSession::from_stream(
                        stream,
                        tx_clone,
                        conn_info,
                        &info_hash,
                        bitfield.unwrap(),
                    )
                    .await
                }
                None => PeerSession::new_session(tx_clone, conn_info, &info_hash).await,
            };

            if let Ok(mut session) = session {
                session.start_listening(recv_msg).await;
            }
        });
    }

    /* Basic peer CRUD-related logic */

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

    fn get_peers_with_piece(
        &mut self,
        conn_info: &ConnectionInfo,
        piece_id: u32,
    ) -> Vec<ConnectionInfo> {
        self.peers
            .iter_mut()
            .filter(|p| p.piece_lookup.piece_exists(piece_id))
            .filter(|p| p.conn_info != *conn_info && !p.peer_choking)
            .map(|p| p.conn_info.clone())
            .collect()
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

    pub async fn create_peer(&mut self, addr: ConnectionInfo) {
        let num_pieces = self.piece_store.lock().await.num_pieces;
        if !self.peers.iter().any(|p| p.conn_info == addr) {
            let new_peer = RemotePeer::from_peer(addr, num_pieces);
            self.peers.push(new_peer);
        }
    }

    pub async fn remove_peer(&mut self, conn_info: &ConnectionInfo) {
        let peer = self.find_peer(conn_info);
        if let Some(peer) = peer {
            error!(
                "Removing peer {:#?} with pipeline: {:#?} and buffer: {:#?}",
                conn_info, peer.pipeline, peer.buffer
            );
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

    /* Managing peer pipelines and distributing work evenly */

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

    async fn pipeline_enqueue(&mut self, entry: PipelineEntry, conn_info: &ConnectionInfo) {
        let peer = self.find_peer_mut(conn_info);
        if let Some(peer) = peer {
            peer.pipeline.push(entry.clone());
            self.request_block(entry, conn_info).await;
        } else {
            warn!("[PIPELINE_ADD] Peer {:#?} not found", conn_info);
        }
    }

    pub async fn redistribute_work(
        &mut self,
        conn_info: &ConnectionInfo,
        mut work: Vec<PipelineEntry>,
    ) {
        if work.is_empty() {
            return;
        }

        let peers_with_piece = self.get_peers_with_piece(conn_info, work[0].piece_id);

        if peers_with_piece.is_empty() {
            panic!(); // if there is not a single peer with ALL pieces, this breaks our assumption!
        }

        let blocks_per_peer = std::cmp::max(1, (work.len() / peers_with_piece.len()) as u32);
        let batch_request =
            self.distribute_work_to_peers(peers_with_piece, &mut work, blocks_per_peer);

        self.request_blocks(batch_request).await;
    }

    fn distribute_work_to_peers(
        &mut self,
        peers_with_piece: Vec<ConnectionInfo>,
        work: &mut Vec<PipelineEntry>,
        blocks_per_peer: u32,
    ) -> Vec<(PipelineEntry, ConnectionInfo)> {
        let mut batch_request = vec![];

        info!(
            "Redistributing work of {} to {} peers ({} each)",
            work.len(),
            peers_with_piece.len(),
            blocks_per_peer
        );

        for conn_info in peers_with_piece {
            let peer = self.find_peer_mut(&conn_info).unwrap();
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

        batch_request
    }

    /* Sending specific BitTorrent messages to peers */

    pub async fn broadcast_have(&mut self, piece_idx: u32) {
        let conns: Vec<_> = self.send_channels.keys().cloned().collect();
        for connection in conns {
            info!("Sending HAVE to {:#?}", connection);
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

    async fn request_blocks(&mut self, batch_request: Vec<(PipelineEntry, ConnectionInfo)>) {
        for (entry, conn) in batch_request {
            self.request_block(entry, &conn).await;
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

    pub async fn send_message(&mut self, conn_info: &ConnectionInfo, msg: Message) {

        if msg.msg_type == MessageType::Request{
            self.global_stats.num_pieces_pending += 1;
        }

        if let Err(_) = self.send_channels[conn_info].send((msg, None)).await {
            error!("Lost connection to peer {:#?}", conn_info);
            self.remove_peer(conn_info).await;
        }
    }

    /* Choking algorithm */

    pub async fn recompute_choke_list(&mut self, _torrent_state: TorrentState) {
        // move optimistic unchoke peer to the end so we don't count it in our list of top peers
        let optimistic_unchoke = self
            .peers
            .iter()
            .position(|item| item.optimistic_unchoke)
            .map(|pos| self.peers.remove(pos));

        // sort peers by average speed depending on torrent state
        // self.peers.sort_by_key(|p| match torrent_state {
        //     TorrentState::Seeder => {
        //         self.stats_tracker.lock().unwrap()[&p.conn_info].upload_total_avg_kbps
        //     }
        //     TorrentState::Leecher => {
        //         self.stats_tracker.lock().unwrap()[&p.conn_info].download_total_avg_kbps
        //     }
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

    /* Main bittorrent protocol handler */

    pub async fn handle_msg(&mut self, fw_msg: &InternalMessage) {
        let conn_info = &fw_msg.origin;
        let peer = self.find_peer_mut(conn_info);
        if peer.is_none() {
            warn!("Peer {:#?} not found", &conn_info);
            return;
        }

        let peer = peer.unwrap();
        match &fw_msg.payload {
            InternalMessagePayload::ForwardMessage { msg } => {
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
                            self.notify_finished_piece.notify_one();
                            self.global_stats.num_pieces_pending -= 1;
                            self.global_stats.num_pieces_downloaded+=1;
                        }
                    }
                    MessageType::Cancel => {
                        // TODO:
                    }
                    MessageType::Port => {
                        // The listen port is the port this peer's DHT node is listening on. This peer should be inserted in the local routing table (if DHT tracker is supported).
                    }
                }
            }
            InternalMessagePayload::CloseConnection => {
                self.remove_peer(conn_info).await;
            }

            InternalMessagePayload::MigrateWork => {
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

            InternalMessagePayload::UpdateDownloadSpeed { speed } => {
                // update speed to the corresponding peer
                if let Some(curr_stats) = self
                    .stats_tracker
                    .lock()
                    .unwrap()
                    .get_mut(&conn_info.clone())
                {
                    curr_stats.download.add_new_speed(speed);
                    info!("Added new speed {:#?} to peer {:#?}", speed, conn_info);
                } else {
                    warn!("Invalid update message found");
                }
            }
            InternalMessagePayload::UpdateUploadSpeed { speed } => {
                // update speed to the corresponding peer
                if let Some(curr_stats) = self
                    .stats_tracker
                    .lock()
                    .unwrap()
                    .get_mut(&conn_info.clone())
                {
                    curr_stats.upload.add_new_speed(speed);
                    info!("Added new speed {:#?} to peer {:#?}", speed, conn_info);
                } else {
                    warn!("Invalid update message found");
                }
            }
        }
    }
}
