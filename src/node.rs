// node participating in DHT
// in our bittorrent implementations, peers are also nodes
#[derive(Eq, PartialEq, Clone, Debug)]
struct Node {
    id: u32,
    ip: IpAddr,
    port: u16,
    // is_good: bool, // responded to our query or requested a query within past 15 min,
}

impl Node {
    fn get_peer_compact_format(&self) -> String {
        let mut compact_info = [0u8; 6];

        if let IpAddr::V4(v4_addr) = self.ip {
            let ip = v4_addr.to_bits().to_le_bytes();
            compact_info[0..4].copy_from_slice(&ip);
        }

        let port = self.port.to_le_bytes();
        compact_info[4..6].copy_from_slice(&port);

        format!("{:?}", compact_info)
    }

    fn get_node_compact_format(&self) -> String {
        let mut compact_info = [0u8; 7];
        compact_info[0] = self.id as u8;

        if let IpAddr::V4(v4_addr) = self.ip {
            let ip = v4_addr.to_bits().to_le_bytes();
            compact_info[1..5].copy_from_slice(&ip);
        }

        let port = self.port.to_le_bytes();
        compact_info[5..7].copy_from_slice(&port);

        format!("{:?}", compact_info)
    }
}

fn deserialize_compact_node(serialized_nodes: Option<&String>) -> Vec<Node> {
    let mut nodes = Vec::new();

    let bytes = serialized_nodes.unwrap().as_bytes();

    for curr_chunk in bytes.chunks(7) {
        if curr_chunk.len() == 7 {
            let id = curr_chunk[0];
            let ip = Ipv4Addr::new(curr_chunk[1], curr_chunk[2], curr_chunk[3], curr_chunk[4]);
            let port = u16::from_be_bytes([curr_chunk[5], curr_chunk[6]]);

            nodes.push(Node {
                id: id.into(),
                port,
                ip: std::net::IpAddr::V4(ip),
            })
        }
    }

    nodes
}

#[derive(Clone, Eq, PartialEq)]
struct NodeDistance {
    node: Node,
    dist: u32,
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
