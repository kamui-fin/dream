use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Write,
    sync::atomic::AtomicU32,
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

    pub fn requested(&mut self) {
        self.status = RequestStatus::Requested;
    }

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

            if self.downloaded_bytes as usize == self.buffer.len() {
                self.status = RequestStatus::Received;
            }
        }
    }

    pub fn verify_hash(&self) -> bool {
        let piece_hash = hash_obj(&self.buffer);
        let orig_hash = self.hash;

        orig_hash == piece_hash
    }

    pub fn block_exists(&self, idx: u32) -> bool {
        self.block_set.contains(&idx)
    }
}

// maybe implement Iter
// tells us which block to get next
#[derive(Default)]
pub struct DownloadMeta {
    next_piece: AtomicU32,
    next_block: AtomicU32,
}

impl DownloadMeta {
    fn increment(&self, piece_size: u32) {
        let block_id = self.next_block.load(std::sync::atomic::Ordering::SeqCst);
        let last_byte_num = (block_id + 1) * BLOCK_SIZE;

        if last_byte_num < piece_size {
            self.next_block
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        } else {
            // check if we're done with all pieces
            self.next_piece
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.next_block
                .store(0, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

// Note: Avg block size is 2 ^ 14
pub struct PieceStore {
    pub num_pieces: u32,
    pub meta_file: Metafile, // contains
    pub pieces: Vec<Piece>,

    download_meta: DownloadMeta,
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
            download_meta: DownloadMeta::default(),
        }
    }

    pub fn persist(&self, idx: usize) {
        let piece = &self.pieces[idx];

        if piece.status == RequestStatus::Received {
            let mut file =
                File::create(format!("{}-{idx}.part", self.meta_file.info.name)).unwrap();
            file.write_all(&piece.buffer);
        }
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

    pub fn get_next_block_dl(&self) -> Option<(u32, u32)> {
        let piece_id = self
            .download_meta
            .next_piece
            .load(std::sync::atomic::Ordering::SeqCst);

        if piece_id == self.num_pieces {
            return None;
        }

        let block_id = self
            .download_meta
            .next_block
            .load(std::sync::atomic::Ordering::SeqCst);

        let piece = &self.pieces[piece_id as usize];
        self.download_meta.increment(piece.buffer.len() as u32);

        // we can keep this function simple because of sequential downloading
        // assumption: block_id + 1 has not been requested yet

        Some((piece_id, block_id))
    }
}
