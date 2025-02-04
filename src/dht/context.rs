use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::time::sleep;

use crate::{
    config::CONFIG,
    dht::{key::gen_secret, node::Node, routing::RoutingTable},
};

use super::key::{generate_node_id, Key};

/// Stores and maintains important runtime objects for the DHT
pub struct RuntimeContext {
    pub routing_table: Arc<tokio::sync::Mutex<RoutingTable>>,
    pub peer_store: Arc<tokio::sync::Mutex<HashMap<Key, Vec<SocketAddrV4>>>>,
    pub node: Node,
    pub secret: Arc<Mutex<[u8; 16]>>,
    pub announce_log: Arc<Vec<Key>>,
}

impl RuntimeContext {
    pub fn init(ip: Ipv4Addr) -> Self {
        let node_id = generate_node_id();
        let routing_table = Arc::new(tokio::sync::Mutex::new(RoutingTable::new(node_id)));
        let peer_store = Arc::new(tokio::sync::Mutex::new(
            HashMap::<Key, Vec<SocketAddrV4>>::new(),
        ));
        let node = Node::new(node_id, ip, CONFIG.network.dht_port); // TODO: public ip instead probably
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
}
