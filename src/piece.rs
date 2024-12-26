use log::{info, trace};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    vec,
};

use crate::{tracker::Metafile, utils::hash_obj};

pub const BLOCK_SIZE: u32 = (2 as u32).pow(14);

#[derive(Debug)]
pub struct BitField(pub Vec<u8>);

impl BitField {
    pub fn new(n: u32) -> Self {
        Self(vec![0; (n as f32 / 8f32).ceil() as usize])
    }

    pub fn piece_exists(&self, index: u32) -> bool {
        let byte_idx = index / 8;
        let byte = self.0[byte_idx as usize];
        let offset = index % 8;

        ((byte >> (8 - offset - 1)) & 1) == 1
    }

    pub fn mark_piece(&mut self, index: u32) {
        let byte_idx = index / 8;
        let offset = index % 8;

        self.0[byte_idx as usize] |= 1 << (8 - offset);
    }
}

#[derive(PartialEq)]
pub enum RequestStatus {
    Pending,
    Requested,
    Received,
}

pub struct Piece {
    pub buffer: Vec<u8>, // store raw bytes of data
    pub hash: [u8; 20],
    pub status: RequestStatus,
    pub block_set: HashSet<u32>,
    pub downloaded_bytes: u32,
}

impl Piece {
    pub fn new(size: u64, hash: [u8; 20]) -> Self {
        Self {
            buffer: vec![0; size as usize],
            status: RequestStatus::Pending,
            block_set: HashSet::new(),
            downloaded_bytes: 0,
            hash,
        }
    }

    // TODO: set requested status

    pub fn retrieve_block(&mut self, begin: usize, len: usize) -> Option<Vec<u8>> {
        if begin + len < self.buffer.len() {
            Some(self.buffer[begin..(begin + len)].to_vec())
        } else {
            None
        }
    }

    // can we really assume serial downloads?
    // also we need a mapping between block_idx <---> (begin, len)
    pub fn store_block(&mut self, begin: usize, len: usize, data: &[u8]) {
        if begin + len < self.buffer.len() {
            let block_id = ((begin / len) as f32).floor() as u32;
            self.block_set.insert(block_id);

            self.buffer[begin..(begin + len)].copy_from_slice(data);
            self.downloaded_bytes += len as u32;

            info!(
                "Block stored, currently recieved {} out of {} bytes",
                self.downloaded_bytes,
                self.buffer.len()
            );

            if self.downloaded_bytes as usize == self.buffer.len() {
                self.status = RequestStatus::Received;
                info!("The whole piece has been recieved");
            }
        }
    }

    pub fn verify_hash(&self) -> bool {
        let piece_hash = hash_obj(&self.buffer);
        let orig_hash = self.hash;

        orig_hash == piece_hash
    }
}

// Note: Avg block size is 2 ^ 14
pub struct PieceStore {
    pub num_pieces: u32,
    pub meta_file: Metafile,
    // TODO: kinda doesn't make sense to have a whole vector when operating on piece one at a time, but mainly just to allow future optimizations for downloading multiple pieces concurrently
    // not sure if it will ever make sense though even from a protocol standpoint, TBD..
    pub pieces: Vec<Piece>,
}

impl PieceStore {
    pub fn new(meta_file: Metafile) -> Self {
        let hashes = meta_file.get_piece_hashes();
        let num_pieces = meta_file.get_num_pieces();

        let mut pieces = vec![];
        for idx in 0..num_pieces {
            pieces.push(Piece::new(
                meta_file.get_piece_len(idx as usize),
                hashes.0[idx as usize],
            ));
        }

        Self {
            num_pieces,
            pieces,
            meta_file,
        }
    }

    pub fn persist(&self, idx: usize, output_dir: &Path) -> anyhow::Result<()> {
        let piece = &self.pieces[idx];

        if piece.status == RequestStatus::Received {
            let output_path = output_dir.join(format!("{}-{idx}.part", self.meta_file.info.name));
            let mut file = File::create(output_path)?;
            file.write_all(&piece.buffer)?;
        }

        Ok(())
    }

    pub fn concat(&self, output_dir: &Path) -> anyhow::Result<()> {
        let piece_files = (0..(self.num_pieces))
            .map(|idx| output_dir.join(format!("{}-{idx}.part", self.meta_file.info.name)));

        let dest_file_path = output_dir.join(&self.meta_file.info.name);
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(dest_file_path)?;

        for input_path in piece_files {
            let mut input = File::open(input_path)?;
            let mut buffer = Vec::new();
            input.read_to_end(&mut buffer)?;
            output.write_all(&buffer)?;
        }

        Ok(())
    }

    pub fn get_status_bitfield(&self) -> BitField {
        let mut bitfield = BitField::new(self.num_pieces);

        for i in 0..self.num_pieces {
            if self.pieces[i as usize].status == RequestStatus::Received {
                bitfield.mark_piece(i);
            }
        }

        bitfield
    }

    pub fn reset_piece(&mut self, idx: usize) {
        self.pieces[idx] = Piece::new(
            self.meta_file.get_piece_len(idx as usize),
            self.meta_file.get_piece_hashes().0[idx as usize],
        )
    }
}
