use std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Duration};

use log::{error, info};
use tokio::{
    sync::{
        mpsc::{self, Receiver},
        Mutex,
    },
    time::{sleep, Instant},
};

use crate::{
    metafile::Metafile,
    msg::{DataReady, InternalMessage, ServerMsg},
    peer::{manager::PeerManager, session::ConnectionInfo},
    piece::{PieceStore, BLOCK_SIZE},
    tracker::{self, TrackerResponse},
    utils::{self, Notifier},
};

pub enum TorrentState {
    Seeder,
    Leecher,
}

pub struct BitTorrent {
    pub meta_file: Metafile,
    pub piece_store: Arc<Mutex<PieceStore>>, // references meta_file
    pub peer_manager: Arc<Mutex<PeerManager>>,
    notify_pipelines_empty: Arc<Notifier>,
    main_piece_queue: VecDeque<usize>,
}

impl BitTorrent {
    pub async fn from_torrent_file(meta_file: Metafile, output_dir: &str) -> anyhow::Result<Self> {
        info!("Parsed metafile: {:#?}", meta_file);

        let peers = Self::fetch_peers(&meta_file).await?;
        let piece_store = Self::initialize_piece_store(&meta_file, output_dir);
        let info_hash = piece_store.meta_file.get_info_hash();
        let num_pieces = piece_store.meta_file.get_num_pieces();
        let piece_store = Arc::new(Mutex::new(piece_store));

        let (msg_tx, msg_rx) = mpsc::channel(2000);
        let notify_pipelines_empty = Arc::new(Notifier::new());

        let peer_manager = Self::connect_to_peers(
            peers,
            piece_store.clone(),
            &info_hash,
            num_pieces,
            msg_tx,
            notify_pipelines_empty.clone(),
        )
        .await;

        let peer_manager = Arc::new(Mutex::new(peer_manager));

        Self::spawn_choke_manager(peer_manager.clone(), piece_store.clone());
        Self::spawn_peer_sync(peer_manager.clone(), piece_store.clone());
        Self::spawn_message_listener(msg_rx, peer_manager.clone());

        Ok(Self {
            meta_file,
            piece_store,
            peer_manager,
            notify_pipelines_empty,
            main_piece_queue: VecDeque::new(),
        })
    }

    async fn fetch_peers(meta_file: &Metafile) -> anyhow::Result<TrackerResponse> {
        let peers = tracker::get_peers_from_tracker(
            &meta_file.get_announce(),
            tracker::TrackerRequest::new(meta_file),
        )?;
        info!("Tracker has found {} peers", peers.peers.len());
        Ok(peers)
    }

    fn initialize_piece_store(meta_file: &Metafile, output_dir: &str) -> PieceStore {
        PieceStore::new(meta_file.clone(), PathBuf::from(output_dir))
    }

    async fn connect_to_peers(
        peers: TrackerResponse,
        piece_store: Arc<Mutex<PieceStore>>,
        info_hash: &[u8; 20],
        num_pieces: u32,
        msg_tx: mpsc::Sender<InternalMessage>,
        notify_pipelines_empty: Arc<Notifier>,
    ) -> PeerManager {
        let peer_manager = PeerManager::connect_peers(
            peers,
            piece_store,
            info_hash,
            num_pieces,
            msg_tx,
            notify_pipelines_empty,
        )
        .await;

        info!(
            "Connected successfully to {:#?} peers.",
            peer_manager.peers.len()
        );

        peer_manager
    }

    fn spawn_choke_manager(
        peer_manager: Arc<Mutex<PeerManager>>,
        piece_store: Arc<Mutex<PieceStore>>,
    ) {
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(10)).await;
                Self::recompute_choke_list(peer_manager.clone(), piece_store.clone()).await;

                sleep(Duration::from_secs(10)).await;
                Self::recompute_choke_list(peer_manager.clone(), piece_store.clone()).await;

                sleep(Duration::from_secs(10)).await;
                peer_manager.lock().await.optimistic_unchoke().await;
                Self::recompute_choke_list(peer_manager.clone(), piece_store.clone()).await;
            }
        });
    }

    async fn recompute_choke_list(
        peer_manager: Arc<Mutex<PeerManager>>,
        piece_store: Arc<Mutex<PieceStore>>,
    ) {
        let state = Self::get_torrent_state(piece_store.clone()).await;
        peer_manager.lock().await.recompute_choke_list(state).await;
    }

    fn spawn_peer_sync(peer_manager: Arc<Mutex<PeerManager>>, piece_store: Arc<Mutex<PieceStore>>) {
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(30 * 60)).await;

                let pt_lock = piece_store.lock().await;
                let peers = tracker::get_peers_from_tracker(
                    &pt_lock.meta_file.get_announce(),
                    tracker::TrackerRequest::new(&pt_lock.meta_file),
                );

                if let Ok(peers) = peers {
                    peer_manager.lock().await.sync_peers(peers).await;
                }
            }
        });
    }

    fn spawn_message_listener(
        msg_rx: mpsc::Receiver<InternalMessage>,
        peer_manager: Arc<Mutex<PeerManager>>,
    ) {
        tokio::spawn(async move {
            Self::listen(msg_rx, peer_manager).await;
        });
    }

    pub async fn get_torrent_state(piece_store: Arc<Mutex<PieceStore>>) -> TorrentState {
        if piece_store.lock().await.get_status_bitfield().has_all() {
            TorrentState::Seeder
        } else {
            TorrentState::Leecher
        }
    }

    pub async fn listen(
        mut msg_rx: Receiver<InternalMessage>,
        peer_manager: Arc<Mutex<PeerManager>>,
    ) {
        loop {
            let int_msg = msg_rx.recv().await;
            if let Some(int_msg) = &int_msg {
                peer_manager.lock().await.handle_msg(int_msg).await;
            }
        }
    }

    pub async fn download_piece(&mut self, piece_idx: usize) -> anyhow::Result<Vec<u8>> {
        let start = Instant::now();
        let piece_size = self.meta_file.get_piece_len(piece_idx);
        let num_blocks = (((piece_size as u32) / BLOCK_SIZE) as f32).ceil() as u32;

        let mut candidates = self
            .peer_manager
            .lock()
            .await
            .with_piece(piece_idx as u32)
            .await;
        info!("Found {} peers with piece {piece_idx}", candidates.len());

        self.show_interest_in_peers(&mut candidates).await;

        let mut candidates_unchoked = self.get_unchoked_candidates(&candidates).await;
        while candidates_unchoked.is_empty() {
            info!("Waiting for peer to unchoke us");
            tokio::time::sleep(Duration::from_secs(5)).await;
            candidates = self
                .peer_manager
                .lock()
                .await
                .with_piece(piece_idx as u32)
                .await;
            candidates_unchoked = self.get_unchoked_candidates(&candidates).await;
        }

        self.distribute_blocks(&candidates_unchoked, piece_idx, num_blocks)
            .await;

        info!("Waiting for all peers to finish work");
        self.notify_pipelines_empty.wait_for_notification().await;
        info!("Done waiting for piece DL");

        let piece_data = self.verify_and_persist_piece(piece_idx).await?;

        println!(
            "Piece {piece_idx} successfully downloaded in {:?}",
            start.elapsed()
        );

        Ok(piece_data)
    }

    async fn show_interest_in_peers(&mut self, candidates: &mut Vec<ConnectionInfo>) {
        for candidate in candidates.iter_mut() {
            self.peer_manager
                .lock()
                .await
                .show_interest_in_peer(candidate)
                .await;
        }
    }

    async fn get_unchoked_candidates(&self, candidates: &[ConnectionInfo]) -> Vec<ConnectionInfo> {
        let mut candidates_unchoked = Vec::new();
        for candidate in candidates {
            let guard = self.peer_manager.lock().await;
            let peer = guard.find_peer(candidate);
            if let Some(peer) = peer {
                if !peer.peer_choking {
                    candidates_unchoked.push(candidate.clone());
                }
            }
        }
        info!(
            "{} matched candidates that haven't choked us",
            candidates_unchoked.len()
        );
        candidates_unchoked
    }

    async fn distribute_blocks(
        &mut self,
        candidates_unchoked: &[ConnectionInfo],
        piece_idx: usize,
        num_blocks: u32,
    ) {
        let blocks_per_peer = std::cmp::max(num_blocks / candidates_unchoked.len() as u32, 1);
        info!(
            "Distributed {} blocks per peer. Total # of blocks: {}",
            blocks_per_peer, num_blocks
        );

        for (i, peer) in candidates_unchoked.iter().enumerate() {
            let start = i as u32 * blocks_per_peer;
            let end = if i == candidates_unchoked.len() - 1 {
                num_blocks
            } else {
                start + blocks_per_peer
            };

            if end > num_blocks {
                break;
            }
            self.peer_manager
                .lock()
                .await
                .queue_blocks(peer, piece_idx as u32, start..end)
                .await;
        }
    }

    async fn verify_and_persist_piece(&mut self, piece_idx: usize) -> anyhow::Result<Vec<u8>> {
        let mut store = self.piece_store.lock().await;
        let piece = &store.pieces[piece_idx];
        let piece_data = piece.buffer.clone();
        if piece.verify_hash() {
            info!("Hash verified");
            store.persist(piece_idx)?;
            store.reset_piece(piece_idx);
        } else {
            error!("Piece {piece_idx} hash mismatch!!");
            panic!();
        }

        info!("reset piece");
        store.reset_piece(piece_idx);
        info!("reset shi");
        self.peer_manager.lock().await.request_tracker.reset();
        info!("distributed have");
        self.peer_manager
            .lock()
            .await
            .broadcast_have(piece_idx as u32)
            .await;

        Ok(piece_data)
    }
}
