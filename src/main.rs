use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use dream::{config::CONFIG, engine::Engine, utils::init_logger};
use tokio::sync::mpsc::{self};

#[tokio::main]
async fn main() -> Result<()> {
    init_logger(&CONFIG.logging.modules.torrent);

    let output_dir = PathBuf::from(&CONFIG.general.output_dir);
    if !output_dir.exists() {
        return Err(anyhow::anyhow!("Output directory does not exist"));
    }

    let (tx, rx) = mpsc::channel(32);

    let result = tokio::spawn(async move {
        let mut engine = Engine::new(rx);
        engine.start_server().await
    });

    tokio::spawn(async move {
        dream::stream::start_server(Arc::new(tx.clone()), Arc::new(output_dir))
            .await
            .expect("Server failed to run");
    });

    result.await?
}
