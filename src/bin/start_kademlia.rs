extern crate pretty_env_logger;
#[macro_use]
extern crate log;

use clap::Parser;
use dream::dht::{config::Args, start_dht};

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    let args = Args::parse();
    start_dht(&args).await;
}
