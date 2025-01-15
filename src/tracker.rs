use std::{
    fs,
    fs::File,
    io::Read,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use http_req::request;
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use url::{form_urlencoded, Url};

use crate::{
    peer::{deserialize_peers, ConnectionInfo, DREAM_ID},
    utils::hash_obj,
    PORT,
};

pub struct Hashes(pub Vec<[u8; 20]>);

#[derive(Clone, Deserialize, Debug)]
pub struct Metafile {
    pub announce: String, // URL of the tracker
    pub comment: Option<String>,
    pub creation_date: Option<usize>,
    pub info: Info,
}

#[derive(Clone, Serialize, Deserialize, Debug, Hash)]
pub struct FileInfo {
    pub length: u64,       // # of bytes
    pub path: Vec<String>, // path split up, last is filename (0 len is error)
    pub md5sum: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Info {
    pub name: String,
    pub pieces: ByteBuf,

    #[serde(rename = "piece length")]
    pub piece_length: u32,

    // Single-file mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5sum: Option<ByteBuf>,

    // Multi-file mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileInfo>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
}

impl Metafile {
    pub fn parse_torrent_file(file_path: &str) -> Result<Metafile> {
        let mut file = File::open(file_path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        let meta_file = serde_bencoded::from_bytes::<Metafile>(&buffer[..])?;

        Ok(meta_file)
    }
    pub fn from_video(input_path: &Path, piece_length: u32, comment: Option<String>) -> Self {
        let file_data = fs::read(input_path).expect("cannot read file");
        let file_length = file_data.len();
        let file_name = input_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        // chunk up
        let mut pieces = Vec::new();
        for chunk in file_data.chunks(piece_length as usize) {
            let hash = hash_obj(chunk);
            pieces.extend_from_slice(&hash);
        }

        let info = Info {
            name: file_name,
            pieces: ByteBuf::from(pieces),
            piece_length,
            length: Some(file_length as u64),
            md5sum: None,
            files: None,
            private: None,
        };

        let creation_date = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs() as usize;

        Metafile {
            announce: String::new(),
            comment,
            creation_date: Some(creation_date),
            info,
        }
    }

    // TODO: repeated computation.. cache this
    pub fn get_info_hash(&self) -> [u8; 20] {
        let bencode_info = serde_bencoded::to_vec(&self.info).unwrap_or_default();
        hash_obj(bencode_info)
    }

    pub fn get_piece_hashes(&self) -> Hashes {
        let bytes = &self.info.pieces;
        let chunks = bytes
            .chunks_exact(20)
            .map(|chunk| {
                let mut arr = [0u8; 20];
                arr.copy_from_slice(chunk);
                arr
            })
            .collect();
        Hashes(chunks)
    }

    pub fn get_num_pieces(&self) -> u32 {
        (self.info.length.unwrap() as f32 / self.info.piece_length as f32).ceil() as u32
    }

    pub fn get_piece_len(&self, idx: usize) -> u64 {
        if idx as u32 == self.get_num_pieces() - 1 {
            self.info.length.unwrap() % self.info.piece_length as u64
        } else {
            self.info.piece_length as u64
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Started,
    Completed,
    Stopped,
    Empty,
}

#[derive(Debug)]
pub struct TrackerRequest {
    pub info_hash: String,
    pub uploaded: usize,
    pub downloaded: usize,
    pub left: usize,
    pub event: Option<Event>,
}

impl TrackerRequest {
    pub fn new(torrent_file: &Metafile) -> Self {
        let left = torrent_file.info.length.unwrap_or_default() as usize;

        Self {
            info_hash: form_urlencoded::byte_serialize(&torrent_file.get_info_hash()).collect(),
            uploaded: 0,
            downloaded: 0,
            left,
            event: None,
        }
    }

    pub fn with_event(self, event: Event) -> Self {
        Self {
            event: Some(event),
            ..self
        }
    }

    pub fn to_url(&self, tracker_url: &str) -> String {
        let port_ascii = PORT.to_string();
        let uploaded_ascii = self.uploaded.to_string();
        let downloaded_ascii = self.downloaded.to_string();
        let left_ascii = self.left.to_string();

        let mut url = Url::parse(tracker_url).expect("Invalid tracker URL");

        url.query_pairs_mut()
            .append_pair("peer_id", &DREAM_ID)
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

#[derive(Deserialize, Debug)]
pub struct TrackerResponse {
    pub interval: u64,
    #[serde(deserialize_with = "deserialize_peers")]
    pub peers: Vec<ConnectionInfo>,
}

pub fn get_peers_from_tracker(tracker_url: &str, body: TrackerRequest) -> Result<TrackerResponse> {
    let get_url = body.to_url(tracker_url);

    let mut body = Vec::new();
    let _ = request::get(get_url, &mut body)?;
    let res: TrackerResponse = serde_bencoded::from_bytes(&body)?;
    Ok(res)
}
