use std::{
    fs,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr},
    path::Path,
};

use log::info;
use rand::{rngs::OsRng, Rng, RngCore};

use crate::dht::node::Node;
// use hex;

pub const ID_SIZE: usize = 20; // 160 bits

pub type NodeId = [u8; ID_SIZE];

pub fn vec_to_id(vec: &Vec<u8>) -> NodeId {
    let mut result = [0u8; ID_SIZE];
    result.copy_from_slice(vec);
    result
}

pub fn decode_node_id(node_id: String) -> NodeId {
    let node_id = hex::decode(node_id).expect("Invalid hash");
    let mut result = [0u8; ID_SIZE];
    result.copy_from_slice(&node_id);
    result
}

pub fn generate_node_id() -> NodeId {
    let path = "node_id.bin";

    if Path::new(path).exists() {
        let mut file = fs::File::open(path).expect("Unable to open file");
        let mut id = [0u8; 20];
        file.read_exact(&mut id).expect("Unable to read data");
        id
    } else {
        let mut rng = rand::thread_rng();
        let mut id = [0u8; 20];
        rng.fill(&mut id);
        let mut file = fs::File::create(path).expect("Unable to create file");
        file.write_all(&id).expect("Unable to write data");
        id
    }
}

pub fn xor_id(id1: &NodeId, id2: &NodeId) -> NodeId {
    let mut result = [0u8; ID_SIZE];
    for i in 0..ID_SIZE {
        result[i] = id1[i] ^ id2[i];
    }
    result
}

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

pub fn deserialize_compact_node(bytes: &Vec<u8>) -> Vec<Node> {
    let mut nodes = Vec::new();

    for curr_chunk in bytes.chunks(26) {
        if curr_chunk.len() == 26 {
            let mut id = [0u8; 20];
            id.copy_from_slice(&curr_chunk[0..20]);
            let ip = Ipv4Addr::new(
                curr_chunk[20],
                curr_chunk[21],
                curr_chunk[22],
                curr_chunk[23],
            );
            let port = u16::from_be_bytes([curr_chunk[24], curr_chunk[25]]);
            nodes.push(Node::new(id, std::net::IpAddr::V4(ip), port));
        }
    }

    nodes
}

pub fn deserialize_compact_peers(bytes: &Vec<u8>) -> Vec<(IpAddr, u16)> {
    let mut peers = Vec::new();

    for curr_chunk in bytes.chunks(6) {
        if curr_chunk.len() == 6 {
            let ip = Ipv4Addr::new(curr_chunk[0], curr_chunk[1], curr_chunk[2], curr_chunk[3]);
            let port = u16::from_be_bytes([curr_chunk[4], curr_chunk[5]]);
            peers.push((std::net::IpAddr::V4(ip), port))
        }
    }

    peers
}
