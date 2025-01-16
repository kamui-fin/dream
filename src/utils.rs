use std::sync::atomic::{AtomicBool, Ordering};

use sha1::{Digest, Sha1};
use tokio::sync::Notify;

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
