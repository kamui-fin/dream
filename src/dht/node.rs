use std::{
    cmp::Ordering,
    net::{Ipv4Addr, SocketAddrV4},
    time::{SystemTime, UNIX_EPOCH},
};

use super::key::Key;

// node participating in DHT
// in our bittorrent implementations, peers are also nodes
#[derive(Eq, PartialEq, Clone, Debug, serde::Serialize)]
pub struct Node {
    pub id: Key,
    pub addr: SocketAddrV4,
    pub last_seen: u64,
}

impl Node {
    pub fn new(id: Key, ip: Ipv4Addr, port: u16) -> Self {
        Self {
            id,
            addr: SocketAddrV4::new(ip, port),
            last_seen: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }

    pub fn from_addr(id: Key, addr: SocketAddrV4) -> Self {
        Self {
            id,
            addr,
            last_seen: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }

    pub fn is_questionable(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // 15 minute timer
        now - self.last_seen >= 60
    }

    pub fn update_last_seen(&mut self) {
        self.last_seen = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
    }
}

#[derive(Clone, Eq, PartialEq, Debug, serde::Serialize)]
pub struct NodeDistance {
    pub node: Node,
    pub dist: Key,
}

impl Ord for NodeDistance {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist.cmp(&other.dist)
    }
}

impl PartialOrd for NodeDistance {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.dist.partial_cmp(&other.dist)
    }
}
