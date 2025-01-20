use std::{
    collections::HashMap,
    net::Ipv4Addr,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use serde_json::{json, Value};
use tokio::time::sleep;

use crate::dht::{config::Args, node::Node, routing::RoutingTable, utils::gen_secret};

use super::utils::{generate_node_id, NodeId};

/// Stores and maintains important runtime objects for the DHT
pub struct RuntimeContext {
    pub routing_table: Arc<tokio::sync::Mutex<RoutingTable>>,
    pub peer_store: Arc<tokio::sync::Mutex<HashMap<NodeId, Vec<Node>>>>,
    pub node: Node,
    pub secret: Arc<Mutex<[u8; 16]>>,
    pub announce_log: Arc<Vec<NodeId>>,
}

impl RuntimeContext {
    pub fn init(args: &Args) -> Self {
        let node_id = generate_node_id();
        let routing_table = Arc::new(tokio::sync::Mutex::new(RoutingTable::new(node_id)));
        let peer_store = Arc::new(tokio::sync::Mutex::new(HashMap::<NodeId, Vec<Node>>::new()));
        let node = Node::new(
            node_id,
            std::net::IpAddr::V4(Ipv4Addr::from_str("0.0.0.0").unwrap()),
            args.port,
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
        // change secret every 10
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
