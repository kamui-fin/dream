use clap::{Parser, Subcommand};

/* Logging-related */

pub const DEFAULT_LOG_LEVEL: &str = "info";
// TODO: further log settings

/* General dream-wide settings */

// By default, we use our public ip to join the mainline DHT
// If you want to only use dream within a private network, enable this
pub const PRIVATE: bool = false;

// Ports for services, default is recommended
pub const ELASTIC_SEARCH_PORT: u16 = 9200;
pub const DHT_PORT: u16 = 8999;
pub const TORRENT_PORT: u16 = 6881;
pub const STREAM_SERVER_PORT: u16 = 3000;

// Path for video player
pub const VIDEO_PLAYER: &str = "mpv";

// Max number of pieces to buffer while streaming
pub const STREAM_BUFFER_NUM_PIECES: u32 = 4;

// Rarest_piece first?
pub const RAREST_PIECE_ENABLED: bool = false;

// Piece size for torrents
pub const PIECE_SIZE: u32 = 2 ^ 20;

// How many concurrent connections are we allowed to have
pub const MAX_PEER_CONNECTIONS: u32 = 50;

// Block size in bytes -> default 16KB
pub const BLOCK_SIZE: u32 = 2_u32.pow(14);

// Block size in KB
pub const KB_PER_BLOCK: u32 = BLOCK_SIZE / 1000;

/* DHT section */

// Always use dht? If false, we will only use DHT if no external tracker is found in .torrent file
pub const ALWAYS_USE_DHT: bool = false;

// Maximum number of peers to request from DHT
pub const MAX_NUM_PEERS_REQUEST: usize = 100;

// Amount of time to wait before refreshing
pub const BUCKET_REFRESH_INTERVAL: u64 = 15 * 60;

// Number of bytes in a node ID
pub const ID_SIZE: usize = 20; // 160 bits

// Max number of entries in K-bucket
pub const K_BUCKET_SIZE: usize = 8;

// Max concurrent requests
pub const ALPHA_PARALLEL_REQUESTS: usize = 3;

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
