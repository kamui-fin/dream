use clap::{command, Parser};

// Max number of entries in K-bucket
pub const K: usize = 4;
// Max concurrent requests
pub const ALPHA: usize = 3;
// Amount of time to wait before refreshing
pub const REFRESH_TIME: u64 = 15 * 60;

// TOML config loading and clap CLI arg parse

#[derive(Parser, Debug, Default)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(short, long, default_value_t = 6881)]
    pub port: u16,

    #[arg(long, default_value = "router.bittorrent.com:6881")]
    pub bootstrap: Option<String>,
}
