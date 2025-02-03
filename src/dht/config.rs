use clap::{command, Parser};

#[derive(Parser, Debug, Default)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(short, long, default_value_t = 6881)]
    pub port: u16,

    #[arg(long, default_value = "router.bittorrent.com:6881")]
    pub bootstrap: Option<String>,
}
