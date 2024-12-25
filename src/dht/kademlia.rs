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

use futures::future::join_all;
use futures::FutureExt;
use log::error;
use log::info;
use serde::{Deserialize, Serialize};
use sha1::Digest;
use sha1::Sha1;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use std::{
    collections::{BinaryHeap, HashSet},
    net::IpAddr,
    sync::Mutex,
    time::Duration,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::dht::utils::deserialize_compact_peers;
use crate::dht::{
    config::{Args, ALPHA, K, NUM_BITS, REFRESH_TIME},
    node::{Node, NodeDistance},
};
use crate::dht::{context::RuntimeContext, utils::deserialize_compact_node, utils::gen_trans_id};
use tokio::{net::UdpSocket, task::JoinSet, time::sleep};

type Peer = (IpAddr, u16);

struct TransactionManager {
    transactions: Arc<Mutex<HashMap<String, oneshot::Sender<KrpcSuccessResponse>>>>,
}

impl TransactionManager {
    fn new() -> Self {
        TransactionManager {
            transactions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn create_future(&self, tx_id: String) -> oneshot::Receiver<KrpcSuccessResponse> {
        let (tx, rx) = oneshot::channel();
        self.transactions.lock().unwrap().insert(tx_id, tx);
        rx
    }

    fn resolve_transaction(&self, tx_id: String, value: KrpcSuccessResponse) {
        println!("Resolving transaction {tx_id}");
        if let Some(sender) = self.transactions.lock().unwrap().remove(&tx_id) {
            let _ = sender.send(value); // Send the value to resolve the future
        }
    }
}

pub enum KrpcError {
    // 201
    GenericError,
    // 202
    ServerError,
    // 203
    ProtocolError,
    // 204
    MethodUnknown,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct KrpcMessage {
    t: String,
    y: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct KrpcRequest {
    t: String,
    y: String,

    q: String,
    a: HashMap<String, String>,
}

impl KrpcRequest {
    pub fn new(request_type: &str, arguments: HashMap<String, String>) -> Self {
        KrpcRequest {
            t: gen_trans_id(),
            y: "q".into(),
            q: request_type.into(),
            a: arguments,
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct KrpcSuccessResponse {
    t: String,
    y: String,

    r: HashMap<String, String>,
}

impl KrpcSuccessResponse {
    pub fn from_request(request: &KrpcRequest, arguments: HashMap<String, String>) -> Self {
        Self {
            t: request.t.clone(),
            y: "r".into(),
            r: arguments,
        }
    }
}

#[derive(Debug)]
enum NodeOrPeer {
    Peers(Vec<Peer>),
    Nodes(Vec<Node>),
}

#[derive(Debug)]
pub struct GetPeersResponse {
    pub token: String,
    pub value: NodeOrPeer,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct KrpcErrorResponse {
    t: String,
    y: String,

    e: (u8, String),
}

pub struct Kademlia {
    pub socket: Arc<UdpSocket>,
    manager: TransactionManager,
    pub context: Arc<RuntimeContext>,
    node_id: u32,
    pub timers: Arc<Mutex<HashMap<usize, JoinHandle<()>>>>,
}

impl Kademlia {
    pub async fn init(args: &Args) -> Self {
        let context = Arc::new(RuntimeContext::init(args));
        let socket = Arc::new(
            UdpSocket::bind(format!("0.0.0.0:{}", context.node.port))
                .await
                .unwrap(),
        );
        let node_id = context.node.id;

        let timers: Arc<Mutex<HashMap<usize, JoinHandle<()>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        Self {
            socket,
            manager: TransactionManager::new(),
            node_id,
            context,
            timers,
        }
    }

    pub fn reset_timer(self: Arc<Self>, bucket_idx: usize) {
        let self_clone = self.clone();

        let mut binding = self_clone.timers.lock().unwrap();

        // only abort if it already exists
        if binding.contains_key(&bucket_idx) {
            let current_timer = &binding.get(&bucket_idx).unwrap();

            current_timer.abort();
        }

        binding.insert(
            bucket_idx,
            tokio::spawn(async move {
                sleep(Duration::from_secs(REFRESH_TIME)).await;
                println!("TIMER RAN OUT!");
                self.clone().refresh_bucket(bucket_idx).await;
            }),
        );
    }

    pub async fn start_server(self: Arc<Self>, bootstrap_node: Option<Node>) {
        // 1. start dht server
        let self_clone = self.clone();
        let handle = tokio::spawn(async move { self_clone.listen().await });

        // 2. enter with a bootstrap contact or init new network
        self.clone().join_dht_network(bootstrap_node).await;

        // 3. start maintenance tasks
        self.clone().republish_peer_task();
        self.context.clone().regen_token_task();

        handle.await;
    }

    pub async fn join_dht_network(self: Arc<Self>, bootstrap_node: Option<Node>) {
        if bootstrap_node.is_none() {
            return;
        }
        let bootstrap_node = bootstrap_node.unwrap();
        info!(
            "Joining network through bootstrap id = {}",
            bootstrap_node.id
        );
        // 1. initialize k-bucket with another known node
        self.context
            .routing_table
            .lock()
            .await
            .upsert_node(bootstrap_node);

        let self_clone = self.clone();

        // 2. run find_nodes on itself to fill k-bucket table
        let my_id = self_clone.context.node.id;
        let k_closest_nodes = self_clone.recursive_find_nodes(my_id).await; // assuming sorted by distance
        info!(
            "Populating routing table with k closest nodes = {:#?}",
            k_closest_nodes
        );

        // 3. refresh buckets past closest node bucket
        let closest_idx = self
            .clone()
            .context
            .routing_table
            .lock()
            .await
            .find_bucket_idx(k_closest_nodes[0].node.id);

        for idx in (closest_idx + 1)..NUM_BITS {
            // self.clone().reset_timer(idx);
            let self_clone = self.clone();
            self_clone.refresh_bucket(idx).await;
        }
    }

    pub async fn send_request(
        &self,
        query: KrpcRequest,
        addr: &str,
    ) -> Option<KrpcSuccessResponse> {
        info!("[CLIENT] Sending query to {addr}: {:#?}", query);
        let transaction_id = query.t.clone();
        let query = serde_bencoded::to_vec(&query).unwrap();
        self.socket.send_to(&query, addr).await.unwrap();

        let future = self.manager.create_future(transaction_id);
        match timeout(Duration::from_secs(3), future).await {
            Ok(result) => result.ok(),
            Err(_elapsed) => {
                println!("{:#?}", _elapsed);
                None
            }
        }
    }

    pub async fn send_response(&self, response: KrpcSuccessResponse, source_node: Node) {
        info!(
            "[SERVER] Sending response {:#?} to {:#?}",
            response, source_node
        );
        let addr = SocketAddr::new(source_node.ip, source_node.port);
        let response = serde_bencoded::to_vec(&response).unwrap();
        self.socket.send_to(&response, addr).await.unwrap();
    }

    pub async fn send_ping(&self, addr: &str) -> Option<KrpcSuccessResponse> {
        let arguments = HashMap::from([("id".to_string(), self.node_id.to_string())]);
        let request = KrpcRequest::new("ping", arguments);

        self.send_request(request, addr).await
    }

    pub async fn send_find_node(&self, dest: Node, target_node_id: u32) -> Vec<Node> {
        let arguments = HashMap::from([
            ("id".to_string(), self.node_id.to_string()),
            ("target".into(), target_node_id.to_string()),
        ]);
        let request = KrpcRequest::new("find_node", arguments);
        let addr = format!("{}:{}", dest.ip, dest.port);

        let response: KrpcSuccessResponse = self.send_request(request, &addr).await.unwrap();
        let serialized_nodes = response.r.get("nodes");
        deserialize_compact_node(serialized_nodes)
    }

    pub async fn send_get_peers(&self, dest: Node, info_hash: String) -> GetPeersResponse {
        let arguments = HashMap::from([
            ("id".to_string(), self.node_id.to_string()),
            ("info_hash".into(), info_hash),
        ]);
        let request = KrpcRequest::new("get_peers", arguments);
        let addr = format!("{}:{}", dest.ip, dest.port);
        let response = self.send_request(request, &addr).await.unwrap();

        let token = response.r.get("token").unwrap().clone();
        if response.r.contains_key("nodes") {
            let nodes = response.r.get("nodes"); // not deserialized
            let nodes = deserialize_compact_node(nodes);
            GetPeersResponse {
                token,
                value: NodeOrPeer::Nodes(nodes),
            }
        } else {
            let peers = response.r.get("values"); // not deserialized
            let peers = deserialize_compact_peers(peers);
            GetPeersResponse {
                token,
                value: NodeOrPeer::Peers(peers),
            }
        }
    }

    pub async fn send_announce_peer(&self, dest: Node, info_hash: String) -> KrpcSuccessResponse {
        info!(
            "Sending announce peer for hash {info_hash} to dest {:#?}",
            dest
        );
        let get_peers = self.send_get_peers(dest.clone(), info_hash.clone()).await;

        info!("First, get_peers response = {:#?}", get_peers);

        let arguments = HashMap::from([
            ("id".to_string(), self.node_id.to_string()),
            ("info_hash".into(), info_hash),
            ("token".into(), get_peers.token),
        ]);
        let request = KrpcRequest::new("announce_peer", arguments);
        let addr = format!("{}:{}", dest.ip, dest.port);
        self.send_request(request, &addr).await.unwrap()
    }

    async fn select_initial_nodes(&self, target_node_id: u32) -> Vec<NodeDistance> {
        let mut alpha_set = vec![];
        let routing_table_clone = self.context.routing_table.lock().await;
        let mut bucket_idx = std::cmp::min(routing_table_clone.find_bucket_idx(target_node_id), 5);

        let mut traversed_buckets = 0;
        while routing_table_clone.buckets[bucket_idx as usize].is_empty()
            && traversed_buckets != routing_table_clone.buckets.len()
        {
            bucket_idx = (bucket_idx + 1) % routing_table_clone.buckets.len();
            traversed_buckets += 1;
        }

        for node in routing_table_clone.buckets[bucket_idx as usize].iter() {
            alpha_set.push(NodeDistance {
                node: node.clone(),
                dist: node.id ^ target_node_id,
            });
        }

        alpha_set
    }

    fn update_closest_nodes(
        &self,
        max_heap: &Arc<Mutex<BinaryHeap<NodeDistance>>>,
        within_heap: &Arc<Mutex<HashSet<u32>>>,
        new_nodes: Vec<Node>,
        target_node_id: u32,
    ) {
        for node in new_nodes {
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
    }

    pub async fn recursive_find_nodes(self: Arc<Self>, target_node_id: u32) -> Vec<NodeDistance> {
        info!("Running recursive get_nodes({target_node_id})");
        let mut set = JoinSet::new();
        let mut already_queried = HashSet::new();
        already_queried.insert(self.node_id); // avoid requesting to self

        // Select initial nodes
        let mut alpha_set = self.select_initial_nodes(target_node_id).await;
        alpha_set.truncate(ALPHA);

        info!("Alpha set: {:#?}", alpha_set);

        let max_heap: Arc<Mutex<BinaryHeap<NodeDistance>>> =
            Arc::new(Mutex::new(BinaryHeap::new()));
        let within_heap: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));

        for distance_node in alpha_set.iter().cloned() {
            within_heap.lock().unwrap().insert(distance_node.node.id);
            max_heap.lock().unwrap().push(distance_node);
        }

        loop {
            // Send parallel FIND_NODE requests
            for distance_node in alpha_set.iter().cloned() {
                let within_heap = Arc::clone(&within_heap);
                let max_heap = Arc::clone(&max_heap);
                let kademlia = self.clone();

                already_queried.insert(distance_node.node.id);

                set.spawn(async move {
                    // info!("Getting K closest from neighbor ({:#?})", distance_node);
                    let k_closest_nodes = kademlia
                        .send_find_node(distance_node.node.clone(), target_node_id)
                        .await;
                    info!("{:#?}", k_closest_nodes);
                    kademlia.update_closest_nodes(
                        &max_heap,
                        &within_heap,
                        k_closest_nodes,
                        target_node_id,
                    );
                });
            }

            // Join all tasks
            while set.join_next().await.is_some() {}

            // Resend to new unqueried nodes
            let new_closest = max_heap
                .lock()
                .unwrap()
                .clone()
                .into_sorted_vec()
                .iter()
                .filter(|n| !already_queried.contains(&n.node.id))
                .cloned()
                .collect::<Vec<NodeDistance>>();

            if new_closest.is_empty() {
                break;
            }

            alpha_set = new_closest[0..std::cmp::min(ALPHA, new_closest.len())].to_vec();
        }

        let max_heap = max_heap.lock().unwrap();
        let result = max_heap
            .clone()
            .into_sorted_vec()
            .iter()
            .filter(|n| n.node.id != self.node_id)
            .cloned()
            .collect::<Vec<NodeDistance>>();

        info!("[recursive_find_node] result: {:#?}", result);

        result
    }

    // duplication from find_node, but subtle and important differences
    // rather duplicate than make one function do many different things
    pub async fn recursive_get_peers(self: Arc<Self>, info_hash: u32) -> Vec<(IpAddr, u16)> {
        // TODO: nodes that fail to respond quickly are removed from consideration until and unless they do respond.
        let mut set = JoinSet::new();
        let mut already_queried = HashSet::new();
        already_queried.insert(self.node_id); // avoid requesting to self

        println!("Currently looking for infohash {}", info_hash);
        // pick α nodes from closest non-empty k-bucket, even if less than α entries
        let mut alpha_set = self.select_initial_nodes(info_hash).await;
        alpha_set.truncate(ALPHA);

        if alpha_set.is_empty() {
            // find locally, most likely we're the only node
            let peer_store_guard = self.context.peer_store.lock().await;
            let peers = peer_store_guard.get(&info_hash.to_string());
            if let Some(peers) = peers {
                let values = peers
                    .iter()
                    .map(|peer| (peer.ip, peer.port))
                    .collect::<Vec<(IpAddr, u16)>>();
                return values;
            }
        }

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
                let max_heap = Arc::clone(&max_heap);
                let kademlia = self.clone();
                // updated k closest
                already_queried.insert(distance_node.node.id);
                set.spawn(async move {
                    let get_peers_res = kademlia
                        .send_get_peers(distance_node.node.clone(), info_hash.to_string())
                        .await;
                    info!("[CLIENT] get_peers response: {:#?}", get_peers_res);
                    match get_peers_res.value {
                        NodeOrPeer::Peers(peers) => Some(peers),
                        NodeOrPeer::Nodes(k_closest_nodes) => {
                            kademlia.update_closest_nodes(
                                &max_heap,
                                &within_heap,
                                k_closest_nodes,
                                info_hash,
                            );
                            None
                        }
                    }
                });
            }
            // JOIN all these tasks
            while let Some(result) = set.join_next().await {
                if let Ok(result) = result {
                    if let Some(peers) = result {
                        info!("[recursive_get_peers] result: {:#?}", peers);
                        return peers;
                    }
                }
            }

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

            alpha_set = new_closest[0..ALPHA].to_vec();
        }

        vec![]
    }

    // fyi: refresh periodically too besides only when joining
    // if no node lookup for bucket range has been done within 1hr
    pub async fn refresh_bucket(self: Arc<Self>, bucket_idx: usize) {
        info!("Refreshing bucket #{bucket_idx}");
        let node_id = self
            .context
            .routing_table
            .lock()
            .await
            .get_refresh_target(bucket_idx);
        self.recursive_find_nodes(node_id).await;
    }

    pub async fn announce_peer(self: Arc<Self>, info_hash: String) {
        let closest_nodes = self
            .clone()
            .recursive_find_nodes(info_hash.parse().unwrap())
            .await;
        if closest_nodes.is_empty() {
            // store locally
            self.context
                .peer_store
                .lock()
                .await
                .entry(info_hash.clone())
                .or_default()
                .push(self.context.node.clone());
        } else {
            let mut tasks = vec![];
            for node_dist in closest_nodes {
                let node: Node = node_dist.node.clone();
                let info_hash = info_hash.clone();
                let kademlia = self.clone();
                let task = tokio::spawn(async move {
                    kademlia.send_announce_peer(node, info_hash).await;
                });
                tasks.push(task);
            }

            join_all(tasks).await;
        }
    }

    pub async fn listen(self: Arc<Self>) {
        info!(
            "Starting Kademlia node {} and listening on {}:{}",
            self.node_id, self.context.node.ip, self.context.node.port
        );
        loop {
            let mut buf = [0; 2048];
            let (len, addr) = self.socket.recv_from(&mut buf).await.unwrap();

            info!("Received packet of len = {len} from {:#?}", addr);

            let krpc_clone = Arc::clone(&self);
            tokio::spawn(async move {
                krpc_clone.handle_krpc_call(&buf, len, addr).await;
            });
        }
    }

    async fn handle_krpc_call(self: Arc<Self>, buf: &[u8; 2048], len: usize, addr: SocketAddr) {
        let query: KrpcMessage = serde_bencoded::from_bytes(&buf[..len]).unwrap();

        if query.y == "r" {
            // this is a response to an earlier request we made
            // future is ready
            let response: KrpcSuccessResponse = serde_bencoded::from_bytes(&buf[..len]).unwrap();
            self.manager.resolve_transaction(query.t, response);
        } else {
            let query: KrpcRequest = serde_bencoded::from_bytes(&buf[..len]).unwrap();

            info!("Received query from {:#?}", addr);
            info!("{:#?}", query);

            let source_id = query.a.get("id").unwrap().parse().unwrap();
            let source_node = Node::new(source_id, addr.ip(), addr.port());

            let check_for_eviction = {
                let mut routing_table = self.context.routing_table.lock().await;
                routing_table.upsert_node(source_node.clone())
            };

            if check_for_eviction {
                info!("Checking for eviction due to source node {}'s query onto queried node {}", source_id, self.context.node.id);
                loop {
                    let mut routing_table = self.context.routing_table.lock().await;
                    let mut curr_bucket = routing_table.buckets[routing_table.find_bucket_idx(source_id)].clone();

                    let mut oldest_node = curr_bucket.front().unwrap().clone();

                    // if we loop through all the questionable nodes and encounter a node that isn't questionable, then we disregard the new node
                    if !oldest_node.is_questionable(){
                        info!("Oldest node not questionable, new source node is discarded");
                        break;
                    }

                    let addr = format!("{}:{}", oldest_node.ip, oldest_node.port);
                    
                    info!("Node {} sent a ping to node {} to check if it should be evicted", self.context.node.id, oldest_node.id);
                    let mut response = self.send_ping(&addr).await;

                    // retry once
                    if response.is_none() {
                        info!("Node {} didn't respond to node {}'s ping and the ping is being retried", oldest_node.id, self.context.node.id);
                        response = self.send_ping(&addr).await;
                    }

                    // if we get a response, we need to upsert the oldest node and update its last seen and continue our search
                    if !response.is_none() {
                        info!("Node {} responded to node {}'s ping and is being updated in the bucket", oldest_node.id, self.context.node.id);
                        oldest_node.update_last_seen();
                        routing_table.upsert_node(oldest_node);
                    } 
                    // if we don't get a reponse after second try, we must remove it and insert the current node
                    else {
                        info!("Node {} didn't respond to node {}'s ping and is being removed from the bucket to make place for node {}", oldest_node.id, self.context.node.id, source_id);
                        curr_bucket.pop_front();
                        routing_table.upsert_node(source_node.clone());
                    }
                }
            } else {
                info!("Eviction not necessary, source node {} is succesfully upserted to {}'s bucket", source_id, self.context.node.id);
            }

            self.clone().reset_timer(source_id as usize);
            // Step 3: Dispatch to respective handler functions
            let return_values = match query.q.as_str() {
                "ping" => self.handle_ping().await,
                "find_node" => self.handle_find_node(&query).await,
                "get_peers" => self.handle_get_peers(&query, source_node.clone()).await,
                "announce_peer" => self.handle_announce_peer(&query, source_node.clone()).await,
                _ => HashMap::new(),
            };

            // Step 4: Send the response back
            let response = KrpcSuccessResponse::from_request(&query, return_values);
            self.send_response(response, source_node).await;
        }
    }

    pub async fn handle_ping(&self) -> HashMap<String, String> {
        HashMap::from([(String::from("id"), self.node_id.to_string())])
    }

    pub async fn handle_find_node(&self, query: &KrpcRequest) -> HashMap<String, String> {
        let target_node_id = query.a.get("target").unwrap().parse().unwrap();
        info!("Received find_nodes RPC for target {target_node_id}");
        let k_closest_nodes = self
            .context
            .routing_table
            .lock()
            .await
            .get_nodes(target_node_id);
        info!("{:#?}", k_closest_nodes);
        let compact_node_info = k_closest_nodes
            .iter()
            .map(|node| node.get_node_compact_format())
            .collect::<Vec<String>>()
            .concat();
        HashMap::from([
            ("id".to_string(), self.node_id.to_string()),
            ("nodes".to_string(), compact_node_info),
        ])
    }

    async fn handle_get_peers(
        &self,
        query: &KrpcRequest,
        source_node: Node,
    ) -> HashMap<String, String> {
        let info_hash = query.a.get("info_hash").unwrap();

        let mut return_values = HashMap::from([("id".into(), self.node_id.to_string())]);

        let peer_store_guard = self.context.peer_store.lock().await;
        let peers = peer_store_guard.get(info_hash);
        if let Some(peers) = peers {
            let values = peers
                .iter()
                .map(|peer| peer.get_peer_compact_format())
                .collect::<Vec<String>>()
                .concat();
            return_values.insert("values".into(), values);
        } else {
            let k_closest_nodes = self
                .context
                .routing_table
                .lock()
                .await
                .get_nodes(info_hash.parse().unwrap());
            let compact_node_info = k_closest_nodes
                .iter()
                .map(|node| node.get_node_compact_format())
                .collect::<Vec<String>>()
                .concat();
            return_values.insert("nodes".into(), compact_node_info);
        }

        // generate token (hash ip + secret)
        let mut hasher = Sha1::new();
        if let IpAddr::V4(v4addr) = source_node.ip {
            hasher.update(v4addr.octets());
        }
        hasher.update(*self.context.secret.lock().unwrap());
        let token = hex::encode(hasher.finalize());

        return_values.insert("token".into(), token);
        return_values
    }

    async fn handle_announce_peer(
        &self,
        query: &KrpcRequest,
        source_node: Node,
    ) -> HashMap<String, String> {
        let info_hash = query.a.get("info_hash").unwrap();
        let token = query.a.get("token").unwrap();

        let mut hasher = Sha1::new();
        if let IpAddr::V4(querying_ip) = source_node.ip {
            hasher.update(querying_ip.octets());
        }
        hasher.update(*self.context.secret.lock().unwrap());

        let target_token = hex::encode(hasher.finalize());

        if *token != target_token {
            error!("[AUTH] Token is not valid")
            // send failure message
        }

        info!("Storing info hash in peer store");
        self.context
            .peer_store
            .lock()
            .await
            .entry(info_hash.clone())
            .or_default()
            .push(source_node);
        info!(
            "[CONFIRM] {:#?}",
            self.context
                .peer_store
                .lock()
                .await
                .get(info_hash.as_str())
        );

        HashMap::from([("id".into(), self.node_id.to_string())])
    }

    pub fn republish_peer_task(self: Arc<Self>) {
        let log_clone = self.context.announce_log.clone();
        // republish k-v pairs every 1 hr
        tokio::spawn(async move {
            let kademlia = self.clone();
            loop {
                sleep(Duration::from_secs(60 * 60)).await;
                for info_hash in log_clone.iter() {
                    kademlia.clone().announce_peer(info_hash.clone()).await;
                }
            }
        });
    }
}
