use std::{cmp::Ordering, net::IpAddr};

// node participating in DHT
// in our bittorrent implementations, peers are also nodes
#[derive(Eq, PartialEq, Clone, Debug, serde::Serialize)]
pub struct Node {
    pub id: u32,
    pub ip: IpAddr,
    pub port: u16,
    // is_good: bool, // responded to our query or requested a query within past 15 min,
}

impl Node {
    pub fn new(id: u32, ip: IpAddr, port: u16) -> Self {
        Self { id, ip, port }
    }

    pub fn get_peer_compact_format(&self) -> String {
        let mut compact_info = [0u8; 6];

        if let IpAddr::V4(v4_addr) = self.ip {
            let ip = v4_addr.to_bits().to_le_bytes();
            compact_info[0..4].copy_from_slice(&ip);
        }

        let port = self.port.to_le_bytes();
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
