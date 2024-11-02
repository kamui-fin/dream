use clap::Parser;
use local_ip_address::local_ip;
use rand::rngs::OsRng;
use rand::Rng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::LinkedList;
use std::net::Ipv4Addr;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::net::UdpSocket;
use tokio::task::JoinSet;
use tokio::time::sleep;

// Number of bits for our IDs
const NUM_BITS: usize = 6;
// Max number of entries in K-bucket
const K: usize = 4;
// Max concurrent requests
const ALPHA: usize = 3;

async fn handle_krpc_call(
    routing_table: &Arc<Mutex<RoutingTable>>,
    peer_store: &Arc<Mutex<HashMap<String, Vec<Node>>>>,
    socket: &UdpSocket,
    buf: &[u8; 2048],
    len: usize,
    addr: SocketAddr,
) {
    let query: KrpcRequest = serde_bencode::from_bytes(&buf[..len]).unwrap();
    println!("Received {:#?} from {}", query, addr);

    match query.q.as_str() {
        "ping" => {
            let routing_table = routing_table.lock().unwrap();
            let mut return_values = HashMap::new();
            return_values.insert("id".into(), routing_table.my_node.id.to_string());

            let response = KrpcSuccessResponse {
                y: "r".into(),
                t: query.t,
                r: return_values,
            };
            let response = serde_bencode::to_bytes(&response).unwrap();
            socket.send_to(&response, addr).await.unwrap();
        }
        "find_node" => {
            let source_id = query.a.get("id").unwrap().parse().unwrap();
            let source_node = Node {
                id: source_id,
                ip: addr.ip(),
                port: addr.port(),
            };
            routing_table.lock().unwrap().upsert_node(source_node);
            let target_node_id = query.a.get("target").unwrap().parse().unwrap();
            let k_closest_nodes = get_nodes(routing_table, target_node_id);
            let compact_node_info = k_closest_nodes
                .iter()
                .map(|node| node.get_node_compact_format())
                .collect::<Vec<String>>()
                .concat();

            let mut return_values = HashMap::new();
            return_values.insert(
                "id".into(),
                routing_table.lock().unwrap().my_node.id.to_string(),
            );
            return_values.insert("nodes".into(), compact_node_info);

            let response = KrpcSuccessResponse {
                y: "r".into(),
                t: query.t,
                r: return_values,
            };
            let response = serde_bencode::to_bytes(&response).unwrap();
            socket.send_to(&response, addr).await.unwrap();
        }
        "get_peers" => {
            let source_id = query.a.get("id").unwrap().parse().unwrap();
            let source_node = Node {
                id: source_id,
                ip: addr.ip(),
                port: addr.port(),
            };
            routing_table.lock().unwrap().upsert_node(source_node);

            let info_hash = query.a.get("info_hash").unwrap();

            let mut return_values = HashMap::new();
            return_values.insert(
                "id".into(),
                routing_table.lock().unwrap().my_node.id.to_string(),
            );

            let peer_store_guard = peer_store.lock().unwrap();
            let peers = peer_store_guard.get(info_hash);
            if let Some(peers) = peers {
                // values
                let values = peers
                    .iter()
                    .map(|peer| peer.get_peer_compact_format())
                    .collect::<Vec<String>>()
                    .concat();
                return_values.insert("values".into(), values); // this won't work rn
            } else {
                // nodes
                let k_closest_nodes = get_nodes(routing_table, info_hash.parse().unwrap());
                let compact_node_info = k_closest_nodes
                    .iter()
                    .map(|node| node.get_node_compact_format())
                    .collect::<Vec<String>>()
                    .concat();
                return_values.insert("nodes".into(), compact_node_info);
            }

            // generate token
            // hash ip + secret
            let mut hasher = Sha1::new();
            if let IpAddr::V4(v4addr) = addr.ip() {
                hasher.update(v4addr.octets());
            }
            hasher.update(*routing_table.lock().unwrap().secret.lock().unwrap());
            let token = format!("{:x}", hasher.finalize());

            return_values.insert("token".into(), token);

            let response = KrpcSuccessResponse {
                y: "r".into(),
                t: query.t,
                r: return_values,
            };
            let response = serde_bencode::to_bytes(&response).unwrap();
            socket.send_to(&response, addr).await.unwrap();
        }
        "announce_peer" => {
            let info_hash = query.a.get("info_hash").unwrap();
            let port = query.a.get("port").unwrap();
            let token = query.a.get("token").unwrap();

            let source_id = query.a.get("id").unwrap().parse().unwrap();
            let source_node = Node {
                id: source_id,
                ip: addr.ip(),
                port: addr.port(),
            };
            routing_table
                .lock()
                .unwrap()
                .upsert_node(source_node.clone());

            let mut hasher = Sha1::new();
            if let IpAddr::V4(querying_ip) = addr.ip() {
                hasher.update(querying_ip.octets());
            }
            hasher.update(*routing_table.lock().unwrap().secret.lock().unwrap());

            let target_token = format!("{:x}", hasher.finalize());

            if *token != target_token {
                // send failure message
            }

            peer_store
                .lock()
                .unwrap()
                .entry(info_hash.clone())
                .or_default()
                .push(source_node);

            let mut return_values: HashMap<String, String> = HashMap::new();
            return_values.insert(
                "id".to_string(),
                routing_table.lock().unwrap().my_node.ip.to_string(),
            );

            let response = KrpcSuccessResponse {
                y: "r".into(),
                t: query.t,
                r: return_values,
            };

            let response = serde_bencode::to_bytes(&response).unwrap();
            socket.send_to(&response, addr).await.unwrap();
        }
        _ => {}
    }
}

fn get_nodes(routing_table: &Arc<Mutex<RoutingTable>>, target_node_id: u32) -> Vec<Node> {
    let mut nodes = routing_table.lock().unwrap().get_all_nodes();
    nodes.sort_by_key(|node| node.id ^ target_node_id);
    nodes.truncate(K);

    if let Some(first_match) = nodes.first() {
        if first_match.id == target_node_id {
            return vec![first_match.clone()];
        }
    }

    nodes
}

// fyi: refresh periodically too besides only when joining
// if no node lookup for bucket range has been done within 1hr
async fn refresh_bucket(
    routing_table: &Arc<Mutex<RoutingTable>>,
    bucket_idx: usize,
    socket: &Arc<UdpSocket>,
) {
    let node_id = routing_table.lock().unwrap().get_refresh_target(bucket_idx);
    recursive_find_nodes(node_id, routing_table, socket).await;
}

async fn send_ping(socket: &UdpSocket, addr: &str) {
    let mut arguments = HashMap::new();
    arguments.insert("id".into(), "client".into());

    let ping_query = KrpcRequest {
        t: gen_trans_id(),
        y: "q".into(),
        q: "ping".into(),
        a: arguments,
    };
    let ping_query = serde_bencode::to_bytes(&ping_query).unwrap();
    socket.send_to(&ping_query, addr).await.unwrap();

    let mut buf = [0; 2048];
    let (amt, src) = socket.recv_from(&mut buf).await.unwrap();

    let response: KrpcSuccessResponse = serde_bencode::from_bytes(&buf[..amt]).unwrap();
    println!("Received {:#?} from {}", response, src);
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
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

    #[arg(long)]
    id: Option<u32>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    // assign random id for our node if not passed in
    let id = args.id.unwrap_or_else(|| {
        // wait could we just hash our IP???
        let mut rng = rand::thread_rng();
        rng.gen_range(0..64)
    });
    let ip = local_ip().unwrap();

    let secret = Arc::new(Mutex::new(gen_secret()));
    let secret_clone = Arc::clone(&secret);

    let socket = UdpSocket::bind(format!("127.0.0.1:{}", args.udp_port))
        .await
        .unwrap();
    let socket = Arc::new(socket);

    let our_node = Node {
        id,
        ip,
        port: args.udp_port,
    };
    let routing_table = Arc::new(Mutex::new(RoutingTable::new(
        our_node.clone(),
        Arc::clone(&secret),
    )));

    println!("Started DHT node on {:#?}", our_node);

    // change secret every 10 min
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(600)).await;
            let mut sec = secret_clone.lock().unwrap();
            *sec = gen_secret();
        }
    });

    // underlying data store for peers
    let peer_store: Arc<Mutex<HashMap<String, Vec<Node>>>> = Arc::new(Mutex::new(HashMap::new()));

    // setup tcp server for external DHT interface (testing)
    if let Some(tcp_port) = args.tcp_port {
        tokio::spawn(async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{}", tcp_port))
                .await
                .unwrap();

            loop {
                let (mut tcp_socket, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = tcp_socket.split();
                let mut reader = BufReader::new(reader);
                let mut buffer = String::new();

                loop {
                    match reader.read_line(&mut buffer).await {
                        Ok(0) => {
                            println!("Connection closed");
                            break;
                        }
                        Ok(_) => {
                            match buffer.trim_end().split_once(" ") {
                                Some(("GET", info_hash)) => {}
                                Some(("PUT", key_value)) => {}
                                Some(("PING", address)) => {
                                    // send_ping(&socket, address);
                                }
                                _ => {}
                            }
                            println!("Received: {}", buffer.trim_end());
                            if let Err(e) = writer.write_all(buffer.as_bytes()).await {
                                eprintln!("Failed to write to socket; err = {:?}", e);
                                break;
                            }
                            buffer.clear();
                        }
                        Err(e) => {
                            eprintln!("Failed to read from socket; err = {:?}", e);
                            break;
                        }
                    }
                }
            }
        });
    }

    if let Some(bootstrap_ip) = args.bootstrap_ip {
        if let Some(bootstrap_port) = args.bootstrap_port {
            if let Some(bootstrap_id) = args.bootstrap_id {
                // 1. initialize k-bucket with another known node
                let bootstrap_node = Node {
                    id: bootstrap_id,
                    ip: bootstrap_ip.parse().unwrap(),
                    port: bootstrap_port,
                };
                routing_table.lock().unwrap().upsert_node(bootstrap_node);
                // 2. run find_nodes on itself to fill k-bucket table
                let k_closest_nodes =
                    recursive_find_nodes(our_node.id, &routing_table, &socket).await; // assuming sorted by distance

                // 3. refresh buckets past closest node bucket
                let closest_idx = routing_table
                    .lock()
                    .unwrap()
                    .find_bucket_idx(k_closest_nodes[0].node.id);
                for idx in (closest_idx + 1)..(NUM_BITS as u32) {
                    refresh_bucket(&routing_table, idx as usize, &socket).await;
                }
            }
        }
    }

    loop {
        let mut buf = [0; 2048];
        let (len, addr) = socket.recv_from(&mut buf).await.unwrap();

        handle_krpc_call(&routing_table, &peer_store, &socket, &buf, len, addr).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_bucket_finder() {
        let secret = Arc::new(Mutex::new([0; 16]));
        let rt = RoutingTable::new(
            Node {
                id: 0,
                ip: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                port: 0,
            },
            secret,
        );
        assert_eq!(rt.find_bucket_idx(0b001101), 2);
        assert_eq!(rt.find_bucket_idx(0b000001), 5);
    }
}
