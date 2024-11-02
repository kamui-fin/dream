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

// utility functions
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

// node participating in DHT
// in our bittorrent implementations, peers are also nodes
#[derive(Eq, PartialEq, Clone, Debug)]
struct Node {
    id: u32,
    ip: IpAddr,
    port: u16,
    // is_good: bool, // responded to our query or requested a query within past 15 min,
}

impl Node {
    fn get_peer_compact_format(&self) -> String {
        let mut compact_info = [0u8; 6];

        if let IpAddr::V4(v4_addr) = self.ip {
            let ip = v4_addr.to_bits().to_le_bytes();
            compact_info[0..4].copy_from_slice(&ip);
        }

        let port = self.port.to_le_bytes();
        compact_info[4..6].copy_from_slice(&port);

        format!("{:?}", compact_info)
    }

    fn get_node_compact_format(&self) -> String {
        let mut compact_info = [0u8; 7];
        compact_info[0] = self.id as u8;

        if let IpAddr::V4(v4_addr) = self.ip {
            let ip = v4_addr.to_bits().to_le_bytes();
            compact_info[1..5].copy_from_slice(&ip);
        }

        let port = self.port.to_le_bytes();
        compact_info[5..7].copy_from_slice(&port);

        format!("{:?}", compact_info)
    }
}

// infohash of torrent = key id
// starts off with one bucket with ID space range 0 - 2^160
// when bucket full of known good nodes, no more nodes may be added unless our own ID falls within the range of the bucket
// -> in that case, bucket is replaced by 2 new buckets each with half the range of the old bucket
// -> and nodes from old bucket and distributed among two new ones
// for new table with 1 bucket, the full bucket is always split into two new buckets covering ranges 0..2^159 and 2^159..2^160
// if any nodes are **known** to be bad, it gets replaced by new node
//
// if questionable nodes not seen in the last 15 min, least recently seen is pinged
// -> until one fails to respond or all nodes are good
// -> but if fails to respond, try once more before discarding node and replacing with new good node
//
// need a "last changed" property for each bucket to indicate freshness
// -> when node is pinged and responds, when node is added to bucket, when node in a bucket is replaced with another node
//    -> the bucket last changed property should b e refreshed
//    -> by picking random id in the range of the bucket and run find_nodes
//    -> nodes that are able to receive queries from other nodes dno't need to refresh buckets often
//    -> but nodes that can't need to refresh periodically  so good nodes are available when DHT is needed
struct RoutingTable {
    my_node: Node,
    secret: Arc<Mutex<[u8; 16]>>,
    // array of linked lists with NUM_BITS elements
    buckets: Vec<LinkedList<Node>>,
}

impl RoutingTable {
    fn new(my_node: Node, secret: Arc<Mutex<[u8; 16]>>) -> Self {
        Self {
            my_node,
            secret,
            buckets: Vec::with_capacity(NUM_BITS),
        }
    }

    fn get_all_nodes(&self) -> Vec<Node> {
        let mut all_nodes = vec![];
        for bucket in self.buckets.iter() {
            for node in bucket.iter() {
                all_nodes.push(node.clone());
            }
        }
        all_nodes
    }

    fn find_bucket_idx(&self, node_id: u32) -> u32 {
        let xor_result = node_id ^ self.my_node.id;
        xor_result.leading_zeros() - ((32 - NUM_BITS) as u32)
    }

    fn node_in_bucket(&self, bucket_idx: usize, node_id: u32) -> Option<&Node> {
        self.buckets[bucket_idx]
            .iter()
            .find(|&node| (node.id == node_id))
    }

    fn remove_node(&mut self, node_id: u32, bucket_idx: usize) {
        let mut new_list: LinkedList<Node> = LinkedList::new();

        while let Some(curr_front) = self.buckets[bucket_idx].pop_front() {
            if (curr_front).id != node_id {
                new_list.push_back(curr_front);
                self.buckets[bucket_idx].pop_front();
            }
        }

        self.buckets[bucket_idx] = new_list;
    }

    fn upsert_node(&mut self, node: Node) {
        let bucket_idx = self.find_bucket_idx(node.id) as usize;
        let already_exists = self.node_in_bucket(bucket_idx, node.id).is_none();
        let is_full = self.buckets[bucket_idx].len() >= K;

        if already_exists && !is_full {
            self.remove_node(node.id, bucket_idx);
            self.buckets[bucket_idx].push_back(node);
        } else if already_exists {
            // ping front of list and go from there
            // if ping
        } else {
            self.buckets[bucket_idx].push_back(node);
        }
    }

    fn get_refresh_target(&self, bucket_idx: usize) -> u32 {
        let start = 2u32.pow((NUM_BITS - bucket_idx - 1) as u32);
        let end = 2u32.pow((NUM_BITS - bucket_idx) as u32);

        let mut rng = rand::thread_rng();
        let node_id = rng.gen_range(start..end);

        node_id
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
struct KrpcRequest {
    t: String,
    y: String,

    q: String,
    a: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
struct KrpcSuccessResponse {
    t: String,
    y: String,

    r: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
struct KrpcErrorResponse {
    t: String,
    y: String,

    e: (u8, String),
}

// KRPC - Bencoded dictionaries sent over UDP without retries
// dictionary with 3 keys common in all msgs and additional keys if needed
// t - transaction id
//      -> generated by the querying node and is echoed in the response
//      -> useful for correlation multiple queries to same node
//      -> short string of binary numbers, 2 characters are enough
// y - single char describing msg type (q for query, r for response, e for error)
// v - versioning (not needed rn)
//
// query:
//      key q - string value containing method name of query
//      key a - named arguments dict
// responses - key r, value is dictionary containing named return values
// errors - key e is a list, first element error code, second element string containing the error message

enum KrpcError {
    // 201
    GenericError,
    // 202
    ServerError,
    // 203
    ProtocolError,
    // 204
    MethodUnknown,
}

// All queries have id key and value containing node id of querying node
// Responses have same for responding node
enum DhtMessageType {
    // q = "ping", id = 20 byte string source id
    // Query = {"t":"aa", "y":"q", "q":"ping", "a":{"id":"abcdefghij0123456789"}}
    // Response = {"t":"aa", "y":"r", "r": {"id":"mnopqrstuvwxyz123456"}}
    Ping,
    // q = "find_node", id = source node id, target = target node id
    // return compact node info OR k closest good nodes in its own routing table
    FindNode,
    // q = "get_peers", id = source, info_hash = basically key id
    // If queried node has the val, return in "values" list
    // Else return "nodes" list with K nodes closest to infohash.
    // "token" short binary string included in return value TODO: what is this for?
    // format:
    // arguments:  {"id" : "<querying nodes id>", "info_hash" : "<20-byte infohash of target torrent>"}
    // response: {"id" : "<queried nodes id>", "token" :"<opaque write token>", "values" : ["<peer 1 info string>", "<peer 2 info string>"]}
    // or: {"id" : "<queried nodes id>", "token" :"<opaque write token>", "nodes" : "<compact node info>"}
    GetPeers,
    // q = "announce_peer", id = source, info_hash = key, port = udp port, token = received in response to previous get_peers query
    // response: {"id" : "<queried nodes id>"}
    AnnouncePeer,
}

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
            return_values.insert("id".into(), routing_table.lock().unwrap().my_node.id.to_string());
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
            return_values.insert("id".into(), routing_table.lock().unwrap().my_node.id.to_string());

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
                let k_closest_nodes = get_nodes(&routing_table, info_hash.parse().unwrap());
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
            routing_table.lock().unwrap().upsert_node(source_node.clone());

            let mut hasher = Sha1::new();
            if let IpAddr::V4(querying_ip) = addr.ip() {
                hasher.update(querying_ip.octets());
            }
            hasher.update(*routing_table.lock().unwrap().secret.lock().unwrap());

            let target_token = format!("{:x}", hasher.finalize());

            if *token != target_token {
                // send failure message
            }

            peer_store.lock().unwrap().entry(info_hash.clone()).or_insert_with(Vec::<Node>::new).push(source_node);
            
            let mut return_values: HashMap<String, String> = HashMap::new();
            return_values.insert("id".to_string(), routing_table.lock().unwrap().my_node.ip.to_string());

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

    if let Some(first_match) = nodes.get(0) {
        if first_match.id == target_node_id {
            return vec![first_match.clone()];
        }
    }

    nodes
}

fn deserialize_compact_node(serialized_nodes: Option<&String>) -> Vec<Node>{
    let mut nodes = Vec::new();

    let bytes = serialized_nodes.unwrap().as_bytes();

    for curr_chunk in bytes.chunks(7){
        if curr_chunk.len() == 7 {
            let id = curr_chunk[0];
            let ip = Ipv4Addr::new(curr_chunk[1], curr_chunk[2], curr_chunk[3], curr_chunk[4]);
            let port = u16::from_be_bytes([curr_chunk[5], curr_chunk[6]]);

            nodes.push(Node {id: id.into(), port, ip: std::net::IpAddr::V4(ip)})
        }
    }

    nodes    
}

async fn send_find_node(socket: &UdpSocket, target_node: &Node, my_id: u32) -> Vec<Node> {
    let mut arguments = HashMap::new();
    arguments.insert("id".into(), my_id.to_string());
    arguments.insert("target".into(), target_node.id.to_string());

    let find_node_query = KrpcRequest {
        t: gen_trans_id(),
        y: "q".into(),
        q: "find_node".into(),
        a: arguments,
    };
    let find_node_query = serde_bencode::to_bytes(&find_node_query).unwrap();
    socket
        .send_to(
            &find_node_query,
            format!("{}:{}", target_node.ip, target_node.port),
        )
        .await
        .unwrap();

    let mut buf = [0; 2048];
    let (amt, src) = socket.recv_from(&mut buf).await.unwrap();

    let response: KrpcSuccessResponse = serde_bencode::from_bytes(&buf[..amt]).unwrap();
    println!("Received {:#?} from {}", response, src);


    let serialized_nodes = response.r.get("nodes");
    let k_closest_nodes = deserialize_compact_node(serialized_nodes);
    k_closest_nodes
}

#[derive(Clone, Eq, PartialEq)]
struct NodeDistance {
    node: Node,
    dist: u32,
}

impl Ord for NodeDistance {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist.cmp(&other.dist)
    }
}

impl PartialOrd for NodeDistance {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

async fn recursive_find_nodes(
    target_node_id: u32,
    routing_table: &Arc<Mutex<RoutingTable>>,
    socket: &Arc<UdpSocket>,
) -> Vec<NodeDistance> {
    let mut set = JoinSet::new();

    let mut already_queried = HashSet::new();

    // pick α nodes from closest non-empty k-bucket, even if less than α entries
    let mut alpha_set = vec![];
    
    let routing_table_clone = routing_table.lock().unwrap();
    let mut bucket_idx = routing_table_clone.find_bucket_idx(target_node_id);
    while routing_table_clone.buckets[bucket_idx as usize].is_empty() {
        bucket_idx = (bucket_idx + 1) % routing_table_clone.buckets.len() as u32;
    }
    for node in routing_table_clone.buckets[bucket_idx as usize].iter() {
        alpha_set.push(NodeDistance {
            node: node.clone(),
            dist: node.id ^ target_node_id,
        });
    }

    let mut current_closest = alpha_set.clone();

    alpha_set.truncate(ALPHA);


    let max_heap: Arc<Mutex<BinaryHeap<NodeDistance>>> =
        Arc::new(Mutex::new(BinaryHeap::new()));
    let within_heap: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));

    for distance_node in alpha_set.iter().cloned() {
        within_heap.lock().unwrap().insert(distance_node.node.id);
        max_heap.lock().unwrap().push(distance_node);
    }

    loop {
        // send parallel find_node to all of em
        for distance_node in alpha_set.iter().cloned() {
            let within_heap = Arc::clone(&within_heap);
            let routing_table = Arc::clone(&routing_table);
            let max_heap = Arc::clone(&max_heap);
            let socket = Arc::clone(&socket);
            // updated k closest
            already_queried.insert(distance_node.node.id);
            set.spawn(async move {
                let my_id = routing_table.lock().unwrap().my_node.id;
                let k_closest_nodes = send_find_node(&socket, &distance_node.node, my_id).await;
                for node in k_closest_nodes {
                    if !within_heap.lock().unwrap().contains(&node.id) {
                        max_heap.lock().unwrap().push(NodeDistance {
                            node: node.clone(),
                            dist: node.id ^ target_node_id,
                        });
                        if max_heap.lock().unwrap().len() > K {
                            max_heap.lock().unwrap().pop();
                        }
                        within_heap.lock().unwrap().insert(node.id);
                    }
                }
            });
        }
        // JOIN all these tasks
        while let Some(_) = set.join_next().await {}

        // resend the find_node to nodes it has learned about from previous RPCs
        // of the k nodes you have heard of closest to the target, it picks α that it has not yet queried and resends the FIND NODE RPC to them.
        let new_closest = max_heap
            .lock()
            .unwrap()
            .clone()
            .into_sorted_vec()
            .iter()
            .filter(|n| !already_queried.contains(&n.node.id))
            .cloned()
            .collect::<Vec<NodeDistance>>();

        // the lookup terminates when the initiator has queried and gotten responses from the k closest nodes it has seen.
        if new_closest.is_empty() {
            break;
        }

        current_closest = new_closest;
        alpha_set = (&current_closest[0..ALPHA]).to_vec();

        // TODO: nodes that fail to respond quickly are removed from consideration until and unless they do respond.

    }

    current_closest
}

// fyi: refresh periodically too besides only when joining
// if no node lookup for bucket range has been done within 1hr
async fn refresh_bucket(routing_table: &Arc<Mutex<RoutingTable>>, bucket_idx: usize, socket: &Arc<UdpSocket>) {
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
    let routing_table= Arc::new(Mutex::new(RoutingTable::new(our_node.clone(), Arc::clone(&secret))));

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
                let k_closest_nodes = recursive_find_nodes(our_node.id, &routing_table, &socket).await; // assuming sorted by distance

                // 3. refresh buckets past closest node bucket
                let closest_idx = routing_table.lock().unwrap().find_bucket_idx(k_closest_nodes[0].node.id);
                for idx in (closest_idx + 1)..(NUM_BITS as u32) {
                    refresh_bucket(&routing_table, idx as usize, &socket).await;
                }
            }
        }
    }

    loop {
        let mut buf = [0; 2048];
        let (len, addr) = socket.recv_from(&mut buf).await.unwrap();

        handle_krpc_call(
            &routing_table,
            &peer_store,
            &socket,
            &buf,
            len,
            addr,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

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

    #[test]
    fn test_compact_addr() {
        // TODO:
    }
}
