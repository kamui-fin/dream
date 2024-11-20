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
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use url::{form_urlencoded, Url};
use url_encoded_data::UrlEncodedData;

const PORT: u16 = 6881;

fn slice_to_u32_msb(bytes: &[u8]) -> u32 {
    // Ensure the slice has exactly 4 bytes
    let array: [u8; 4] = bytes.try_into().unwrap();
    u32::from_be_bytes(array)
}

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
    fn new(ip: Ipv4Addr, port: u16) -> Self {
        Self { ip, port }
    }
    fn addr(&self) -> SocketAddr {
        SocketAddr::new(std::net::IpAddr::V4(self.ip), self.port)
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
        stream.shutdown().await?;
        error!("Handshake mismatch");
        Err(anyhow!("Handshake failed"))
    }
}

#[derive(Debug)]
enum MessageType {
    KeepAlive,
    Choke,
    UnChoke,
    Interested,
    NotInterested,
    Have,
    Bitfield,
    Request,
    Piece,
    Cancel,
    Port,
}

impl MessageType {
    fn from_id(msg_id: Option<u8>) -> MessageType {
        if let Some(msg_id) = msg_id {
            match msg_id {
                0 => Self::Choke,
                1 => Self::UnChoke,
                2 => Self::Interested,
                3 => Self::NotInterested,
                4 => Self::Have,
                5 => Self::Bitfield,
                6 => Self::Request,
                7 => Self::Piece,
                8 => Self::Cancel,
                9 => Self::Port,
                _ => Self::KeepAlive, // TODO
            }
        } else {
            MessageType::KeepAlive
        }
    }

    fn to_id(&self) -> u8 {
        match self {
            Self::Choke => 0,
            Self::UnChoke => 1,
            Self::Interested => 2,
            Self::NotInterested => 3,
            Self::Have => 4,
            Self::Bitfield => 5,
            Self::Request => 6,
            Self::Piece => 7,
            Self::Cancel => 8,
            Self::Port => 9,
            Self::KeepAlive => 10,
        }
    }
}

#[derive(Debug)]
struct Message {
    msg_type: MessageType,
    payload: Vec<u8>,
}

impl Message {
    fn parse(buf: &[u8]) -> Self {
        let msg_length = slice_to_u32_msb(&buf[0..4]);
        let msg_id = if msg_length == 0 { None } else { Some(buf[4]) };
        let msg_type = MessageType::from_id(msg_id);

        let payload_size = msg_length - 1;
        let payload = buf
            .get(5..(5 + payload_size as usize))
            .map(|b| b.to_owned())
            .unwrap_or_default();

        Message { msg_type, payload }
    }

    fn serialize(self) -> Vec<u8> {
        let msg_length = (self.payload.len() + 1) as u32;
        let msg_length = msg_length.to_be_bytes();
        let msg_id = self.msg_type.to_id();

        let mut msg_buf: Vec<u8> = vec![];
        msg_buf.extend(msg_length);
        msg_buf.push(msg_id);
        msg_buf.extend(self.payload);

        msg_buf
    }
}

async fn start_server(port: u16) -> Result<()> {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await?;

    loop {
        let (mut socket, addr) = listener.accept().await?;
        println!("New connection from {:?}", addr);

        tokio::spawn(async move {
            let mut buf = [0; 2028];

            loop {
                match socket.read(&mut buf).await {
                    Ok(0) => {
                        println!("Connection closed by {:?}", addr);
                        break;
                    }
                    Ok(n) => {
                        let bt_msg = Message::parse(&buf);
                        info!("Received msg: {:#?}", bt_msg);

                        handle_msg(bt_msg).await;
                    }
                    Err(e) => {
                        println!("Failed to read from socket; err = {:?}", e);
                        break;
                    }
                }
            }
        });
    }
}

async fn handle_msg(bt_msg: Message) {
    todo!()
}

struct BitField(Vec<u8>);

impl BitField {
    fn piece_exists(&self, index: usize) -> bool {
        let byte_idx = index / 8;
        let byte = self.0[byte_idx];
        let offset = index % 8;

        ((byte >> (8 - offset - 1)) % 1) == 1
    }

    // fn mark_piece(&self, index: usize) -> bool {
    //     let byte
    // }
}

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

    // let req = TrackerRequest::from_id_port(peer_id.clone(), PORT, &meta_file);
    // info!("Sending request to tracker: {:#?}", req);
    // let peers = get_peers_from_tracker(&meta_file.announce, req)?;
    // info!("GET peers: {:#?}", peers);

    // e.g. connecting with a peer
    // let mut conn = peer_handshake(peers.peers[0].clone(), meta_file, peer_id).await?;

    // Listen on port 6881 for incoming requests
    start_server(PORT).await?;

    Ok(())
}