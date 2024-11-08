extern crate pretty_env_logger;
#[macro_use]
extern crate log;

use crate::dht::start_dht;
use clap::Parser;
use config::Args;
use dht::start_n_nodes;

mod config;
mod context;
mod dht;
mod kademlia;
mod node;
mod routing;
mod utils;

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    let args = Args::parse();
    // start_n_nodes(3).await;
    start_dht(&args).await;
}
