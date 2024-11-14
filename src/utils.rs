use crate::node::Node;
use rand::{rngs::OsRng, Rng, RngCore};
use std::net::{IpAddr, Ipv4Addr};
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

    let bytes = hex::decode(serialized_nodes.unwrap()).unwrap();

    for curr_chunk in bytes.chunks(7) {
        if curr_chunk.len() == 7 {
            let id = curr_chunk[0];
            let ip = Ipv4Addr::new(curr_chunk[1], curr_chunk[2], curr_chunk[3], curr_chunk[4]);
            info!("Ip decoded to {:#?}", ip);
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

pub fn deserialize_compact_peers(serialized_peers: Option<&String>) -> Vec<(IpAddr, u16)> {
    let mut peers = Vec::new();

    let bytes = hex::decode(serialized_peers.unwrap()).unwrap();

    println!("Decoded bytes {:#?}", bytes);

    for curr_chunk in bytes.chunks(6) {
        if curr_chunk.len() == 6 {
            let ip = Ipv4Addr::new(curr_chunk[0], curr_chunk[1], curr_chunk[2], curr_chunk[3]);
            let port = u16::from_be_bytes([curr_chunk[4], curr_chunk[5]]);
            peers.push((std::net::IpAddr::V4(ip), port))
        }
    }

    peers
}
