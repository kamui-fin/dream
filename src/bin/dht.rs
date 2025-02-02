use clap::Parser;
use dream::{
    dht::{config::Args, start_dht},
    utils::init_logger,
};

#[tokio::main]
async fn main() {
    init_logger();

    let args = Args::parse();
    start_dht(&args).await;
}
