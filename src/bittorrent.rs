use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc::{Receiver, Sender};

use crate::peer::UnchokeMessage;
use crate::piece;
use crate::tracker::{self, parse_torrent_file};
use crate::{
    msg::{Message, MessageType},
    peer::{PeerManager, RemotePeer},
    piece::{BitField, PieceStore, BLOCK_SIZE},
    tracker::Metafile,
    utils::slice_to_u32_msb,
};

pub struct BitTorrent {
    meta_file: Metafile,
    piece_store: PieceStore, // references meta_file
    peer_manager: PeerManager,

    pub unchoke_tx: Sender<UnchokeMessage>,
    pub unchoke_rx: Receiver<UnchokeMessage>,
}

impl BitTorrent {
    pub async fn from_torrent_file(torrent_file: &str) -> anyhow::Result<Self> {
        let (unchoke_tx, unchoke_rx) = tokio::sync::mpsc::channel(100);

        let meta_file = parse_torrent_file(torrent_file)?;

        let peers = tracker::get_peers_from_tracker(
            &meta_file.announce,
            tracker::TrackerRequest::new(&meta_file),
        )?;

        let piece_store = PieceStore::new(meta_file.clone());

        let peer_manager =
            PeerManager::connect_peers(peers, Arc::new(Mutex::new(piece_store)), &unchoke_tx).await;

        

        Ok(Self {
            meta_file,
            piece_store,
            peer_manager,
            unchoke_rx,
            unchoke_tx,
        })
    }

    pub async fn begin_download(&mut self, output_dir: &str) {
        let output_path = Path::new(output_dir);
        if !output_path.exists() {
            std::fs::create_dir(output_path).unwrap();
        }

        // request each piece sequentially for now (sensible for streaming but could use optimization)
        for piece_idx in 0..(self.piece_store.num_pieces) {
            let piece_size = self.meta_file.get_piece_len(piece_idx as usize);
            let num_blocks = (((piece_size as u32) / BLOCK_SIZE) as f32).ceil() as u32;
            // check how many peers have this piece
            let candidates = self.peer_manager.with_piece(piece_idx);
            // if = 0 then wait on mpsc channel for someone to advertise it
            // TODO: zero seeders even possible?
            // check if any have us unchoked
            let candidates: Vec<_> = candidates
                .iter()
                .filter(|p| !p.peer_choking)
                .map(|p| p.peer.clone())
                .collect();
            if candidates.is_empty() {
                // if none, then wait on mpsc channel for them to unchoke us
                loop {
                    let unchoke_msg = self.unchoke_rx.recv().await;
                    if let Some(UnchokeMessage { peer }) = unchoke_msg {
                        if self.peer_manager.peer_has_piece(&peer, piece_idx) {
                            // let this peer download the whole piece for now, TODO optimize
                            self.peer_manager
                                .queue_blocks_for_peer(&peer, piece_idx, 0..num_blocks)
                                .await;
                            break;
                        }
                    }
                }
            } else {
                let blocks_per_peer = num_blocks / candidates.len() as u32;

                for (i, peer) in candidates.iter().enumerate() {
                    let start = i as u32 * blocks_per_peer;
                    let end = if i == candidates.len() - 1 {
                        num_blocks
                    } else {
                        (i as u32 + 1) * blocks_per_peer
                    };
                    self.peer_manager
                        .queue_blocks_for_peer(peer, piece_idx, start..end)
                        .await;
                }
            }

            // listen for responses here UNTIL we assemble the whole piece
            self.peer_manager.flush_pipeline();

            // verify hash & persist
            // if invalid hash, then put entry back on pipeline and flush again, if failed twice, then panic??
        }
    }

    /* pub async fn start_server(&mut self, port: u16) -> Result<()> {
           let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await?;

           loop {
               let (mut socket, addr) = listener.accept().await?;

               self.peer_manager
                   .find_or_create(addr, self.piece_store.clone(), self.unchoke_tx.clone())
                   .await;

               let peer: &RemotePeer = self.peer_manager.swarm.last().unwrap();
               println!("New connection from {:?}", peer.peer);

               tokio::spawn(async move {
                   let mut buf = [0; 2028];

                   loop {
                       match socket.read(&mut buf).await {
                           Ok(0) => {
                               println!("Connection closed by {:?}", addr);
                               break;
                           }
                           Ok(_) => {
                               let bt_msg = Message::parse(&buf);
                               info!("Received msg: {:#?}", bt_msg);

                               self.handle_msg(bt_msg, &mut peer).await;
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
    */
    pub async fn handle_msg(&mut self, bt_msg: Message, peer: &mut RemotePeer) {
        match bt_msg.msg_type {
            MessageType::KeepAlive => {
                // close connection after 2 min of inactivity (no commands)
                // keepalive is just a dummy msg to reset that timer
            }
            MessageType::Choke => {
                // peer has choked us
                peer.peer_choking = true;
            }
            MessageType::UnChoke => {
                // peer has unchoked us
                peer.unchoke_us().await;
            }
            MessageType::Interested => {
                // peer is interested in us
                peer.peer_interested = true;
            }
            MessageType::NotInterested => {
                // peer is not interested in us
                peer.peer_interested = false;
            }
            MessageType::Have => {
                // peer has piece <piece_index>
                // sent after piece is downloaded and verified
                let piece_index = slice_to_u32_msb(&bt_msg.payload[0..4]);
                peer.piece_lookup.mark_piece(piece_index);
            }
            MessageType::Bitfield => {
                // info about which pieces peer has
                // only sent right after handshake, and before any other msg (so optional)
                peer.piece_lookup = BitField(bt_msg.payload);
            }
            MessageType::Request => {
                // requests a piece - (index, begin byte offset, length)
                let piece_idx = slice_to_u32_msb(&bt_msg.payload[0..4]);

                if !peer.am_choking && peer.piece_lookup.piece_exists(piece_idx){
                    let byte_offset = slice_to_u32_msb(&bt_msg.payload[4..8]);
                    let length = slice_to_u32_msb(&bt_msg.payload[8..12]);

                    self.piece_store.pieces[piece_idx as usize].requested();
                    let target_block = self.piece_store.pieces[piece_idx as usize].retrieve_block(byte_offset as usize, length as usize).unwrap();
                    
                    let mut pay_load = Vec::with_capacity(target_block.len()+8);

                    pay_load.extend_from_slice(&piece_idx.to_be_bytes());
                    pay_load.extend_from_slice(&byte_offset.to_be_bytes());
                    pay_load.extend_from_slice(&target_block);

                    peer.send_message(MessageType::Piece.build_msg(pay_load));
                } 
            }
            MessageType::Piece => {
                // in response to Request, returns piece data
                // index, begin, block data
                let piece_idx = slice_to_u32_msb(&bt_msg.payload[0..4]);
                let begin_offset = slice_to_u32_msb(&bt_msg.payload[4..8]);
                let block_data = &bt_msg.payload[8..];

                self.piece_store.pieces[piece_idx as usize].store_block(begin_offset as usize, block_data.len(), block_data);
            }
            MessageType::Cancel => {
                // informing us that block <index><begin><length> is not needed anymore
                // for endgame algo
                todo!();
            }
            MessageType::Port => {
                // port that their dht node is listening on
                // only for DHT extension
                todo!();
            }
            _ => {}
        }
    }
}
