use std::{net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc};

use log::{error, info};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::{mpsc, Mutex},
};

use crate::{
    bittorrent::BitTorrent,
    metafile::Metafile,
    msg::{DataReady, ServerMsg},
    peer::session::ConnectionInfo,
    utils, PORT,
};

pub struct Engine {
    torrents: Vec<Arc<Mutex<BitTorrent>>>,
    info_hashes: Vec<[u8; 20]>,

    // listen for external commands
    command_rx: mpsc::Receiver<ServerMsg>,
}

impl Engine {
    pub fn new(command_rx: mpsc::Receiver<ServerMsg>) -> Self {
        Self {
            torrents: Vec::new(),
            info_hashes: Vec::new(),
            command_rx,
        }
    }

    pub async fn add_torrent(
        &mut self,
        meta_file: Metafile,
        output_dir: PathBuf,
    ) -> anyhow::Result<()> {
        let info_hash = meta_file.get_info_hash();
        let bt = BitTorrent::from_torrent_file(meta_file, output_dir.join(hex::encode(info_hash)))
            .await?;
        let bt = Arc::new(Mutex::new(bt));

        self.torrents.push(bt);
        self.info_hashes.push(info_hash);

        info!("Finished adding torrent");

        Ok(())
    }

    pub async fn start_server(&mut self) -> anyhow::Result<()> {
        info!("Listening on inbound server...");
        let listener = TcpListener::bind(format!("127.0.0.1:{}", PORT)).await?;

        loop {
            tokio::select! {
                Ok((socket, addr)) = listener.accept() => {
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
                pm_guard
                    .init_session_from_stream(socket, peer, info_hash)
                    .await;
            }
        }
        // If not serving, connection reset

        Ok(())
    }

    async fn handle_command(&mut self, command: ServerMsg) -> anyhow::Result<()> {
        match command {
            ServerMsg::AddExternalTorrent {
                input_path,
                output_dir,
            } => {
                let meta_file = Metafile::parse_torrent_file(&input_path);
                match meta_file {
                    Err(e) => {
                        error!("Failed to parse torrent file: {:?}", e);
                    }
                    Ok(meta_file) => {
                        if let Err(e) = self.add_torrent(meta_file, PathBuf::from(output_dir)).await
                        {
                            error!("Failed to add torrent: {:?}", e);
                        }
                    }
                }
            }
            ServerMsg::AddVideo {
                input_path,
                output_dir,
            } => {
                let meta_file =
                    Metafile::from_video(&PathBuf::from_str(&input_path).unwrap(), 1024, None);
                if let Err(e) = self.add_torrent(meta_file, PathBuf::from(output_dir)).await {
                    error!("Failed to add video: {:?}", e);
                }
            }
            ServerMsg::StreamRequestRange {
                start,
                end,
                info_hash,
                response_tx,
            } => {
                let idx = self
                    .info_hashes
                    .iter()
                    .position(|i| i == &info_hash)
                    .unwrap();
                let mut bt = self.torrents[idx].lock().await;

                let pieces_needed =
                    utils::byte_to_piece_range(start, end, bt.meta_file.get_piece_len(0));

                let mut curr_start = start;
                for (i, piece) in pieces_needed.enumerate() {
                    let mut piece_data: Vec<u8> = if bt
                        .piece_store
                        .lock()
                        .await
                        .get_status_bitfield()
                        .piece_exists(piece as u32)
                    {
                        // fetch from output file
                        bt.piece_store.lock().await.get_piece_data_fs(piece as u32)
                    } else {
                        // download
                        bt.download_piece(piece as usize).await.unwrap_or_default()
                    };
                    let piece_len = piece_data.len() as u64;

                    // handle when start starts middle of piece

                    // start truncation --> truncate start_of_piece to start % piece_len (NONE-INCLUSIVE)
                    // end truncation --> truncate end+1 till the end_of_piece (INCLUSIVE) end
                    let normal_piece_len = bt.meta_file.get_piece_len(0);

                    if curr_start == start && curr_start % piece_len != 0 {
                        piece_data = piece_data[(start % piece_len) as usize..].to_vec();
                    }

                    if end / normal_piece_len == piece {
                        let truncate_len = (end % normal_piece_len + 1);
                        piece_data.truncate(truncate_len as usize);
                    }

                    let data_msg = DataReady {
                        has_more: end / normal_piece_len == piece,
                        data: piece_data,
                    };
                    response_tx.send(data_msg).await?;

                    curr_start = (piece + 1) * bt.meta_file.get_piece_len(0);
                }
            }
        }

        Ok(())
    }
}
