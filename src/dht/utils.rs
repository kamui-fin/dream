use std::{
    fs,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr},
    path::Path,
};

use log::info;
use rand::{rngs::OsRng, Rng, RngCore};
use serde::Serialize;
use serde_bytes::ByteBuf;

use crate::dht::node::Node;
// use hex;

pub const ID_SIZE: usize = 20; // 160 bits

// wrap the NodeId in a newtype to implement the Debug trait
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct HashId(pub [u8; ID_SIZE]);

impl std::fmt::Debug for HashId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

impl std::cmp::PartialOrd for HashId {
    fn partial_cmp(&self, other: &HashId) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for HashId {
    fn cmp(&self, other: &HashId) -> std::cmp::Ordering {
        for i in 0..ID_SIZE {
            if self.0[i] < other.0[i] {
                return std::cmp::Ordering::Less;
            } else if self.0[i] > other.0[i] {
                return std::cmp::Ordering::Greater;
            }
        }
        std::cmp::Ordering::Equal
    }
}

impl From<String> for HashId {
    fn from(s: String) -> Self {
        let id = hex::decode(s).expect("Invalid hash");
        id.into()
    }
}

impl From<[u8; ID_SIZE]> for HashId {
    fn from(id: [u8; ID_SIZE]) -> Self {
        HashId(id)
    }
}

impl From<Vec<u8>> for HashId {
    fn from(vec: Vec<u8>) -> Self {
        let mut id = [0u8; ID_SIZE];
        id.copy_from_slice(&vec);
        HashId(id)
    }
}

impl From<&[u8]> for HashId {
    fn from(buf: &[u8]) -> Self {
        let mut id = [0u8; ID_SIZE];
        id.copy_from_slice(buf);
        HashId(id)
    }
}

// from ByteBuf
impl From<&ByteBuf> for HashId {
    fn from(buf: &ByteBuf) -> Self {
        let mut id = [0u8; ID_SIZE];
        id.copy_from_slice(&buf);
        HashId(id)
    }
}

impl HashId {
    pub fn new(id: [u8; ID_SIZE]) -> Self {
        HashId(id)
    }

    pub fn distance(&self, other: &HashId) -> HashId {
        let mut result = [0u8; ID_SIZE];
        for i in 0..ID_SIZE {
            result[i] = self.0[i] ^ other.0[i];
        }
        result.into()
    }

    pub fn to_string(&self) -> String {
        hex::encode(&self.0)
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl std::fmt::Display for HashId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

pub fn generate_node_id() -> HashId {
    let path = "node_id.bin";

    if Path::new(path).exists() {
        let mut file = fs::File::open(path).expect("Unable to open file");
        let mut id = [0u8; 20];
        file.read_exact(&mut id).expect("Unable to read data");
        id.into()
    } else {
        let mut rng = rand::thread_rng();
        let mut id = [0u8; 20];
        rng.fill(&mut id);
        let mut file = fs::File::create(path).expect("Unable to create file");
        file.write_all(&id).expect("Unable to write data");
        id.into()
    }
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
            nodes.push(Node::new(id.into(), std::net::IpAddr::V4(ip), port));
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
