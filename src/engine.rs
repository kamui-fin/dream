use std::{net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc};

use log::{error, info};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::{mpsc, Mutex},
};

use crate::{
    bittorrent::BitTorrent, metafile::Metafile, msg::ServerCommand, peer::session::ConnectionInfo,
    PORT,
};

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
