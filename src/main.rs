use clap::Parser;
use local_ip_address::local_ip;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::collections::LinkedList;
use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::net::UdpSocket;

// Number of bits for our IDs
const NUM_BITS: usize = 6;
// Max number of entries in K-bucket
const K: usize = 4;
// Max concurrent requests
const M: usize = 3;

// node participating in DHT
// in our bittorrent implementations, peers are also nodes
#[derive(Debug)]
struct Node {
    id: u32,
    ip: IpAddr,
    port: u16,
    // is_good: bool, // responded to our query or requested a query within past 15 min,
}

impl Node {
    fn get_compact_format(&self) -> String {
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
    my_id: u32,
    // array of linked lists with NUM_BITS elements
    buckets: Vec<LinkedList<Node>>,
}

struct PeerStore {}

impl PeerStore {
    fn new() -> Self {
        Self {
            store: HashMap::new(),
        }
    }
}

impl RoutingTable {
    fn new(my_id: u32) -> Self {
        Self {
            my_id,
            buckets: Vec::with_capacity(NUM_BITS),
        }
    }

    fn find_bucket_idx(&self, node_id: u32) -> u32 {
        let xor_result = node_id ^ self.my_id;
        return xor_result.leading_zeros() - ((32 - NUM_BITS) as u32);
    }

    fn node_in_bucket(&self, bucket_idx: usize, node_id: u32) -> Option<&Node> {
        for node in self.buckets[bucket_idx].iter() {
            if (node.id == node_id) {
                return Some(node);
            }
        }

        return None;
    }

    fn find_node(&self, node_id: u32, bucket_idx: usize) -> Option<usize> {
        let mut index = 0;

        for node in self.buckets[bucket_idx].iter() {
            if node.id == node_id {
                return Some(index + 1);
            } else {
                index = index + 1;
            }
        }

        return None;
    }

    fn remove_node(&mut self, node_id: u32, bucket_idx: usize) {
        let mut new_list: LinkedList<Node> = LinkedList::new();

        while let Some(curr_front) = self.buckets[bucket_idx].pop_front() {
            if ((curr_front).id != node_id) {
                new_list.push_back(curr_front);
                self.buckets[bucket_idx].pop_front();
            }
        }

        self.buckets[bucket_idx] = new_list;
    }

    fn upsert_node(&mut self, node: Node) {
        let bucket_idx = self.find_bucket_idx(node.id) as usize;
        let already_exists = self.node_in_bucket(bucket_idx, node.id).is_none();
        let is_full = self.buckets[bucket_idx].len() >= K as usize;

        if (already_exists && !is_full) {
            self.remove_node(node.id, bucket_idx);
            self.buckets[bucket_idx].push_back(node);
        } else if (already_exists) {
            // ping front of list and go from there
        } else {
            self.buckets[bucket_idx].push_back(node);
        }
    }
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    #[test]
    fn test_bucket_finder() {
        let rt = RoutingTable::new(0);
        assert_eq!(rt.find_bucket_idx(0b001101), 2);
        assert_eq!(rt.find_bucket_idx(0b000001), 5);
    }

    #[test]
    fn test_compact_addr() {
        // TODO:
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

fn gen_trans_id() -> String {
    let mut rng = rand::thread_rng();
    let trans_id: u16 = rng.gen();
    format!("{:02x}", trans_id)
}

async fn handle_krpc_call(
    routing_table: &mut RoutingTable,
    peer_store: &mut HashMap<String, Vec<Node>>,
    socket: &UdpSocket,
    buf: &[u8; 2048],
    len: usize,
    addr: SocketAddr,
) {
    let query: KrpcRequest = serde_bencode::from_bytes(&buf[..len]).unwrap();
    println!("Received {:#?} from {}", query, addr.to_string());

    match query.q.as_str() {
        "ping" => {
            let mut return_values = HashMap::new();
            return_values.insert("id".into(), routing_table.my_id.to_string());

            let response = KrpcSuccessResponse {
                y: "r".into(),
                t: query.t,
                r: return_values,
            };
            let response = serde_bencode::to_bytes(&response).unwrap();
            socket.send_to(&response, addr).await.unwrap();
        }
        "find_node" => {
            let mut k_closest_nodes = vec![];

            let source_id = query.a.get("id").unwrap().parse().unwrap();
            let source_node = Node {
                id: source_id,
                ip: addr.ip(),
                port: addr.port(),
            };
            let target_node_id = query.a.get("target").unwrap().parse().unwrap();

            // 1. update source_id into routing table
            routing_table.upsert_node(source_node);
            // 2. find the initial k-bucket
            let mut bucket_idx = routing_table.find_bucket_idx(target_node_id);
            // 2.5. Check if the exact match is already there
            if let Some(exact_node) =
                routing_table.node_in_bucket(bucket_idx as usize, target_node_id)
            {
                // {"t":"aa", "y":"r", "r": {"id":"0123456789abcdefghij", "nodes": "def456..."}}
                k_closest_nodes.push(exact_node.get_compact_format())
            } else {
                // 3. if not enough, move on to i + 1 bucket and wrap around if needed
                let original_bucket = bucket_idx;
                loop {
                    // append new list of bucket+i
                    for node in routing_table.buckets[bucket_idx as usize].iter() {
                        k_closest_nodes.push(node.get_compact_format());
                    }

                    bucket_idx += 1;
                    bucket_idx %= 160;

                    if bucket_idx == original_bucket {
                        break;
                    }
                }
                // 4. collect the k elements and return
                if k_closest_nodes.len() >= K {
                    k_closest_nodes.truncate(K);
                }
            }

            let compact_node_info = k_closest_nodes.concat();
            let mut return_values = HashMap::new();
            return_values.insert("id".into(), routing_table.my_id.to_string());
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

        }
        "announce_peer" => {}
        _ => {}
    };
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
    port: u16,

    #[arg(short = 't', long = "test")]
    is_testing: bool,

    #[arg(long)]
    bootstrap: Option<String>,

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

    let our_node = Node {
        id,
        ip,
        port: args.port,
    };

    let socket = UdpSocket::bind(format!("127.0.0.1:{}", args.port))
        .await
        .unwrap();
    println!("Started DHT node on {:#?}", our_node);

    let mut routing_table = RoutingTable::new(id);
    let mut peer_store: HashMap<String, Vec<Node>> = HashMap::new();

    if args.is_testing {
        send_ping(&socket, "127.0.0.1:8080").await;
    }

    if let Some(bootstrap) = args.bootstrap {
        // 1. initialize k-bucket with another known node
        // 2. run find_nodes on itself to fill k-bucket table
        // 3. refresh k-buckets farther than bootstrap node with find_node on random key within range
    } else {
    }

    loop {
        let mut buf = [0; 2048];
        let (len, addr) = socket.recv_from(&mut buf).await.unwrap();
        handle_krpc_call(
            &mut routing_table,
            &mut peer_store,
            &socket,
            &buf,
            len,
            addr,
        )
        .await;
    }
}
