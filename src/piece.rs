use std::{fs::File, io::Write};

use crate::{tracker::Metafile, utils::hash_obj};

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
pub enum PieceStatus {
    Pending,
    Requested,
    Received,
}

pub struct Piece {
    pub buffer: Vec<u8>, // store raw bytes of data
    pub hash: [u8; 20],
    pub status: PieceStatus,
    pub downloaded_bytes: u32,
}

impl Piece {
    pub fn new(size: u64, hash: [u8; 20]) -> Self {
        Self {
            buffer: vec![0; size as usize],
            status: PieceStatus::Pending,
            downloaded_bytes: 0,
            hash,
        }
    }

    pub fn requested(&mut self) {
        self.status = PieceStatus::Requested;
    }

    pub fn retrieve_block(&mut self, begin: usize, len: usize) -> Option<Vec<u8>> {
        if begin + len < self.buffer.len() {
            Some(self.buffer[begin..(begin + len)].to_vec())
        } else {
            None
        }
    }

    pub fn store_block(&mut self, begin: usize, len: usize, data: &[u8]) {
        if begin + len < self.buffer.len() {
            self.buffer[begin..(begin + len)].copy_from_slice(data);
            self.downloaded_bytes += len as u32;

            if self.downloaded_bytes as usize == self.buffer.len() {
                self.status = PieceStatus::Received;
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
struct PieceStore<'a> {
    pub num_pieces: u32,
    pub meta_file: &'a Metafile, // contains
    pub pieces: Vec<Piece>,
}

impl<'a> PieceStore<'a> {
    pub fn new(meta_file: &'a Metafile) -> Self {
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

    pub fn persist(&self, idx: usize) {
        let piece = &self.pieces[idx];

        if piece.status == PieceStatus::Received {
            let mut file =
                File::create(format!("{}-{idx}.part", self.meta_file.info.name)).unwrap();
            file.write_all(&piece.buffer);
        }
    }

    pub fn get_status_bitfield(&self) -> BitField {
        let mut bitfield = BitField::new(self.num_pieces);

        for i in 0..self.num_pieces {
            if self.pieces[i as usize].status == PieceStatus::Received {
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