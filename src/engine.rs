use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use log::{error, info};
use reqwest::Client;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, Mutex},
};

use crate::{
    bittorrent::BitTorrent,
    config::CONFIG,
    metafile::Metafile,
    msg::{DataReady, ServerMsg},
    peer::session::ConnectionInfo,
    stream::VideoRecord,
    utils,
};

pub struct Engine {
    torrents: Vec<Arc<Mutex<BitTorrent>>>,
    info_hashes: Vec<[u8; 20]>,

    // listen for external commands
    command_rx: mpsc::Receiver<ServerMsg>,

    last_stream_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Engine {
    pub fn new(command_rx: mpsc::Receiver<ServerMsg>) -> Self {
        Self {
            torrents: Vec::new(),
            info_hashes: Vec::new(),
            command_rx,
            last_stream_handle: None,
        }
    }

    pub async fn load_from_elastic_search(&mut self) -> anyhow::Result<()> {
        // get all records (unique info_hashes) from elastic search
        // for each record, get the metafile and output_dir
        // run add_torrent on each one

        let url = format!(
            "http://{}:{}/videos/_search",
            CONFIG.network.elastic_search_ip, CONFIG.network.elastic_search_port
        );
        let body = serde_json::json!({
            "query": {
                "match_all": {}
            },
            "size": 10000,
            "from": 0
        });

        let client = Client::new();
        let response = client.post(&url).json(&body).send().await?;

        let records = if response.status().is_success() {
            let json: serde_json::Value = response.json().await?;
            let hits = json["hits"]["hits"].as_array().unwrap();
            let records: Vec<VideoRecord> = hits
                .iter()
                .map(|hit| serde_json::from_value(hit["_source"].clone()).unwrap())
                .collect();

            records
        } else {
            println!("Failed to perform search: {:?}", response.text().await?);
            vec![]
        };

        info!("Initializing engine with {} records", records.len());

        for record in records {
            let meta_file_bytes = hex::decode(&record.meta_file_bytes).unwrap();
            let meta_file = Metafile::parse_bytes(&meta_file_bytes).unwrap();

            let (response_tx, response_rx) = oneshot::channel();
            self.add_torrent(meta_file, response_tx).await?;
        }

        Ok(())
    }

    pub async fn add_torrent(
        &mut self,
        meta_file: Metafile,
        response_tx: oneshot::Sender<(u64, String)>,
    ) -> anyhow::Result<()> {
        let info_hash = meta_file.get_info_hash();

        info!("Adding torrent with info hash: {:?}", info_hash);

        let mime_type = mime_guess::from_path(&meta_file.info.name)
            .first_or_octet_stream()
            .to_string();

        response_tx
            .send((meta_file.info.length.unwrap(), mime_type))
            .unwrap();

        if self.info_hashes.contains(&info_hash) {
            info!("Torrent already added");
            return Ok(());
        }

        let output_dir = PathBuf::from(&CONFIG.general.output_dir);
        let output_dir = output_dir.join(hex::encode(info_hash));
        let bt = BitTorrent::from_torrent_file(meta_file, output_dir).await?;
        let bt = Arc::new(Mutex::new(bt));

        self.torrents.push(bt);
        self.info_hashes.push(info_hash);

        info!("Finished adding torrent");

        Ok(())
    }

    pub async fn start_server(&mut self) -> anyhow::Result<()> {
        info!("Listening on inbound server...");
        let listener =
            TcpListener::bind(format!("0.0.0.0:{}", CONFIG.network.torrent_port)).await?;

        loop {
            tokio::select! {
                Ok((socket, addr)) = listener.accept() => {
                    self.handle_new_connection(socket, addr).await?;
                }

                Some(command) = self.command_rx.recv() => {
                    self.handle_command(command).await.unwrap();
                }
            }
        }
    }

    async fn handle_new_connection(
        &mut self,
        mut socket: TcpStream,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let peer = ConnectionInfo::from_addr(addr);

        info!("New connection from {:?}", peer);

        let mut res = [0u8; 68];
        socket.read_exact(&mut res).await?;

        if res[0] != 19 || &res[1..20] != b"BitTorrent protocol" {
            error!("Invalid handshake");
            return Ok(());
        }

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
                input_data,
                output_dir,
                response_tx,
            } => {
                let meta_file = Metafile::parse_bytes(&input_data);
                match meta_file {
                    Err(e) => {
                        error!("Failed to parse torrent file: {:?}", e);
                    }
                    Ok(meta_file) => {
                        info!("Successfully parsed torrent file");
                        if let Err(e) = self.add_torrent(meta_file, response_tx).await {
                            error!("Failed to add torrent: {:?}", e);
                        }
                    }
                }
            }
            ServerMsg::StreamRequestRange {
                start,
                end,
                info_hash,
                response_tx,
            } => {
                if let Some(last_stream) = &self.last_stream_handle {
                    last_stream.abort();
                }

                let idx = self
                    .info_hashes
                    .iter()
                    .position(|i| i == &info_hash)
                    .unwrap();

                let bt = self.torrents[idx].clone();
                self.last_stream_handle = Some(tokio::spawn(async move {
                    let mut bt = bt.lock().await;
                    let (start_piece, end_piece) =
                        utils::byte_to_piece_range(start, end + 1, bt.meta_file.get_piece_len(0));

                    let pieces_needed = start_piece..=end_piece;

                    info!("Pieces needed: {:?}", pieces_needed);

                    let last_piece = end_piece;
                    let mut curr_start = start;
                    let window_size = CONFIG.stream.buffer_num_pieces;

                    for (i, piece) in pieces_needed.enumerate() {
                        if i % window_size == 0 && CONFIG.stream.rarest_piece_enabled {
                            // download rarest piece every now and then if enabled in config
                            // may decrease performance but increases availability of data across peers
                            let piece = bt.peer_manager.lock().await.get_rarest_piece().await;
                            if let Some(piece) = piece {
                                // only download if we don't already have it
                                if !bt
                                    .piece_store
                                    .lock()
                                    .await
                                    .get_status_bitfield()
                                    .piece_exists(piece)
                                {
                                    bt.download_piece(piece as usize).await.unwrap();
                                }
                            }
                        }

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
                            bt.download_piece(piece as usize).await.unwrap()
                        };
                        let piece_len = piece_data.len() as u64;
                        // start truncation --> truncate start_of_piece to start % piece_len (NONE-INCLUSIVE)
                        // end truncation --> truncate end+1 till the end_of_piece (INCLUSIVE) end
                        let normal_piece_len = bt.meta_file.get_piece_len(0);

                        // FIXME: attempt to calculate the remainder with a divisor of zero
                        if curr_start == start && curr_start % piece_len != 0 {
                            piece_data = piece_data[(start % piece_len) as usize..].to_vec();
                        }

                        if end / normal_piece_len == piece {
                            let truncate_len = end % normal_piece_len + 1;
                            piece_data.truncate(truncate_len as usize);
                        }

                        let data_msg = DataReady {
                            has_more: last_piece != piece,
                            data: piece_data,
                        };

                        response_tx.send(data_msg).await.unwrap();
                        curr_start = (piece + 1) * bt.meta_file.get_piece_len(0);
                    }
                }));
            }
        }

        Ok(())
    }
}
