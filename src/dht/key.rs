use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

use rand::{rngs::OsRng, Rng, RngCore};
use serde::Serialize;
use serde_bytes::ByteBuf;

use crate::config::ID_SIZE;
// use hex;

// wrap the NodeId in a newtype to implement the Debug trait
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(pub [u8; ID_SIZE]);

// deserialize from bytes
impl<'de> serde::de::Deserialize<'de> for Key {
    fn deserialize<D>(deserializer: D) -> Result<Key, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        let buf = ByteBuf::deserialize(deserializer)?;
        Ok(Key::from(&buf))
    }
}

// serialize to string
impl serde::ser::Serialize for Key {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

impl std::cmp::PartialOrd for Key {
    fn partial_cmp(&self, other: &Key) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for Key {
    fn cmp(&self, other: &Key) -> std::cmp::Ordering {
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

impl From<String> for Key {
    fn from(s: String) -> Self {
        let id = hex::decode(s).expect("Invalid hash");
        id.into()
    }
}

impl From<[u8; ID_SIZE]> for Key {
    fn from(id: [u8; ID_SIZE]) -> Self {
        Key(id)
    }
}

impl From<Vec<u8>> for Key {
    fn from(vec: Vec<u8>) -> Self {
        let mut id = [0u8; ID_SIZE];
        id.copy_from_slice(&vec);
        Key(id)
    }
}

impl From<&[u8]> for Key {
    fn from(buf: &[u8]) -> Self {
        let mut id = [0u8; ID_SIZE];
        id.copy_from_slice(buf);
        Key(id)
    }
}

// from ByteBuf
impl From<&ByteBuf> for Key {
    fn from(buf: &ByteBuf) -> Self {
        let mut id = [0u8; ID_SIZE];
        id.copy_from_slice(buf);
        Key(id)
    }
}

impl Key {
    pub fn new(id: [u8; ID_SIZE]) -> Self {
        Key(id)
    }

    pub fn distance(&self, other: &Key) -> Key {
        let mut result = [0u8; ID_SIZE];
        for i in 0..ID_SIZE {
            result[i] = self.0[i] ^ other.0[i];
        }
        result.into()
    }

    pub fn to_string(&self) -> String {
        hex::encode(self.0)
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl AsRef<[u8]> for Key {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

pub fn generate_node_id() -> Key {
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

pub fn gen_trans_id() -> Vec<u8> {
    let mut rng = rand::thread_rng();
    let trans_id: u16 = rng.gen();
    format!("{:02x}", trans_id).as_bytes().to_vec()
}
