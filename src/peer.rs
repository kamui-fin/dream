use anyhow::{anyhow, Result};
use serde::{Deserialize, Deserializer};
use serde_bytes::ByteBuf;
use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use log::{error, info};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

use crate::{
    msg::{Message, MessageType},
    piece::BitField,
    tracker::Metafile,
    utils::slice_to_u32_msb,
};

#[derive(Clone, Debug)]
pub struct Peer {
    ip: Ipv4Addr,
    port: u16,
}

impl Peer {
    pub fn new(ip: Ipv4Addr, port: u16) -> Self {
        Self { ip, port }
    }

    pub fn addr(&self) -> SocketAddr {
        SocketAddr::new(std::net::IpAddr::V4(self.ip), self.port)
    }
}

pub fn deserialize_peers<'de, D>(deserializer: D) -> Result<Vec<Peer>, D::Error>
where
    D: Deserializer<'de>,
{
    let bytes: ByteBuf = Deserialize::deserialize(deserializer)?;
    let mut peers = vec![];
    for curr_chunk in bytes.chunks(6) {
        if curr_chunk.len() == 6 {
            let ip = Ipv4Addr::new(curr_chunk[0], curr_chunk[1], curr_chunk[2], curr_chunk[3]);
            let port = u16::from_be_bytes([curr_chunk[4], curr_chunk[5]]);
            peers.push(Peer::new(ip, port))
        }
    }
    Ok(peers)
}

pub struct RemotePeer {
    pub peer: Peer,
    pub connection: PeerConnection,
    pub piece_lookup: BitField,
    pub am_choking: bool,      // = 1
    pub am_interested: bool,   // = 0
    pub peer_choking: bool,    // has this peer choked us? = 1
    pub peer_interested: bool, // is this peer interested in us? = 0
}

pub struct PeerManager {
    pub swarm: Vec<RemotePeer>,
    pub choke_list: Vec<RemotePeer>,
}

pub async fn peer_handshake(peer: Peer, meta_file: Metafile, my_id: String) -> Result<TcpStream> {
    info!("Initiating peer handshake with {:#?}", peer);

    let mut handshake = [0u8; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    handshake[28..48].copy_from_slice(&meta_file.get_info_hash());
    handshake[48..68].copy_from_slice(my_id.as_bytes());

    let connect_timeout = Duration::from_secs(5);
    let mut stream = match timeout(connect_timeout, TcpStream::connect(peer.addr())).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            error!("Failed to connect: {}", e);
            return Err(anyhow!(e));
        }
        Err(_) => {
            error!("Connection attempt timed out");
            return Err(anyhow!("Connection timed out"));
        }
    };
    stream.write_all(&handshake).await?;

    let mut res = [0u8; 68];
    stream.read_exact(&mut res).await?;

    if res[0..20] == handshake[0..20] && res[28..48] == handshake[28..48] {
        Ok(stream)
    } else {
        stream.shutdown().await?;
        error!("Handshake mismatch");
        Err(anyhow!("Handshake failed"))
    }
}

pub struct PeerConnection {
    conn: Option<TcpStream>,
}

impl PeerConnection {
    pub fn send_message(&mut self, msg: Message) {
        if let Some(conn) = &mut self.conn {
            conn.write_all(&msg.serialize());
        }
    }

    pub fn receive_message(&mut self) -> Option<Message> {
        if let Some(conn) = &mut self.conn {
            let mut len_buf = [0u8; 4];
            conn.read_exact(&mut len_buf);

            let msg_length = slice_to_u32_msb(&len_buf);
            if msg_length > 0 {
                let mut id_buf = [0u8; 1];
                conn.read_exact(&mut id_buf);

                let mut payload_buf = vec![0u8; msg_length as usize];
                conn.read_exact(&mut payload_buf);

                Some(MessageType::from_id(Some(id_buf[0])).build_msg(payload_buf))
            } else {
                Some(MessageType::KeepAlive.build_msg(vec![]))
            }
        } else {
            None
        }
    }
}