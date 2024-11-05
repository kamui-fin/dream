use crate::dht::start_dht;
use clap::Parser;
use config::Args;

mod config;
mod context;
mod dht;
mod krpc;
mod node;
mod routing;
mod utils;

#[tokio::main]
async fn main() {
    let args = Args::parse();

    start_dht(&args).await;
}
