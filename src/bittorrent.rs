use std::{path::PathBuf, sync::Arc, time::Duration};

use log::{error, info};
use tokio::{
    sync::{
        mpsc::{self, Receiver},
        Mutex,
    },
    time::{sleep, Instant},
};

use crate::{
    config::CONFIG,
    metafile::Metafile,
    msg::InternalMessage,
    peer::{manager::PeerManager, session::ConnectionInfo},
    piece::PieceStore,
    tracker::{self},
    utils::Notifier,
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
}

impl BitTorrent {
    pub async fn from_torrent_file(
        meta_file: Metafile,
        output_dir: PathBuf,
    ) -> anyhow::Result<Self> {
        let piece_store = Self::initialize_piece_store(&meta_file, output_dir.clone());
        let info_hash = piece_store.meta_file.get_info_hash();
        let num_pieces = piece_store.meta_file.get_num_pieces();
        let piece_store = Arc::new(Mutex::new(piece_store));

        let (msg_tx, msg_rx) = mpsc::channel(2000);

        let notify_pipelines_empty = Arc::new(Notifier::new());

        let peers = Self::fetch_peers(&meta_file).await?;
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
        })
    }

    async fn fetch_peers(meta_file: &Metafile) -> anyhow::Result<Vec<ConnectionInfo>> {
        if CONFIG.dht.always_use_dht {
            if !CONFIG.dht.enabled {
                return Err(anyhow::anyhow!("DHT is not enabled"));
            } else {
                return tracker::get_peers_from_dht(meta_file.get_info_hash());
            }
        }

        let peers = if let Some(tracker_url) = &meta_file.get_announce() {
            tracker::get_peers_from_tracker(tracker_url, tracker::TrackerRequest::new(meta_file))?
        } else if CONFIG.dht.enabled {
            tracker::get_peers_from_dht(meta_file.get_info_hash())?
        } else {
            return Err(anyhow::anyhow!("No tracker found and DHT not enabled"));
        };

        Ok(peers)
    }

    fn initialize_piece_store(meta_file: &Metafile, output_dir: PathBuf) -> PieceStore {
        PieceStore::new(meta_file.clone(), output_dir)
    }

    async fn connect_to_peers(
        peers: Vec<ConnectionInfo>,
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

                let peers = Self::fetch_peers(&pt_lock.meta_file).await;

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
            if let Some(int_msg) = msg_rx.recv().await {
                info!("Received {:#?}", int_msg.payload);
                peer_manager.lock().await.handle_msg(&int_msg).await;
            }
        }
    }

    pub async fn download_piece(&mut self, piece_idx: usize) -> anyhow::Result<Vec<u8>> {
        let start = Instant::now();
        let piece_size = self.meta_file.get_piece_len(piece_idx);
        let mut num_blocks = (((piece_size as u32) / CONFIG.torrent.block_size) as f32).ceil() as u32;

        let is_last_piece = piece_idx == self.meta_file.get_num_pieces() as usize - 1;

        if is_last_piece{
            num_blocks += 1;
        }
        self.peer_manager
            .lock()
            .await
            .init_work_queue(piece_idx, num_blocks)
            .await;

        while self.peer_manager.lock().await.start_work().await.is_err() {
            info!("Waiting for peer to unchoke us");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }

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

        store.reset_piece(piece_idx);
        self.peer_manager.lock().await.request_tracker.reset();
        self.peer_manager
            .lock()
            .await
            .broadcast_have(piece_idx as u32)
            .await;

        Ok(piece_data)
    }
}
