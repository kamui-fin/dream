use std::sync::Arc;

use crate::{config::Args, context::RuntimeContext, kademlia::Kademlia};

pub async fn start_dht(args: &Args) {
    let context = Arc::new(RuntimeContext::init(args));
    let kademlia = Arc::new(Kademlia::init(context.clone()).await);

    // 1. enter with a bootstrap contact or init new network
    kademlia
        .clone()
        .join_dht_network(args.get_bootstrap())
        .await;

    // 2. start maintenance tasks
    kademlia.clone().republish_peer_task();
    context.regen_token_task();

    // 3. start dht server
    kademlia.listen().await;
}
