use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use log::{debug, error, info, trace, warn};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time;

use crate::msg::InternalMessage;
use crate::peer::{self, ConnectionInfo, UnchokeMessage};
use crate::tracker::{self, parse_torrent_file};
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

    pub unchoke_rx: Receiver<UnchokeMessage>,
    notify_pipelines_empty: Arc<Notify>,
}

impl BitTorrent {
    pub async fn from_torrent_file(torrent_file: &str) -> anyhow::Result<Self> {
        let (unchoke_tx, unchoke_rx) = tokio::sync::mpsc::channel(100);

        let meta_file = parse_torrent_file(torrent_file)?;
        info!("Parsed metafile: {:#?}", meta_file);

        let peers = tracker::get_peers_from_tracker(
            &meta_file.announce,
            tracker::TrackerRequest::new(&meta_file),
        )?;

        info!("Tracker has found {} peers", peers.peers.len());
        info!("Get Peers: {:#?}", peers);

        let piece_store = PieceStore::new(meta_file.clone());
        let info_hash = piece_store.meta_file.get_info_hash();
        let num_pieces = piece_store.meta_file.get_num_pieces();
        let piece_store = Arc::new(Mutex::new(piece_store));

        // the channel for receiving messages from peer sessions
        let (msg_tx, msg_rx) = mpsc::channel(500);

        let notify_pipelines_empty = Arc::new(Notify::new());
        let peer_ready_notify = Arc::new(Notify::new());
        let peer_manager = PeerManager::connect_peers(
            peers,
            piece_store.clone(),
            &info_hash,
            num_pieces,
            unchoke_tx,
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

        tokio::spawn(async move { Self::listen(msg_rx, pm_clone).await });

        peer_ready_notify.notified().await;

        Ok(Self {
            meta_file,
            piece_store,
            peer_manager,
            unchoke_rx,
            notify_pipelines_empty,
        })
    }

    pub async fn listen(
        mut msg_rx: Receiver<InternalMessage>,
        peer_manager: Arc<Mutex<PeerManager>>,
    ) {
        info!("Beginning to listen for peer msgs");
        loop {
            let msg = msg_rx.recv().await;
            if let Some(InternalMessage {
                msg,
                conn_info,
                should_close,
            }) = &msg
            {
                if *should_close {
                    peer_manager.lock().await.remove_peer(conn_info);
                } else {
                    info!("Received msg {:#?} from peer {:#?}", msg, conn_info);
                    peer_manager.lock().await.handle_msg(&msg, conn_info).await;
                }
            }
        }
    }

    pub async fn begin_download(&mut self, output_dir: &str) -> anyhow::Result<()> {
        let output_path = Path::new(output_dir);
        if !output_path.exists() {
            info!("Creating output directory as it doesn't exist..");
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
                if !peer.peer_choking {
                    candidates_unchoked.push(candidate);
                }
            }
            info!(
                "{} matched candidates that haven't choked us",
                candidates_unchoked.len()
            );
            if candidates_unchoked.is_empty() {
                // if none, then wait on mpsc channel for them to unchoke us
                info!("Waiting for peer to unchoke us");
                loop {
                    let unchoke_msg = self.unchoke_rx.recv().await;
                    if let Some(UnchokeMessage { peer }) = unchoke_msg {
                        if self
                            .peer_manager
                            .lock()
                            .await
                            .peer_has_piece(&peer, piece_idx as u32)
                            .await
                        {
                            info!("Received unchoke msg from relevant peer {:#?}", peer);
                            // let this peer download the whole piece for now, TODO optimize
                            self.peer_manager
                                .lock()
                                .await
                                .queue_blocks(&peer, piece_idx as u32, 0..num_blocks)
                                .await;
                            break;
                        } else {
                            info!("Received unchoke msg but peer does not have piece");
                        }
                    }
                }
            } else {
                let blocks_per_peer = num_blocks / candidates_unchoked.len() as u32;
                info!("Distributed {} blocks per peer", blocks_per_peer);

                for (i, peer) in candidates_unchoked.iter().enumerate() {
                    let start = i as u32 * blocks_per_peer;
                    let end = if i == candidates.len() - 1 {
                        num_blocks
                    } else {
                        (i as u32 + 1) * blocks_per_peer
                    };
                    self.peer_manager
                        .lock()
                        .await
                        .queue_blocks(peer, piece_idx as u32, start..end)
                        .await;
                }
            }

            // listen for responses here UNTIL we assemble the whole piece
            info!("Waiting for all peers to finish work");
            self.notify_pipelines_empty.notified().await;

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
                main_piece_queue.push_front(piece_idx);
            }
        }

        // collected all pieces, now simply concat files
        info!("Concatenating all pieces");
        self.piece_store.lock().await.concat(output_path)?;

        Ok(())
    }

    
    

    pub async fn start_server(&mut self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", PORT)).await?;

        loop {
            let (mut socket, addr) = listener.accept().await?;
            let info_hash = self.piece_store.lock().await.meta_file.get_info_hash();
            let num_pieces = self.piece_store.lock().await.meta_file.get_num_pieces();

            let mut pm_guard = self.peer_manager.lock().await;
            let peer = pm_guard.find_or_create(addr, num_pieces, info_hash).await;

            info!("New connection from {:?}", peer.conn_info);

            tokio::spawn(async move {
                let mut buf = [0; 2028];

                loop {
                    match socket.read(&mut buf).await {
                        Ok(0) => {
                            warn!("Connection closed by {:?}", addr);
                            break;
                        }
                        Ok(_) => {
                            let bt_msg = Message::parse(&buf);
                            info!("Received msg: {:#?}", bt_msg);

                            // self.handle_msg(bt_msg, &mut peer).await;
                        }
                        Err(e) => {
                            println!("Failed to read from socket; err = {:?}", e);
                            break;
                        }
                    }
                }
            });
        }
    }
}
