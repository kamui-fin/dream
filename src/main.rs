mod msg;
mod peer;
mod piece;
mod server;
mod tracker;
mod utils;

use anyhow::Result;

const PORT: u16 = 6881;

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    // let peer_id = gen_peer_id();
    // info!("Initialized peer {peer_id}");

    // let meta_file = parse_torrent_file("debian.torrent")?;
    // info!("Parsed metafile: {:#?}", meta_file);

    // start_server(PORT).await?;

    Ok(())
}
