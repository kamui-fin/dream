use rand::{rngs::OsRng, Rng, RngCore};

fn gen_secret() -> [u8; 16] {
    let mut secret = [0u8; 16];
    OsRng.fill_bytes(&mut secret);
    secret
}

fn gen_trans_id() -> String {
    let mut rng = rand::thread_rng();
    let trans_id: u16 = rng.gen();
    format!("{:02x}", trans_id)
}
