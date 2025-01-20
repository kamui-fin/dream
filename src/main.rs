mod bittorrent;
mod engine;
mod metafile;
mod msg;
mod peer;
mod piece;
mod tracker;
mod utils;

use anyhow::Result;
use engine::Engine;
use tokio::sync::mpsc::{self};

const PORT: u16 = 6881;

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init_timed();

    let input_path = "debian.torrent".to_string();
    let output_dir = "output".to_string();
    let (tx, rx) = mpsc::channel(32);

    let result = tokio::spawn(async move {
        let mut engine = Engine::new(rx);
        engine.start_server().await
    });

    tx.send(msg::ServerCommand::AddExternalTorrent {
        input_path,
        output_dir,
    })
    .await?;

    tx.send(msg::ServerCommand::Start(0)).await?;

    result.await?
}
