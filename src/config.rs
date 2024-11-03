// Number of bits for our IDs
pub const NUM_BITS: usize = 6;
// Max number of entries in K-bucket
pub const K: usize = 4;
// Max concurrent requests
pub const ALPHA: usize = 3;

// TOML config loading and clap CLI arg parse

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long)]
    id: Option<u32>,

    #[arg(short, long)]
    udp_port: u16,

    #[arg(short, long)]
    tcp_port: Option<u16>,

    #[arg(long)]
    bootstrap_id: Option<u32>,

    #[arg(long)]
    bootstrap_ip: Option<String>,

    #[arg(long)]
    bootstrap_port: Option<u16>,
}

impl Args {
    fn get_bootstrap(&self) -> Option<Node> {
        let (id, ip, port) = (self.bootstrap_id?, self.bootstrap_ip?, self.bootstrap_port?);
        Node::new(id, ip, port)
    }
}
