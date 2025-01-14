use std::{
    collections::HashMap,
    net::Ipv4Addr,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use rand::Rng;
use serde_json::{json, Value};
use tokio::time::sleep;

use crate::dht::{config::Args, node::Node, routing::RoutingTable, utils::gen_secret};

/// Stores and maintains important runtime objects for the DHT
pub struct RuntimeContext {
    pub routing_table: Arc<tokio::sync::Mutex<RoutingTable>>,
    pub peer_store: Arc<tokio::sync::Mutex<HashMap<String, Vec<Node>>>>,
    pub node: Node,
    pub secret: Arc<Mutex<[u8; 16]>>,
    pub announce_log: Arc<Vec<String>>,
}

impl RuntimeContext {
    pub fn init(args: &Args) -> Self {
        let node_id = args.id.unwrap_or_else(|| {
            let mut rng = rand::thread_rng();
            rng.gen_range(0..64)
        });
        let routing_table = Arc::new(tokio::sync::Mutex::new(RoutingTable::new(node_id)));
        let peer_store = Arc::new(tokio::sync::Mutex::new(HashMap::<String, Vec<Node>>::new()));
        let node = Node::new(
            node_id,
            std::net::IpAddr::V4(Ipv4Addr::from_str("0.0.0.0").unwrap()),
            args.udp_port,
        );
        let secret = Arc::new(Mutex::new(gen_secret()));

        Self {
            routing_table,
            peer_store,
            node,
            secret,
            announce_log: Arc::new(vec![]),
        }
    }

    pub fn regen_token_task(self: Arc<Self>) {
        let secret_clone = self.secret.clone();
        // change secret every 10 min
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(600)).await;
                let mut sec = secret_clone.lock().unwrap();
                *sec = gen_secret();
            }
        });
    }

    pub async fn dump_state(&self) -> Value {
        let current_state = json!({
            "time": SystemTime::now(),
            "node": serde_json::to_string(&self.node).unwrap(),
            "peer_store": serde_json::to_string(&self.peer_store.lock().await.clone()).unwrap(),
            "routing_table": serde_json::to_string(&self.routing_table.lock().await.clone()).unwrap(),
        });

        current_state
    }
}

/// Interfacing with the DHT from an external client
/// Primarily for testing the network
/// To be implemented as a simple HTTP server supporting:
///     - PING
///     - SRC
///     - GET
///     - PUT
async fn start_testing_interface(port: u16) {
    todo!()
}
