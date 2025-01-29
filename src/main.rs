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

    let (response_tx, response_rx) = oneshot::channel();
    tx.send(msg::ServerMsg::AddExternalTorrent {
        input_path: PathBuf::from("test.torrent"),
        output_dir: output_dir.clone(),
        response_tx,
    })
    .await
    .unwrap();

    let (response_tx, mut response_rx) = mpsc::channel(2000);

    // 10 mb
    let end = 10 * 1024 * 1024;
    tx.send(msg::ServerMsg::StreamRequestRange {
        start: 0,
        end,
        info_hash: decode_node_id("8d14f17d735d1c07aa6a1f2fbd011b91fcd34100".into()),
        response_tx,
    })
    .await
    .unwrap();

    // consume response_rx
    while let Some(data_ready) = response_rx.recv().await {
        if data_ready.has_more {
            info!("Received data");
        } else {
            break;
        }
    }

    // tokio::spawn(async move {
    //     dream::stream::start_server(Arc::new(tx.clone()), Arc::new(PathBuf::from(output_dir)))
    //         .await
    //         .expect("Server failed to run");
    // });

    result.await?
}
