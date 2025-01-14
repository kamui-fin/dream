use std::{
    cmp::Ordering,
    net::IpAddr,
    time::{SystemTime, UNIX_EPOCH},
};

// node participating in DHT
// in our bittorrent implementations, peers are also nodes
#[derive(Eq, PartialEq, Clone, Debug, serde::Serialize)]
pub struct Node {
    pub id: u32,
    pub ip: IpAddr,
    pub port: u16,
    pub last_seen: u64,
}

impl Node {
    pub fn new(id: u32, ip: IpAddr, port: u16) -> Self {
        Self {
            id,
            ip,
            port,
            last_seen: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }

    pub fn get_peer_compact_format(&self) -> String {
        let mut compact_info = [0u8; 6];

        if let IpAddr::V4(v4_addr) = self.ip {
            let ip = v4_addr.octets();
            compact_info[0..4].copy_from_slice(&ip);
        }

        let port = self.port.to_be_bytes();
        compact_info[4..6].copy_from_slice(&port);

        hex::encode(compact_info)
    }

    pub fn get_node_compact_format(&self) -> String {
        let mut compact_info = [0u8; 7];
        compact_info[0] = self.id as u8;

        if let IpAddr::V4(v4_addr) = self.ip {
            let ip = v4_addr.octets();
            compact_info[1..5].copy_from_slice(&ip);
        }

        let port = self.port.to_be_bytes();
        compact_info[5..7].copy_from_slice(&port);

        hex::encode(compact_info)
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
    pub dist: u32,
}

impl Ord for NodeDistance {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist.cmp(&other.dist)
    }
}

impl PartialOrd for NodeDistance {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
