use dream::{config::CONFIG, dht::start_dht, utils::init_logger};

#[tokio::main]
async fn main() {
    init_logger(CONFIG.logging.modules.dht.as_str());
    start_dht().await;
}
