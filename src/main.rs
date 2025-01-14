mod bittorrent;
mod msg;
mod peer;
mod piece;
mod tracker;
mod utils;

use std::sync::Arc;

use anyhow::Result;
use bittorrent::{BitTorrent, Engine};
use log::info;
use msg::InternalMessage;
use tokio::sync::{
    mpsc::{self, Receiver},
    Mutex,
};

const PORT: u16 = 6881;

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init_timed();

    let input_file = "debian.torrent";
    let output_dir = "output";

    let mut engine = Engine::new();

    engine.add_torrent(input_file).await?;

    engine.start_server().await?;

    Ok(())
}
