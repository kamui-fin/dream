use std::{collections::HashMap, fs};

use clap::{Parser, Subcommand};
use serde::Deserialize;

lazy_static! {
    pub static ref CONFIG: Config = Config::new();
}

#[derive(Parser)]
#[command(name = "dream-cli")]
#[command(version)]
#[command(about = "A CLI into the dream streaming platform", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Upload { path: String, title: String },
    Stream { query: String },
}

#[derive(Debug, Deserialize)]
pub struct GeneralConfig {
    pub output_directry: String,
}

#[derive(Debug, Deserialize)]
pub struct LogConfig {
    pub modules: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct DHTConfig {
    pub always_use_dht: bool,
    pub max_num_peers_request: usize,
    pub bucket_refresh_interval: u64,
    pub id_size: usize,
    pub k_bucket_size: usize,
    pub alpha_parallel_requests: usize,
    pub private: bool,
}

#[derive(Debug, Deserialize)]
pub struct TorrentConfig {
    pub block_size: u32,
    pub piece_size: u32,
    pub max_peer_connections: u32,
}

#[derive(Debug, Deserialize)]
pub struct StreamConfig {
    pub video_player: String,
    pub stream_buffer_num_pieces: u32,
    pub rarest_piece_enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct NetworkConfig {
    pub dht_port: u32,
    pub elastic_search_port: u32,
    pub torrent_port: u32,
    pub stream_server_port: u32,
    pub bootstrap_ip: String,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub logging: LogConfig,
    pub dht: DHTConfig,
    pub log: LogConfig,
    pub network: NetworkConfig,
    pub torrent: TorrentConfig,
    pub streaming: StreamConfig,
}

impl Config {
    pub fn new() -> Self {
        let config_path = "../config.toml";
        let content = fs::read_to_string(config_path).unwrap();

        toml::from_str(&content).unwrap()
    }
}
