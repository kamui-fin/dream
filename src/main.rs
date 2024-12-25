mod bittorrent;
mod msg;
mod peer;
mod piece;
mod tracker;
mod utils;

use anyhow::Result;
use bittorrent::BitTorrent;

const PORT: u16 = 6881;

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let input_file = "debian.torrent";
    let output_dir = "output";

    let mut client = BitTorrent::from_torrent_file(input_file).await?;

    tokio::spawn(async move {
        client.start_server().await.unwrap();
    });

    // client.begin_download(output_dir).await;

    Ok(())
}
