use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use dream::{dht::utils::decode_node_id, engine::Engine, msg};
use http_req::response;
use log::info;
use tokio::sync::{
    mpsc::{self},
    oneshot,
};

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init_timed();

    let output_dir = PathBuf::from("output");
    let (tx, rx) = mpsc::channel(32);

    let result = tokio::spawn(async move {
        let mut engine = Engine::new(rx);
        engine.start_server().await
    });

    tokio::spawn(async move {
        dream::stream::start_server(Arc::new(tx.clone()), Arc::new(PathBuf::from(output_dir)))
            .await
            .expect("Server failed to run");
    });

    result.await?
}
