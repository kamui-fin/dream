use std::sync::atomic::AtomicU32;

use anyhow::Result;
use log::info;
use tokio::{io::AsyncReadExt, net::TcpListener};

use crate::{
    msg::{Message, MessageType},
    peer::{PeerManager, RemotePeer},
    piece::{BitField, PieceStore},
    tracker::Metafile,
    utils::slice_to_u32_msb,
};

pub struct BitTorrent<'a> {
    meta_file: Metafile,
    piece_store: PieceStore<'a>, // references meta_file
    peer_manager: PeerManager,
}

impl<'a> BitTorrent<'a> {
    pub async fn start_server(&mut self, port: u16) -> Result<()> {
        let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await?;

        loop {
            let (mut socket, addr) = listener.accept().await?;

            let peer: &RemotePeer = self.peer_manager.find_or_create(addr);
            println!("New connection from {:?}", peer);

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

                            self.handle_msg(bt_msg, &mut peer);
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

    pub fn handle_msg(&mut self, bt_msg: Message, peer: &mut RemotePeer) {
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
                peer.peer_choking = false;
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
            }
            MessageType::Piece => {
                // in response to Request, returns piece data
                // index, begin, block data
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
// =======
pub async fn start_server(port: u16) -> Result<()> {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await?;

    loop {
        let (mut socket, addr) = listener.accept().await?;
        println!("New connection from {:?}", addr);

        tokio::spawn(async move {
            let mut buf = [0; 2028];

            loop {
                match socket.read(&mut buf).await {
                    Ok(0) => {
                        println!("Connection closed by {:?}", addr);
                        break;
                    }
                    Ok(n) => {
                        let bt_msg = Message::parse(&buf);
                        info!("Received msg: {:#?}", bt_msg);

                        // handle_msg(bt_msg);
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

pub fn handle_msg(bt_msg: Message, peer: &mut RemotePeer, meta_file: &Metafile) {
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
            peer.peer_choking = false;
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
        }
        MessageType::Piece => {
            // in response to Request, returns piece data
            // index, begin, block data
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
