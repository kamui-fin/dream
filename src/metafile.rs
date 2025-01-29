use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use url::Url;

use crate::utils::hash_obj;

pub struct Hashes(pub Vec<[u8; 20]>);

#[derive(Clone, Deserialize, Debug)]
pub struct Metafile {
    pub announce: Option<String>,
    #[serde(default)]
    #[serde(rename = "announce-list")]
    pub announce_list: Vec<Vec<String>>,
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
    pub fn parse_torrent_file(file_path: PathBuf) -> anyhow::Result<Metafile> {
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
            announce: None,
            announce_list: Vec::new(),
            comment,
            creation_date: Some(creation_date),
            info,
        }
    }

    pub fn get_announce(&self) -> String {
        if let Some(announce) = &self.announce {
            return announce.clone();
        } else if !self.announce_list.is_empty() {
            for tier in &self.announce_list {
                for tracker in tier {
                    let url = Url::parse(&tracker).expect("Invalid tracker URL");
                    if url.scheme() == "http" || url.scheme() == "https" {
                        return tracker.clone();
                    }
                }
            }
        }
        panic!("No announce URL found");
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
