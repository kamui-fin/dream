use std::sync::Arc;

use clap::Parser;
use config::Args;
use context::RuntimeContext;
use krpc::Krpc;

mod config;
mod context;
mod dht;
mod krpc;
mod node;
mod routing;
mod utils;

async fn start_dht(args: &Args) {
    let context = Arc::new(RuntimeContext::init(args));
    let krpc = Arc::new(Krpc::init(context.clone()).await);

    // 1. enter with a bootstrap contact or init new network
    dht::join_dht_network(&context, args.get_bootstrap(), &krpc).await;

    // 2. start dht server
    krpc.listen().await;
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    start_dht(&args).await;
}
