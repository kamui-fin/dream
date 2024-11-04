use rand::{rngs::OsRng, Rng, RngCore};
use std::net::Ipv4Addr;
use crate::node::Node;
// use hex;

pub fn gen_secret() -> [u8; 16] {
    let mut secret = [0u8; 16];
    OsRng.fill_bytes(&mut secret);
    secret
}

pub fn gen_trans_id() -> String {
    let mut rng = rand::thread_rng();
    let trans_id: u16 = rng.gen();
    format!("{:02x}", trans_id)
}

pub fn deserialize_compact_node(serialized_nodes: Option<&String>) -> Vec<Node> {
    let mut nodes = Vec::new();

    let bytes = hex::decode(hex::encode(serialized_nodes.unwrap())).unwrap();

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
