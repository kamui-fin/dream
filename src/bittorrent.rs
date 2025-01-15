use std::{
    collections::VecDeque,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use http_req::tls::Conn;
use log::{error, info};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::{
        mpsc::{self, Receiver},
        Mutex, Notify,
    },
    time::{sleep, Instant},
};

use crate::{
    msg::{InternalMessage, ServerCommand},
    peer::{ConnectionInfo, PeerManager},
    piece::{PieceStore, BLOCK_SIZE},
    tracker::{self, Metafile, TrackerResponse},
    utils::Notifier,
    PORT,
};

pub enum TorrentState {
    Seeder,
    Leecher,
}

pub struct BitTorrent {
    meta_file: Metafile,
    piece_store: Arc<Mutex<PieceStore>>, // references meta_file
    peer_manager: Arc<Mutex<PeerManager>>,
    notify_pipelines_empty: Arc<Notifier>,
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
        let peer_ready_notify = Arc::new(Notify::new());

        let peer_manager = Self::connect_to_peers(
            peers,
            piece_store.clone(),
            &info_hash,
            num_pieces,
            msg_tx,
            peer_ready_notify.clone(),
            notify_pipelines_empty.clone(),
        )
        .await;

        let peer_manager = Arc::new(Mutex::new(peer_manager));

        Self::spawn_choke_manager(peer_manager.clone(), piece_store.clone());
        Self::spawn_peer_sync(peer_manager.clone(), piece_store.clone());
        Self::spawn_message_listener(msg_rx, peer_manager.clone());

        peer_ready_notify.notified().await;

        Ok(Self {
            meta_file,
            piece_store,
            peer_manager,
            notify_pipelines_empty,
        })
    }

    async fn fetch_peers(meta_file: &Metafile) -> anyhow::Result<TrackerResponse> {
        let peers = tracker::get_peers_from_tracker(
            &meta_file.announce,
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
        peer_ready_notify: Arc<Notify>,
        notify_pipelines_empty: Arc<Notifier>,
    ) -> PeerManager {
        let peer_manager = PeerManager::connect_peers(
            peers,
            piece_store,
            info_hash,
            num_pieces,
            msg_tx,
            peer_ready_notify,
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
                let pt_lock = piece_store.lock().await;
                let peers = tracker::get_peers_from_tracker(
                    &pt_lock.meta_file.announce,
                    tracker::TrackerRequest::new(&pt_lock.meta_file),
                );

                if let Ok(peers) = peers {
                    peer_manager.lock().await.sync_peers(peers).await;
                }

                sleep(Duration::from_secs(30 * 60)).await;
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

    pub async fn begin_download(&mut self) -> anyhow::Result<()> {
        let mut main_piece_queue =
            VecDeque::from(self.piece_store.lock().await.get_missing_pieces());

        info!(
            "Started downloaded queue for {} pieces",
            main_piece_queue.len()
        );

        while let Some(piece_idx) = main_piece_queue.pop_front() {
            self.download_piece(piece_idx).await?;
        }

        info!("Concatenating all pieces");
        self.piece_store.lock().await.concat()?;

        Ok(())
    }

    async fn download_piece(&mut self, piece_idx: usize) -> anyhow::Result<()> {
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
            candidates_unchoked = self.get_unchoked_candidates(&candidates).await;
        }

        self.distribute_blocks(&candidates_unchoked, piece_idx, num_blocks)
            .await;

        info!("Waiting for all peers to finish work");
        self.notify_pipelines_empty.wait_for_notification().await;
        info!("Done waiting for piece DL");

        self.verify_and_persist_piece(piece_idx).await?;

        println!(
            "Piece {piece_idx} successfully downloaded in {:?}",
            start.elapsed()
        );

        Ok(())
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

    async fn verify_and_persist_piece(&mut self, piece_idx: usize) -> anyhow::Result<()> {
        let mut store = self.piece_store.lock().await;
        let piece = &store.pieces[piece_idx as usize];
        if piece.verify_hash() {
            info!("Hash verified");
            store.persist(piece_idx)?;
            store.reset_piece(piece_idx);
        } else {
            error!("Piece {piece_idx} hash mismatch!!");
            panic!();
        }

        self.piece_store.lock().await.reset_piece(piece_idx);
        self.peer_manager.lock().await.request_tracker.reset();
        self.peer_manager
            .lock()
            .await
            .broadcast_have(piece_idx as u32)
            .await;

        Ok(())
    }
}

pub struct Engine {
    torrents: Vec<Arc<Mutex<BitTorrent>>>,
    info_hashes: Vec<[u8; 20]>,

    // listen for external commands
    command_rx: mpsc::Receiver<ServerCommand>,
}

impl Engine {
    pub fn new(command_rx: mpsc::Receiver<ServerCommand>) -> Self {
        Self {
            torrents: Vec::new(),
            info_hashes: Vec::new(),
            command_rx,
        }
    }

    pub async fn add_torrent(
        &mut self,
        meta_file: Metafile,
        output_dir: &str,
    ) -> anyhow::Result<()> {
        let info_hash = meta_file.get_info_hash();
        let bt = BitTorrent::from_torrent_file(meta_file, output_dir).await?;
        let bt = Arc::new(Mutex::new(bt));

        self.torrents.push(bt);
        self.info_hashes.push(info_hash);

        Ok(())
    }

    pub async fn start_server(&mut self) -> anyhow::Result<()> {
        info!("Listening on inbound server...");
        let listener = TcpListener::bind(format!("127.0.0.1:{}", PORT)).await?;

        loop {
            tokio::select! {
                Ok((mut socket, addr)) = listener.accept() => {
                    self.handle_new_connection(socket, addr).await?;
                }

                Some(command) = self.command_rx.recv() => {
                    self.handle_command(command).await?;
                }
            }
        }
    }

    // FIXME: replace assert with proper error handling
    async fn handle_new_connection(
        &mut self,
        mut socket: TcpStream,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let peer = ConnectionInfo::from_addr(addr);

        let mut res = [0u8; 68];
        socket.read_exact(&mut res).await?;

        assert_eq!(res[0], 19);
        assert_eq!(&res[1..20], b"BitTorrent protocol");

        let info_hash: [u8; 20] = res[28..48].try_into().unwrap();

        if let Some(index) = self.info_hashes.iter().position(|i| i == &info_hash) {
            let bittorrent = self.torrents[index].lock().await;

            let mut pm_guard = bittorrent.peer_manager.lock().await;
            if pm_guard.find_peer(&peer).is_none() {
                pm_guard.create_peer(peer.clone()).await;
                info!("New connection from {:?}", peer);
                pm_guard.init_session(socket, peer, info_hash).await;
            }
        }
        // If not serving, connection reset

        Ok(())
    }

    async fn handle_command(&mut self, command: ServerCommand) -> anyhow::Result<()> {
        match command {
            ServerCommand::AddExternalTorrent {
                input_path,
                output_dir,
            } => {
                let meta_file = Metafile::parse_torrent_file(&input_path);
                match meta_file {
                    Err(e) => {
                        error!("Failed to parse torrent file: {:?}", e);
                    }
                    Ok(meta_file) => {
                        if let Err(e) = self.add_torrent(meta_file, &output_dir).await {
                            error!("Failed to add torrent: {:?}", e);
                        }
                    }
                }
            }
            ServerCommand::AddVideo {
                input_path,
                output_dir,
            } => {
                let meta_file =
                    Metafile::from_video(&PathBuf::from_str(&input_path).unwrap(), 1024, None);
                if let Err(e) = self.add_torrent(meta_file, &output_dir).await {
                    error!("Failed to add video: {:?}", e);
                }
            }
            ServerCommand::Start(idx) => {
                self.torrents[idx as usize]
                    .lock()
                    .await
                    .begin_download()
                    .await?;
            }
        }

        Ok(())
    }
}
