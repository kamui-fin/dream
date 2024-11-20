use rand::{distributions::Alphanumeric, thread_rng, Rng};
use sha1::{Sha1, Digest};

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

pub fn gen_peer_id() -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(20)
        .map(char::from)
        .collect()
}