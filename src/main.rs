use http_req::request;
use serde::{Deserialize, Deserializer, Serialize};
use serde_bytes::ByteBuf;
use std::{fs::File, io::Read};
use url::form_urlencoded;

#[derive(Deserialize, Debug)]
struct Metafile {
    announce: String, // URL of the tracker
    comment: Option<String>,
    creation_date: Option<usize>,
    info: Info,
}

#[derive(Debug)]
pub struct Hashes(Vec<[u8; 20]>);

fn deserialize_pieces<'de, D>(deserializer: D) -> Result<Hashes, D::Error>
where
    D: Deserializer<'de>,
{
    let byte_buf = ByteBuf::deserialize(deserializer).unwrap();
    let bytes = byte_buf.into_vec();

    if bytes.len() % 20 != 0 {
        return Err(serde::de::Error::custom("Invalid length for pieces field"));
    }

    let chunks = bytes
        .chunks_exact(20)
        .map(|chunk| {
            let mut arr = [0u8; 20];
            arr.copy_from_slice(chunk);
            arr
        })
        .collect();

    Ok(Hashes(chunks))
}

#[derive(Deserialize, Debug)]
struct Info {
    name: String,

    #[serde(rename = "piece length")]
    piece_length: usize, // usually a power of 2, last piece may not be this length

    #[serde(deserialize_with = "deserialize_pieces")]
    pieces: Hashes, // split into str of len 20, each is sha-1 hash of piece

    length: Option<usize>,        // single-file case (# of bytes)
    files: Option<Vec<FileInfo>>, // multi-file case
}

#[derive(Serialize, Deserialize, Debug)]
struct FileInfo {
    length: usize,     // # of bytes
    path: Vec<String>, // path split up, last is filename (0 len is error)
}

struct TrackerRequest {
    info_hash: [u8; 20],
    peer_id: String,
    ip: String,
    port: u16,
    uploaded: usize,
    downloaded: usize,
    left: usize,
    event: Option<Event>,
    failure: Option<String>,
}

impl TrackerRequest {
    fn to_url(&self, tracker_url: &str) -> String {}
}

enum Event {
    Started,
    Completed,
    Stopped,
    Empty,
}

// 1. Parse .torrent file

fn parse_torrent_file(file_path: &str) -> Metafile {
    let mut file = File::open(file_path).unwrap();
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).unwrap();

    let meta_file = serde_bencode::from_bytes::<Metafile>(&buffer[..]).unwrap();
    meta_file
}

// 2. Retrieving peers from the tracker

fn get_peers_from_tracker(tracker_url: &str) {}

// 3. Parsing the tracker response
fn main() {
    let meta_file = parse_torrent_file("debian.torrent");

    println!("{:#?}", meta_file);
}
