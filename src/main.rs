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

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let context = RuntimeContext::init(args);
    let krpc = Krpc::init(context).await;

    // 1. enter with a bootstrap contact or init new network
    dht::join_dht_network(args.get_bootstrap());

    // 2. start dht server
    krpc.listen();
}
