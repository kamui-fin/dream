use clap::{Parser, Subcommand};
use directories::{BaseDirs, ProjectDirs, UserDirs};
use serde::Deserialize;
use std::{collections::HashMap, fs};

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
    pub output_dir: String,
}

#[derive(Debug, Deserialize)]
pub struct LogConfig {
    pub log_file: String,
    pub modules: LogModules,
}

#[derive(Debug, Deserialize)]
pub struct LogModules {
    pub dht: String,
    pub torrent: String,
}

#[derive(Debug, Deserialize)]
pub struct DHTConfig {
    pub enabled: bool,
    pub always_use_dht: bool,
    pub max_num_peers_request: usize,
    pub bucket_refresh_interval: u64,
    pub k_bucket_size: usize,
    pub alpha_parallel_requests: usize,
}

#[derive(Debug, Deserialize)]
pub struct TorrentConfig {
    pub block_size: u32,
    pub piece_size: u32,
}

#[derive(Debug, Deserialize)]
pub struct StreamConfig {
    pub video_player: String,
    pub buffer_num_pieces: usize,
    pub rarest_piece_enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct NetworkConfig {
    pub dht_bootstrap: String,
    pub dht_port: u16,
    pub dht_api_port: u16,

    pub elastic_search_ip: String,
    pub elastic_search_port: u16,

    pub torrent_port: u16,
    pub stream_server_port: u16,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub general: GeneralConfig,
    pub dht: DHTConfig,
    pub logging: LogConfig,
    pub network: NetworkConfig,
    pub torrent: TorrentConfig,
    pub stream: StreamConfig,
}

pub fn get_config_path() -> String {
    let proj_dirs =
        ProjectDirs::from("com", "kamui", "dream").expect("Unable to find project directory");
    let config_dir = proj_dirs.config_dir();
    let config_path = config_dir.join("config.toml");
    config_path.to_str().unwrap().to_string()
}

impl Config {
    pub fn new() -> Self {
        let content = fs::read_to_string(get_config_path()).unwrap();
        toml::from_str(&content).unwrap()
    }
}
