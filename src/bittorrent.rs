use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use log::{debug, error, info, trace, warn};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

use crate::msg::InternalMessage;
use crate::peer::{self, PipelineEntry, UnchokeMessage};
use crate::tracker::{self, parse_torrent_file};
use crate::utils::Notifier;
use crate::{
    msg::{Message, MessageType},
    peer::{PeerManager, RemotePeer},
    piece::{BitField, PieceStore, BLOCK_SIZE},
    tracker::Metafile,
    utils::slice_to_u32_msb,
};
use crate::{piece, PORT};

pub struct BitTorrent {
    meta_file: Metafile,
    piece_store: Arc<Mutex<PieceStore>>, // references meta_file
    peer_manager: Arc<Mutex<PeerManager>>,
    notify_pipelines_empty: Arc<Notifier>,
}

impl BitTorrent {
    pub async fn from_torrent_file(meta_file: Metafile) -> anyhow::Result<Self> {
        info!("Parsed metafile: {:#?}", meta_file);
        let peers = tracker::get_peers_from_tracker(
            &meta_file.announce,
            tracker::TrackerRequest::new(&meta_file),
        )?;

        info!("Tracker has found {} peers", peers.peers.len());

        let piece_store = PieceStore::new(meta_file.clone());
        let info_hash = piece_store.meta_file.get_info_hash();
        let num_pieces = piece_store.meta_file.get_num_pieces();
        let piece_store = Arc::new(Mutex::new(piece_store));

        // the channel for receiving messages from peer sessions
        let (msg_tx, msg_rx) = mpsc::channel(2000);

        let notify_pipelines_empty = Arc::new(Notifier::new());
        let peer_ready_notify = Arc::new(Notify::new());

        let peer_manager = PeerManager::connect_peers(
            peers,
            piece_store.clone(),
            &info_hash,
            num_pieces,
            msg_tx,
            peer_ready_notify.clone(),
            notify_pipelines_empty.clone(),
        )
        .await;

        info!(
            "Connected successfully to {:#?} peers.",
            peer_manager.peers.len()
        );

        let peer_manager = Arc::new(Mutex::new(peer_manager));
        let pm_clone = peer_manager.clone();

        tokio::spawn(async move {
            Self::listen(msg_rx, pm_clone).await;
        });

        peer_ready_notify.notified().await;

        Ok(Self {
            meta_file,
            piece_store,
            peer_manager,
            notify_pipelines_empty,
        })
    }

    pub async fn listen(
        mut msg_rx: Receiver<InternalMessage>,
        peer_manager: Arc<Mutex<PeerManager>>,
    ) {
        loop {
            let msg = msg_rx.recv().await;
            if let Some(InternalMessage { msg, conn_info }) = &msg {
                info!(
                    "Received msg {:?} from peer {:?}",
                    msg.msg_type,
                    conn_info.addr().ip()
                );
                let start = Instant::now();
                trace!("Acquiring peer lock...");
                let mut peer_guard = peer_manager.lock().await;
                trace!("Got the lock in {:?}", start.elapsed());
                peer_guard.handle_msg(&msg, conn_info).await;
            }
        }
    }

    pub async fn begin_download(&mut self, output_dir: &str) -> anyhow::Result<()> {
        let output_path = Path::new(output_dir);
        if !output_path.exists() {
            std::fs::create_dir(output_path)?;
        }

        let mut main_piece_queue = VecDeque::from(
            (0usize..(self.piece_store.lock().await.num_pieces as usize)).collect::<Vec<usize>>(),
        );

        info!(
            "Started downloaded queue for {} pieces",
            main_piece_queue.len()
        );

        // request each piece sequentially for now (sensible for streaming but could use optimization)
        while let Some(piece_idx) = main_piece_queue.pop_front() {
            let start = Instant::now();
            let piece_size = self.meta_file.get_piece_len(piece_idx);
            let num_blocks = (((piece_size as u32) / BLOCK_SIZE) as f32).ceil() as u32;
            // check how many peers have this piece
            let mut candidates = self
                .peer_manager
                .lock()
                .await
                .with_piece(piece_idx as u32)
                .await;
            info!("Found {} peers with piece {piece_idx}", candidates.len());
            // send "interested" to all all of them
            for candidate in candidates.iter_mut() {
                self.peer_manager
                    .lock()
                    .await
                    .show_interest_in_peer(candidate)
                    .await;
            }
            // check if any have us unchoked
            let mut candidates_unchoked = Vec::new();
            for candidate in &candidates {
                let guard = self.peer_manager.lock().await;
                let peer = guard.find_peer(candidate);
                if let Some(peer) = peer {
                    if !peer.peer_choking {
                        candidates_unchoked.push(candidate);
                    }
                }
            }
            info!(
                "{} matched candidates that haven't choked us",
                candidates_unchoked.len()
            );
            while candidates_unchoked.is_empty() {
                // if none, then wait on mpsc channel for them to unchoke us
                info!("Waiting for peer to unchoke us");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }

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

            // listen for responses here UNTIL we assemble the whole piece
            info!("Waiting for all peers to finish work");
            self.notify_pipelines_empty.wait_for_notification().await;
            info!("Done waiting for piece DL");

            // verify hash & persist
            let mut store = self.piece_store.lock().await;
            let piece = &store.pieces[piece_idx as usize];
            if piece.verify_hash() {
                info!("Hash verified");
                store.persist(piece_idx, output_path)?;
                store.reset_piece(piece_idx);
            } else {
                // if invalid hash, then put entry back on pipeline and flush again, if failed twice, then panic??
                error!("Piece {piece_idx} hash mismatch!!");
                panic!();
                // main_piece_queue.push_front(piece_idx);
            }

            self.peer_manager.lock().await.request_tracker.reset();

            println!(
                "Piece {piece_idx} successfully downloaded in {:?}",
                start.elapsed()
            );
        }

        // collected all pieces, now simply concat files
        info!("Concatenating all pieces");
        self.piece_store.lock().await.concat(output_path)?;

        Ok(())
    }
}

pub struct Engine {
    // map info hash to bittorrent struct
    torrents: HashMap<[u8; 20], Arc<Mutex<BitTorrent>>>,
}

// TODO: be able to create a torrent from video file

impl Engine {
    pub fn new() -> Self {
        Self {
            torrents: HashMap::new(),
        }
    }

    pub async fn add_torrent(&mut self, input_path: &str) -> anyhow::Result<()> {
        let meta_file = parse_torrent_file(input_path)?;
        let info_hash = meta_file.get_info_hash();
        let bt = BitTorrent::from_torrent_file(meta_file).await?;
        let bt = Arc::new(Mutex::new(bt));

        self.torrents.insert(info_hash, bt);

        Ok(())
    }

    pub async fn start_server(&mut self) -> anyhow::Result<()> {
        info!("Listening on inbound server...");
        let listener = TcpListener::bind(format!("127.0.0.1:{}", PORT)).await?;

        loop {
            let (mut socket, addr) = listener.accept().await?;
            // figure out which info hash they want
            // wait for handshake here
            let mut res = [0u8; 68];
            // add timeout on this
            socket.read_exact(&mut res).await?;

            assert_eq!(res[0], 19);
            assert_eq!(&res[1..20], b"BitTorrent protocol");

            let info_hash: [u8; 20] = res[28..48].try_into().unwrap();
            let bittorrent = self.torrents.get(&info_hash);

            if let Some(bittorrent) = bittorrent {
                let bittorrent = bittorrent.lock().await;
                let mut pm_guard = bittorrent.peer_manager.lock().await;
                let peer = pm_guard.find_or_create(addr).await.conn_info.clone();
                info!("New connection from {:?}", peer);
                pm_guard.init_session(socket, peer, info_hash).await;
            } else {
                // if not serving, connection reset
            }
        }
    }
}
