use http_req::request;
use percent_encoding::{percent_encode, utf8_percent_encode, AsciiSet, CONTROLS, NON_ALPHANUMERIC};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Deserializer, Serialize};
use serde_bytes::ByteBuf;
use sha1::{Digest, Sha1};
use std::hash::{Hash, Hasher};
use std::{fs::File, io::Read};
use url::{form_urlencoded, Url};
use url_encoded_data::UrlEncodedData;

const FRAGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'%');

#[derive(Deserialize, Debug)]
struct Metafile {
    announce: String, // URL of the tracker
    comment: Option<String>,
    creation_date: Option<usize>,
    info: Info,
}

fn compute_sha1_hash<T: Hash>(data: &T) -> [u8; 20] {
    let mut hasher = Sha1::new();
    let mut s = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut s);

    let hash_bytes = s.finish().to_ne_bytes();
    hasher.update(&hash_bytes);

    let result = hasher.finalize();
    let mut output = [0u8; 20];
    output.copy_from_slice(&result[..]);

    output
}

impl Metafile {
    fn get_info_hash(&self) -> [u8; 20] {
        compute_sha1_hash(&self.info)
    }
}

#[derive(Debug, Hash)]
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

#[derive(Deserialize, Debug, Hash)]
struct Info {
    name: String,

    #[serde(rename = "piece length")]
    piece_length: usize, // usually a power of 2, last piece may not be this length

    #[serde(deserialize_with = "deserialize_pieces")]
    pieces: Hashes, // split into str of len 20, each is sha-1 hash of piece

    length: Option<usize>,        // single-file case (# of bytes)
    files: Option<Vec<FileInfo>>, // multi-file case
}

#[derive(Serialize, Deserialize, Debug, Hash)]
struct FileInfo {
    length: usize,     // # of bytes
    path: Vec<String>, // path split up, last is filename (0 len is error)
}

#[derive(Debug)]
struct TrackerRequest {
    info_hash: String,
    peer_id: String,
    port: u16,
    uploaded: usize,
    downloaded: usize,
    left: usize,
    event: Option<Event>,
}

impl TrackerRequest {
    fn from_id_port(peer_id: String, port: u16, torrent_file: &Metafile) -> Self {
        Self {
            info_hash: form_urlencoded::byte_serialize(&torrent_file.get_info_hash()).collect(),
            peer_id,
            port,
            uploaded: 0,
            downloaded: 0,
            left: torrent_file.info.length.unwrap(), // TODO: assuming single file
            event: None,
        }
    }

    fn with_event(self, event: Event) -> Self {
        Self {
            event: Some(event),
            ..self
        }
    }

    fn to_url(&self, tracker_url: &str) -> String {
        let info_hash_escaped: String =
            percent_encode(&self.info_hash.clone().into_bytes(), NON_ALPHANUMERIC).to_string();
        let peer_id_escaped: String =
            percent_encode(&self.peer_id.clone().into_bytes(), NON_ALPHANUMERIC).to_string();

        let port_ascii = self.port.to_string();
        let uploaded_ascii = self.uploaded.to_string();
        let downloaded_ascii = self.downloaded.to_string();
        let left_ascii = self.left.to_string();

        let mut url = Url::parse(tracker_url).expect("Invalid tracker URL");

        url.query_pairs_mut()
            .append_pair("info_hash", &info_hash_escaped)
            .append_pair("peer_id", &peer_id_escaped)
            .append_pair("port", &port_ascii)
            .append_pair("uploaded", &uploaded_ascii)
            .append_pair("downloaded", &downloaded_ascii)
            .append_pair("left", &left_ascii);

        if let Some(event) = &self.event {
            let event_repr = match event {
                Event::Started => "started",
                Event::Completed => "completed",
                Event::Empty => "empty",
                Event::Stopped => "stopped",
            };
            url.query_pairs_mut().append_pair("event", event_repr);
        }

        url.into()
    }
}

#[derive(Debug)]
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

fn get_peers_from_tracker(tracker_url: &str, body: TrackerRequest) {
    let get_url = body.to_url(tracker_url);
    println!("Url: {}", get_url);

    let mut body = Vec::new();
    let res = request::get(get_url, &mut body);
    let res = res.unwrap();
    println!("Status: {} {}", res.status_code(), res.reason());
    println!("Headers: {}", res.headers());
    //println!("{}", String::from_utf8_lossy(&body));
}

// 3. Parsing the tracker response
fn main() {
    let peer_id: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(20)
        .map(char::from)
        .collect();
    let meta_file = parse_torrent_file("debian.torrent");
    println!("{:#?}", meta_file.announce);

    let req = TrackerRequest::from_id_port(peer_id, 6881, &meta_file);
    println!("{:#?}", req);

    get_peers_from_tracker(&meta_file.announce, req);
}
