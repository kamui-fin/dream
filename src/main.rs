mod bittorrent;
mod engine;
mod metafile;
mod msg;
mod peer;
mod piece;
mod stream;
mod tracker;
mod utils;

use std::{path::PathBuf, sync::Arc};

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

    tx.send(msg::ServerMsg::AddExternalTorrent {
        input_path,
        output_dir: output_dir.clone(),
    })
    .await?;

    tokio::spawn(async move {
        stream::start_server(Arc::new(tx.clone()), Arc::new(PathBuf::from(output_dir)))
            .await
            .expect("Server failed to run");
    });

    result.await?
}
