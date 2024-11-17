extern crate pretty_env_logger;
#[macro_use]
extern crate log;

use anyhow::{anyhow, Result};
use debug_ignore::DebugIgnore;
use dream::dht::kademlia::GetPeersResponse;
use dream::dht::utils::deserialize_compact_peers;
use http_req::request;
use log::{error, info};
use percent_encoding::{percent_encode, utf8_percent_encode, AsciiSet, CONTROLS, NON_ALPHANUMERIC};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Deserializer, Serialize};
use serde_bytes::ByteBuf;
use sha1::{Digest, Sha1};
use std::hash::{Hash, Hasher};
use std::net::{Ipv4Addr, SocketAddr};
use std::option::Option;
use std::str::FromStr;
use std::time::Duration;
use std::{fs::File, io::Read};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
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
    info: DebugIgnore<Info>,
}

impl Metafile {
    fn get_info_hash(&self) -> [u8; 20] {
        let bencode_info = serde_bencoded::to_vec(&self.info).unwrap_or_default();
        let mut hasher = Sha1::new();
        hasher.update(&bencode_info);
        let result = hasher.finalize();
        let mut output = [0u8; 20];
        output.copy_from_slice(&result[..]);
        output
    }
}

/* #[derive(Serialize, Debug, Hash)]
pub struct Hashes(Vec<[u8; 20]>);
let byte_buf = ByteBuf::deserialize(deserializer).unwrap();
let bytes = byte_buf.into_vec();
let chunks = bytes
    .chunks_exact(20)
    .map(|chunk| {
        let mut arr = [0u8; 20];
        arr.copy_from_slice(chunk);
        arr
    })
    .collect();
Ok(Hashes(chunks))
 */

#[derive(Serialize, Deserialize, Debug, Hash)]
struct FileInfo {
    length: u64,       // # of bytes
    path: Vec<String>, // path split up, last is filename (0 len is error)
    md5sum: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Info {
    name: ByteBuf,
    pieces: ByteBuf,
    #[serde(rename = "piece length")]
    piece_length: u32,

    // Single-file mode
    #[serde(skip_serializing_if = "Option::is_none")]
    length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    md5sum: Option<ByteBuf>,

    // Multi-file mode
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<Vec<FileInfo>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    private: Option<bool>,
}

fn none<T>() -> Option<T> {
    None
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
        let left = torrent_file.info.length.unwrap_or_default() as usize;
        Self {
            info_hash: form_urlencoded::byte_serialize(&torrent_file.get_info_hash()).collect(),
            peer_id,
            port,
            uploaded: 0,
            downloaded: 0,
            left,
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
        let port_ascii = self.port.to_string();
        let uploaded_ascii = self.uploaded.to_string();
        let downloaded_ascii = self.downloaded.to_string();
        let left_ascii = self.left.to_string();

        let mut url = Url::parse(tracker_url).expect("Invalid tracker URL");

        url.query_pairs_mut()
            .append_pair("peer_id", &self.peer_id)
            .append_pair("port", &port_ascii)
            .append_pair("uploaded", &uploaded_ascii)
            .append_pair("downloaded", &downloaded_ascii)
            .append_pair("left", &left_ascii)
            .append_pair("compact", "1");

        if let Some(event) = &self.event {
            let event_repr = match event {
                Event::Started => "started",
                Event::Completed => "completed",
                Event::Empty => "empty",
                Event::Stopped => "stopped",
            };
            url.query_pairs_mut().append_pair("event", event_repr);
        }

        format!("{}&info_hash={}", url, self.info_hash)
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

fn parse_torrent_file(file_path: &str) -> Result<Metafile> {
    let mut file = File::open(file_path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    let meta_file = serde_bencoded::from_bytes::<Metafile>(&buffer[..])?;
    Ok(meta_file)
}

#[derive(Clone, Debug)]
struct Peer {
    ip: Ipv4Addr,
    port: u16,
}

impl Peer {
    fn addr(&self) -> SocketAddr {
        SocketAddr::new(std::net::IpAddr::V4(self.ip), self.port)
    }
}

impl Peer {
    fn new(ip: Ipv4Addr, port: u16) -> Self {
        Self { ip, port }
    }
}

fn deserialize_peers<'de, D>(deserializer: D) -> Result<Vec<Peer>, D::Error>
where
    D: Deserializer<'de>,
{
    let bytes: ByteBuf = Deserialize::deserialize(deserializer)?;
    let mut peers = vec![];
    for curr_chunk in bytes.chunks(6) {
        if curr_chunk.len() == 6 {
            let ip = Ipv4Addr::new(curr_chunk[0], curr_chunk[1], curr_chunk[2], curr_chunk[3]);
            let port = u16::from_be_bytes([curr_chunk[4], curr_chunk[5]]);
            peers.push(Peer::new(ip, port))
        }
    }
    Ok(peers)
}

#[derive(Deserialize, Debug)]
struct TrackerResponse {
    interval: u64,
    #[serde(deserialize_with = "deserialize_peers")]
    peers: Vec<Peer>,
}

// 2. Retrieving peers from the tracker

fn get_peers_from_tracker(tracker_url: &str, body: TrackerRequest) -> Result<TrackerResponse> {
    let get_url = body.to_url(tracker_url);

    let mut body = Vec::new();
    let res = request::get(get_url, &mut body)?;
    info!("Status: {} {}", res.status_code(), res.reason());
    info!("Headers: {}", res.headers());
    info!("Body: {:?}", body);
    let res: TrackerResponse = serde_bencoded::from_bytes(&body)?;
    Ok(res)
}

struct RemotePeer {
    peer: Peer,
    am_choking: bool,      // = 1
    am_interested: bool,   // = 0
    peer_choking: bool,    // has this peer choked us? = 1
    peer_interested: bool, // is this peer interested in us? = 0
}

// 3. Handshake

async fn peer_handshake(peer: Peer, meta_file: Metafile, my_id: String) -> Result<TcpStream> {
    info!("Initiating peer handshake with {:#?}", peer);

    let mut handshake = [0u8; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    handshake[28..48].copy_from_slice(&meta_file.get_info_hash());
    handshake[48..68].copy_from_slice(my_id.as_bytes());

    let connect_timeout = Duration::from_secs(5);
    let mut stream = match timeout(connect_timeout, TcpStream::connect(peer.addr())).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            error!("Failed to connect: {}", e);
            return Err(anyhow!(e));
        }
        Err(_) => {
            error!("Connection attempt timed out");
            return Err(anyhow!("Connection timed out"));
        }
    };
    stream.write_all(&handshake).await?;

    let mut res = [0u8; 68];
    stream.read_exact(&mut res).await?;

    if res[0..20] == handshake[0..20] && res[28..48] == handshake[28..48] {
        Ok(stream)
    } else {
        error!("Handshake mismatch");
        Err(anyhow!("Handshake failed"))
    }
}

// TODO
// 4. Leeching
// 5. Seeding

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let peer_id: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(20)
        .map(char::from)
        .collect();

    info!("Initialized peer {peer_id}");

    let meta_file = parse_torrent_file("debian.torrent")?;

    info!("Parsed metafile: {:#?}", meta_file);

    let req = TrackerRequest::from_id_port(peer_id.clone(), 6881, &meta_file);
    info!("Sending request to tracker: {:#?}", req);

    let peers = get_peers_from_tracker(&meta_file.announce, req)?;

    info!("GET peers: {:#?}", peers);

    let mut conn = peer_handshake(peers.peers[0].clone(), meta_file, peer_id).await?;

    conn.shutdown().await?;

    Ok(())
}
