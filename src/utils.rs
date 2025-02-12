
use std::{
    ops::Range,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use sha1::{Digest, Sha1};
use simplelog::*;
use std::fs::File;
use tokio::sync::Notify;

use crate::config::CONFIG;

fn get_log_level(level: &str) -> LevelFilter {
    match level {
        "trace" => LevelFilter::Trace,
        "debug" => LevelFilter::Debug,
        "info" => LevelFilter::Info,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        _ => LevelFilter::Info,
    }
}

pub fn init_logger(level: &str) {
    let log_path = PathBuf::from(&CONFIG.logging.log_file);
    if !log_path.parent().unwrap().exists() {
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    }

    let log_level = get_log_level(level);

    CombinedLogger::init(vec![
        TermLogger::new(
            log_level,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        WriteLogger::new(
            log_level,
            Config::default(),
            File::create(log_path).unwrap(),
        ),
    ])
    .unwrap();
}

pub fn init_logger_debug() {
    CombinedLogger::init(vec![TermLogger::new(
        LevelFilter::Debug,
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )])
    .unwrap();
}

pub fn slice_to_u32_msb(bytes: &[u8]) -> u32 {
    // Ensure the slice has exactly 4 bytes
    let array: [u8; 4] = bytes.try_into().unwrap();
    u32::from_be_bytes(array)
}

pub fn hash_obj<B: AsRef<[u8]>>(buf: B) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(&buf);

    let result = hasher.finalize();
    let mut computed_hash = [0u8; 20];
    computed_hash.copy_from_slice(&result[..]);

    computed_hash
}

pub struct Notifier {
    notify: Notify,
    notified: AtomicBool,
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifier {
    pub fn new() -> Self {
        Notifier {
            notify: Notify::new(),
            notified: AtomicBool::new(false),
        }
    }

    // send a notification only if it hasn't been sent already
    pub fn notify_one(&self) {
        let previous =
            self.notified
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst);
        match previous {
            Ok(_) => {
                self.notify.notify_one();
            }
            Err(_) => {
                // has already been sent
            }
        }
    }

    pub async fn wait_for_notification(&self) {
        self.notify.notified().await;
        self.notified.store(false, Ordering::SeqCst);
    }
}

// TODO: structured logger

// given a (start, end) inclusive byte range, what is the Range<u32> of pieces that it covers?
// TODO: unit test needed
pub fn byte_to_piece_range(start: u64, end: u64, piece_len: u64) -> (u64, u64) {
    let start_piece = start / piece_len;
    let end_piece = end / piece_len;

    (start_piece, end_piece)
}
