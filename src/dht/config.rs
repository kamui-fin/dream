use std::{net::Ipv4Addr, str::FromStr};

use clap::{command, Parser};

use crate::dht::node::Node;

// Number of bits for our IDs
pub const NUM_BITS: usize = 6;
// Max number of entries in K-bucket
pub const K: usize = 2;
// Max concurrent requests
pub const ALPHA: usize = 2;
// Amount of time to wait before refreshing
pub const REFRESH_TIME: u64 = 15 * 60;

// TOML config loading and clap CLI arg parse

#[derive(Parser, Debug, Default)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(long)]
    pub id: Option<u32>,

    #[arg(short, long)]
    pub udp_port: u16,

    #[arg(long)]
    pub bootstrap_id: Option<u32>,

    #[arg(long)]
    pub bootstrap_ip: Option<String>,

    #[arg(long)]
    pub bootstrap_port: Option<u16>,
}

impl Args {
    pub fn get_bootstrap(&self) -> Option<Node> {
        let (id, ip, port) = (
            self.bootstrap_id?,
            self.bootstrap_ip.clone()?,
            self.bootstrap_port?,
        );
        Some(Node::new(
            id,
            std::net::IpAddr::V4(Ipv4Addr::from_str(&ip).unwrap()),
            port,
        ))
    }
}
