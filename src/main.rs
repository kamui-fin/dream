mod bittorrent;
mod msg;
mod peer;
mod piece;
mod tracker;
mod utils;

use std::sync::Arc;

use anyhow::Result;
use bittorrent::BitTorrent;
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

    let client = Arc::new(Mutex::new(BitTorrent::from_torrent_file(input_file).await?));

    // let client_clone = client.clone();
    // tokio::spawn(async move {
    //     client_clone.lock().await.start_server().await.unwrap();
    // });

    client.lock().await.begin_download(output_dir).await?;

    Ok(())
}
