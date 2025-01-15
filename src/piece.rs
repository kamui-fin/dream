use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
    vec,
};

use log::info;

use crate::{tracker::Metafile, utils::hash_obj};

pub const BLOCK_SIZE: u32 = (2 as u32).pow(14);
pub const KB_PER_BLOCK: u32 = BLOCK_SIZE / 1000;

#[derive(Debug, Clone)]
pub struct BitField(pub Vec<u8>);

impl BitField {
    pub fn new(n: u32) -> Self {
        Self(vec![0; (n as f32 / 8f32).ceil() as usize])
    }

    pub fn piece_exists(&self, index: u32) -> bool {
        let byte_idx = index / 8;
        let offset = index % 8;

        (self.0[byte_idx as usize] & (1 << (8 - offset - 1))) > 0
    }

    pub fn mark_piece(&mut self, index: u32) {
        let byte_idx = index / 8;
        let offset = index % 8;

        self.0[byte_idx as usize] |= 1 << (8 - offset - 1);
    }

    pub fn has_all(&self) -> bool {
        self.0.iter().all(|byte| *byte == 0xFF)
    }

    pub fn return_piece_indexes(&self) -> String {
        (0..(self.0.len() * 8))
            .filter(|i| self.piece_exists(*i as u32))
            .count()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let bitfield = BitField::new(16);
        assert_eq!(bitfield.0.len(), 2);
        assert!(bitfield.0.iter().all(|&byte| byte == 0));
    }

    #[test]
    fn test_real_world() {
        let mut bitfield = BitField::new(316 * 8);
        bitfield.mark_piece(0x9c8);
        assert!(bitfield.piece_exists(0x9c8));
        bitfield.mark_piece(0x810);
        assert!(bitfield.piece_exists(0x810));
        assert!(!bitfield.piece_exists(4));
    }

    #[test]
    fn test_piece_exists() {
        let mut bitfield = BitField::new(16);

        for i in 0..16 {
            assert!(!bitfield.piece_exists(i));
        }

        bitfield.mark_piece(3);
        assert!(bitfield.piece_exists(3));
        assert!(!bitfield.piece_exists(4));
    }

    #[test]
    fn test_mark_piece() {
        let mut bitfield = BitField::new(16);

        bitfield.mark_piece(7);
        assert!(bitfield.piece_exists(7));

        bitfield.mark_piece(0);
        assert!(bitfield.piece_exists(0));
    }

    #[test]
    fn test_return_piece_indexes() {
        let mut bitfield = BitField::new(16);

        assert_eq!(bitfield.return_piece_indexes(), "0");

        bitfield.mark_piece(3);
        bitfield.mark_piece(7);
        bitfield.mark_piece(12);

        assert_eq!(bitfield.return_piece_indexes(), "3");

        bitfield.mark_piece(15);
        assert_eq!(bitfield.return_piece_indexes(), "4");
    }

    #[test]
    #[should_panic(expected = "index out of bounds")]
    fn test_out_of_bounds_piece_exists() {
        let bitfield = BitField::new(8);
        bitfield.piece_exists(10);
    }

    #[test]
    #[should_panic(expected = "index out of bounds")]
    fn test_out_of_bounds_mark_piece() {
        let mut bitfield = BitField::new(8);
        bitfield.mark_piece(10);
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
    pub output_path: PathBuf,
}

impl Piece {
    pub fn new(size: u64, hash: [u8; 20], output_path: PathBuf) -> Self {
        if !output_path.exists() {
            Self {
                buffer: vec![0; size as usize],
                status: RequestStatus::Pending,
                block_set: HashSet::new(),
                downloaded_bytes: 0,
                hash,
                output_path,
            }
        } else {
            let mut file = File::open(&output_path).expect("Failed to open the file");
            let mut buffer = vec![0; size as usize];
            let bytes_read = file.read(&mut buffer).expect("Failed to read the file");
            let status = if bytes_read as u64 == size && hash_obj(&buffer) == hash {
                RequestStatus::Received
            } else {
                RequestStatus::Pending
            };

            Self {
                buffer: vec![0; size as usize],
                block_set: HashSet::new(),
                hash,
                output_path,
                status,
                downloaded_bytes: bytes_read as u32,
            }
        }
    }

    pub fn reset_piece(&mut self) {
        // NOTE: we only clear this data to save memory
        // other metadata remains like status == Received
        self.buffer.clear();
        self.block_set.clear();
    }

    pub fn retrieve_block(&mut self, begin: usize, len: usize) -> Option<Vec<u8>> {
        if begin + len < self.buffer.len() {
            Some(self.buffer[begin..(begin + len)].to_vec())
        } else {
            None
        }
    }

    pub fn store_block(&mut self, begin: usize, len: usize, data: &[u8]) -> bool {
        if begin + len <= self.buffer.len() {
            let block_id = ((begin / len) as f32).floor() as u32;
            if self.block_set.contains(&block_id) {
                return false;
            }
            self.block_set.insert(block_id);

            self.buffer[begin..(begin + len)].copy_from_slice(data);
            self.downloaded_bytes += len as u32;

            info!(
                "Block {block_id} stored, currently recieved {} out of {} bytes",
                self.downloaded_bytes,
                self.buffer.len()
            );
            let mut missing_pieces = vec![];
            for i in 0..16 {
                if !self.block_set.contains(&i) {
                    missing_pieces.push(i);
                }
            }
            info!("Missing pieces: {:?}", missing_pieces);

            if self.downloaded_bytes as usize == self.buffer.len() {
                self.status = RequestStatus::Received;
                info!("The whole piece has been recieved");
                // return something to indicate we need to notify bittorrent.rs about finished piece
                return true;
            }
        }
        false
    }

    pub fn verify_hash(&self) -> bool {
        let piece_hash = hash_obj(&self.buffer);
        let orig_hash = self.hash;

        orig_hash == piece_hash
    }

    pub fn persist(&self) -> anyhow::Result<()> {
        if self.status == RequestStatus::Received {
            let mut file = File::create(&self.output_path)?;
            file.write_all(&self.buffer)?;
        }

        Ok(())
    }
}

// Note: Avg block size is 2 ^ 14
pub struct PieceStore {
    pub num_pieces: u32,
    pub meta_file: Metafile,
    // TODO: kinda doesn't make sense to have a whole vector when operating on piece one at a time, but mainly just to allow future optimizations for downloading multiple pieces concurrently
    // not sure if it will ever make sense though even from a protocol standpoint, TBD..
    pub pieces: Vec<Piece>,
    pub output_dir: PathBuf,
}

impl PieceStore {
    pub fn new(meta_file: Metafile, output_dir: PathBuf) -> Self {
        if !output_dir.exists() {
            std::fs::create_dir(&output_dir).unwrap();
        }

        let hashes = meta_file.get_piece_hashes();
        let num_pieces = meta_file.get_num_pieces();

        let mut pieces = vec![];
        for idx in 0..num_pieces {
            let output_path = output_dir.join(format!("{}-{idx}.part", meta_file.info.name));
            let piece = Piece::new(
                meta_file.get_piece_len(idx as usize),
                hashes.0[idx as usize],
                output_path,
            );

            pieces.push(piece);
        }

        Self {
            num_pieces,
            pieces,
            meta_file,
            output_dir,
        }
    }

    pub fn persist(&self, idx: usize) -> anyhow::Result<()> {
        let piece = &self.pieces[idx];
        piece.persist()
    }

    pub fn concat(&self) -> anyhow::Result<()> {
        let piece_files = (0..(self.num_pieces)).map(|idx| &self.pieces[idx as usize].output_path);

        let dest_file_path = self.output_dir.join(&self.meta_file.info.name);
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

    pub fn get_missing_pieces(&self) -> Vec<usize> {
        (0..self.num_pieces as usize)
            .filter(|i| self.pieces[*i].status == RequestStatus::Received)
            .collect::<Vec<usize>>()
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
        self.pieces[idx].reset_piece();
    }
}
