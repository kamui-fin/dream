use clap::{Parser, Subcommand};

pub const ip: &str = "localhost:9200";

pub const stream_server_port: u16 = 3000;

// if torrent file has no tracker url, use dht
pub const fallback_to_dht: bool = false;

// always use dht?
pub const force_dht: bool = false;

// path for logs to be outputed to
pub const log_output: &str = "log.txt";

// path for video player (defaults to .npv when being used)
pub const video_player: &str = "mpv";

// max number of pieces to buffer while streaming
pub const stream_buffer: u32 = 4;

// rarest_piece first?
pub const enable_rarest_piece: bool = false;

// piece size for torrents
pub const piece_size: u32 = 2 ^ 20;

// how many concurrent connections are we allowed to have
pub const max_concurrent_connections: u32 = 50;

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
