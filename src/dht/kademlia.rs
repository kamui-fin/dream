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

use crate::dht::compact;
use anyhow::anyhow;
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::future::join_all;
use hex::encode;
use log::{error, info, warn};
use serde::{
    de::{self, DeserializeOwned},
    Deserialize, Serialize,
};
use serde_bytes::ByteBuf;
use sha1::{Digest, Sha1};
use tokio::{
    net::UdpSocket,
    sync::{oneshot, Semaphore},
    task::{JoinHandle, JoinSet},
    time::{sleep, timeout, Instant},
};

use crate::dht::{
    config::{Args, ALPHA, K, REFRESH_TIME},
    context::RuntimeContext,
    key::{gen_trans_id, ID_SIZE},
    node::{Node, NodeDistance},
};

use super::key::Key;

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

    fn create_future(&self, tx_id: Vec<u8>) -> oneshot::Receiver<KrpcSuccessResponse> {
        let (tx, rx) = oneshot::channel();
        let tx_id = hex::encode(tx_id);
        self.transactions.lock().unwrap().insert(tx_id, tx);
        rx
    }

    fn resolve_transaction(&self, tx_id: Vec<u8>, value: KrpcSuccessResponse) {
        let tx_id = hex::encode(tx_id);
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
    #[serde(with = "serde_bytes")]
    t: Vec<u8>,
    y: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub struct KrpcRequest {
    #[serde(with = "serde_bytes")]
    t: Vec<u8>,
    y: String,

    q: String,
    a: HashMap<String, ByteBuf>,
}

impl KrpcRequest {
    pub fn new(request_type: &str, arguments: HashMap<String, ByteBuf>) -> Self {
        KrpcRequest {
            t: gen_trans_id(),
            y: "q".into(),
            q: request_type.into(),
            a: arguments,
        }
    }
}

pub enum KrpcRequestType {
    Ping,
    FindNode,
    GetPeers,
    AnnouncePeer,
}
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct KrpcSuccessResponse {
    #[serde(default)]
    ip: Option<ByteBuf>,

    #[serde(with = "serde_bytes")]
    t: Vec<u8>,
    y: String,

    r: ResponseBody,
}

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct ResponseBody {
    pub id: Key,

    // Only present in responses to GetPeers
    #[serde(
        with = "compact::values",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub values: Vec<SocketAddrV4>,

    #[serde(
        with = "compact::nodes",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub nodes: Vec<Node>,

    // present in responses to GetPeers
    #[serde(with = "serde_bytes", default, skip_serializing_if = "Option::is_none")]
    pub token: Option<Vec<u8>>,
}

impl KrpcSuccessResponse {
    pub fn from_request(request: &KrpcRequest, body: ResponseBody) -> Self {
        Self {
            ip: None,
            t: request.t.clone(),
            y: "r".into(),
            r: body,
        }
    }

    pub fn extract_id(&self) -> Key {
        self.r.id
    }
}

impl fmt::Display for KrpcRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KrpcRequest {{ t: {}, y: {}, q: {}, a: {:?} }}",
            hex::encode(&self.t),
            self.y,
            self.q,
            self.a
                .iter()
                .map(|(k, v)| (k, Key::from(v)))
                .collect::<HashMap<_, _>>()
        )
    }
}

impl fmt::Display for KrpcSuccessResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KrpcSuccessResponse {{ t: {}, y: {}, r: {:?} }}",
            hex::encode(&self.t),
            self.y,
            self.r
        )
    }
}

// debug for KrpcRequest
impl fmt::Debug for KrpcRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KrpcRequest {{ t: {}, y: {}, q: {}, a: {:?} }}",
            hex::encode(&self.t),
            self.y,
            self.q,
            self.a
                .iter()
                .map(|(k, v)| (k, Key::from(v)))
                .collect::<HashMap<_, _>>()
        )
    }
}

#[derive(Debug)]
pub enum NodeOrPeer {
    Peers(Vec<SocketAddrV4>),
    Nodes(Vec<Node>),
}

#[derive(Debug)]
pub struct GetPeersResponse {
    pub token: Option<Vec<u8>>,
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
    node_id: Key,
    pub timers: Arc<Mutex<HashMap<usize, JoinHandle<()>>>>,
}

async fn get_public_ip() -> Ipv4Addr {
    let ip = public_ip::addr().await.unwrap();
    if let IpAddr::V4(ipv4) = ip {
        ipv4
    } else {
        panic!("Only IPv4 addresses are supported");
    }
}

impl Kademlia {
    pub async fn init(args: &Args) -> Self {
        let context = Arc::new(RuntimeContext::init(args, get_public_ip().await));
        let socket = UdpSocket::bind(format!("0.0.0.0:{}", args.port))
            .await
            .unwrap();
        let socket = Arc::new(socket); // Set socket options to reuse address and port
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

        error!("Waiting for listen to finish");
        handle.await.unwrap();
    }

    pub async fn join_dht_network(self: Arc<Self>, bootstrap_node: Option<Node>) {
        if bootstrap_node.is_none() {
            return;
        }
        let bootstrap_node = bootstrap_node.unwrap();
        // info!(
        //     "Joining network through bootstrap id = {}",
        //     bootstrap_node.id
        // );
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

        // info!(
        //     "Populating routing table with k closest nodes = {:#?}",
        //     k_closest_nodes
        // );

        // 3. refresh buckets past closest node bucket
        let closest_idx = self
            .clone()
            .context
            .routing_table
            .lock()
            .await
            .find_bucket_idx(k_closest_nodes[0].node.id);

        for idx in (closest_idx + 1)..ID_SIZE {
            // self.clone().reset_timer(idx);
            let self_clone = self.clone();
            self_clone.refresh_bucket(idx).await;
        }
    }

    pub async fn send_request(
        &self,
        query: KrpcRequest,
        addr: SocketAddrV4,
    ) -> Option<KrpcSuccessResponse> {
        info!("[CLIENT] Sending query to {addr}: {:?}", query);
        let transaction_id = query.t.clone();
        let query = serde_bencoded::to_vec(&query).unwrap();

        self.socket.send_to(&query, addr).await.ok()?;

        let future = self.manager.create_future(transaction_id);
        match timeout(Duration::from_millis(300), future).await {
            Ok(result) => result.ok(),
            Err(_elapsed) => {
                info!("Timed out waiting for response from {addr}");
                None
            }
        }
    }

    pub async fn send_response(&self, response: KrpcSuccessResponse, source_node: Node) {
        info!(
            "[SERVER] Sending response {:?} to {:?}",
            response, source_node
        );
        let response = serde_bencode::to_bytes(&response).unwrap();

        self.socket
            .send_to(&response, source_node.addr)
            .await
            .unwrap();
    }

    pub async fn send_ping(&self, addr: SocketAddrV4) -> Option<KrpcSuccessResponse> {
        let arguments = HashMap::from([("id".to_string(), self.node_id.to_vec().into())]);
        let request = KrpcRequest::new("ping", arguments);

        self.send_request(request, addr).await
    }

    pub async fn send_ping_init(&self, addr: &str) -> Option<KrpcSuccessResponse> {
        info!("Sending initial ping to {addr}");
        let arguments = HashMap::from([("id".to_string(), self.node_id.to_vec().into())]);
        let query = KrpcRequest::new("ping", arguments);
        let query = serde_bencode::to_bytes(&query).expect("Failed to serialize query");

        if let Err(e) = self.socket.send_to(&query, addr).await {
            eprintln!("Failed to send query: {e}");
            return None;
        }

        let mut buf = [0; 2048];
        match timeout(Duration::from_millis(500), self.socket.recv_from(&mut buf)).await {
            Ok(Ok((len, addr))) => {
                let response: KrpcSuccessResponse = serde_bencode::from_bytes(&buf[..len]).unwrap();
                Some(response)
            }
            Ok(Err(e)) => {
                eprintln!("Failed to receive response: {e}");
                None
            }
            Err(_) => {
                eprintln!("Timeout while waiting for response");
                None
            }
        }
    }

    pub async fn send_find_node(
        &self,
        dest: Node,
        target_node_id: Key,
    ) -> anyhow::Result<Vec<Node>> {
        let arguments = HashMap::from([
            ("id".to_string(), self.node_id.to_vec().into()),
            ("target".into(), target_node_id.to_vec().into()),
        ]);
        let request = KrpcRequest::new("find_node", arguments);

        let response = self.send_request(request, dest.addr).await;
        if let Some(response) = response {
            Ok(response.r.nodes)
        } else {
            Err(anyhow!("Failed to send find_node request"))
        }
    }

    pub async fn send_get_peers(&self, dest: Node, info_hash: Key) -> Option<GetPeersResponse> {
        let arguments = HashMap::from([
            ("id".to_string(), self.node_id.to_vec().into()),
            ("info_hash".into(), info_hash.to_vec().into()),
        ]);
        let request = KrpcRequest::new("get_peers", arguments);
        let response = self.send_request(request, dest.addr).await?;

        info!("get_peers response = {:?}", response);

        let token = response.r.token.clone();

        let nodes = response.r.nodes;
        let peers = response.r.values;

        if !nodes.is_empty() {
            Some(GetPeersResponse {
                token,
                value: NodeOrPeer::Nodes(nodes),
            })
        } else {
            Some(GetPeersResponse {
                token,
                value: NodeOrPeer::Peers(peers),
            })
        }
    }

    pub async fn send_announce_peer(
        &self,
        dest: Node,
        info_hash: Key,
    ) -> Option<KrpcSuccessResponse> {
        info!(
            "Sending announce peer for hash {:#?} to dest {:#?}",
            info_hash, dest
        );
        let get_peers = self.send_get_peers(dest.clone(), info_hash.clone()).await?;

        info!("First, get_peers response = {:#?}", get_peers);

        let mut arguments = HashMap::from([
            ("id".to_string(), self.node_id.to_vec().into()),
            ("info_hash".into(), info_hash.to_vec().into()),
        ]);

        if let Some(token) = get_peers.token {
            arguments.insert("token".into(), token.into());
        }

        let request = KrpcRequest::new("announce_peer", arguments);

        self.send_request(request, dest.addr).await
    }

    pub async fn recursive_find_nodes(self: Arc<Self>, target_node_id: Key) -> Vec<NodeDistance> {
        let contacted = Arc::new(Mutex::new(HashSet::new()));
        let candidates = Arc::new(Mutex::new(BinaryHeap::new()));
        let closest_nodes = Arc::new(Mutex::new(BinaryHeap::new()));
        let semaphore = Arc::new(Semaphore::new(ALPHA));

        // Initialize candidates from routing table
        {
            let initial_candidates = self
                .context
                .routing_table
                .lock()
                .await
                .get_nodes(target_node_id);

            let mut cand_guard = candidates.lock().unwrap();
            for cand in initial_candidates {
                cand_guard.push(Reverse(NodeDistance {
                    node: cand.clone(),
                    dist: target_node_id.distance(&cand.id),
                }));
            }
        }

        loop {
            let mut current_batch = Vec::with_capacity(ALPHA);

            // Acquire permits for parallel queries
            let permits = {
                let mut permits = Vec::with_capacity(ALPHA);
                for _ in 0..ALPHA {
                    match semaphore.clone().acquire_owned().await {
                        Ok(permit) => permits.push(permit),
                        Err(_) => break,
                    }
                }
                permits
            };

            if permits.is_empty() {
                break;
            }

            // Get batch of candidates
            {
                let mut cand_guard = candidates.lock().unwrap();
                while current_batch.len() < ALPHA && !cand_guard.is_empty() {
                    if let Some(candidate) = cand_guard.pop() {
                        current_batch.push(candidate);
                    }
                }
            }

            if current_batch.is_empty() {
                break;
            }

            // Process batch in parallel
            let mut futures = Vec::new();
            for candidate in current_batch {
                let self_clone = self.clone();
                let target = target_node_id;
                let contacted_clone = contacted.clone();
                let candidates_clone = candidates.clone();
                let closest_clone = closest_nodes.clone();
                let semaphore_clone = semaphore.clone();

                let candidate = candidate.0;

                if candidate.node.id == self.node_id {
                    continue;
                }

                futures.push(tokio::spawn(async move {
                    // Check if already contacted
                    {
                        let mut contacted_guard = contacted_clone.lock().unwrap();
                        if contacted_guard.contains(&candidate.node.id) {
                            return;
                        }
                        contacted_guard.insert(candidate.node.id.clone());
                    }

                    match self_clone
                        .send_find_node(candidate.node.clone(), target)
                        .await
                    {
                        Ok(nodes) => {
                            // Process discovered nodes
                            let mut new_candidates = BinaryHeap::new();
                            for node in nodes {
                                let dist = target.distance(&node.id);
                                new_candidates.push(NodeDistance { node, dist });
                            }

                            // Merge candidates
                            {
                                let mut cand_guard = candidates_clone.lock().unwrap();
                                for nd in new_candidates {
                                    if cand_guard.len() < K
                                        || nd.dist < cand_guard.peek().unwrap().0.dist
                                    {
                                        cand_guard.push(Reverse(nd));
                                    }
                                }
                            }

                            // Update closest nodes
                            {
                                let mut closest_guard = closest_clone.lock().unwrap();
                                closest_guard.push(candidate.clone());
                                while closest_guard.len() > K {
                                    closest_guard.pop();
                                }
                            }

                            self_clone
                                .context
                                .routing_table
                                .lock()
                                .await
                                .upsert_node(candidate.node);
                        }
                        Err(_) => {
                            self_clone
                                .context
                                .routing_table
                                .lock()
                                .await
                                .remove_node(candidate.node.id);
                        }
                    }

                    drop(semaphore_clone);
                }));
            }
            // Wait for batch completion
            futures::future::join_all(futures).await;

            // Check termination condition
            let should_terminate = {
                let closest_guard = closest_nodes.lock().unwrap();
                let cand_guard = candidates.lock().unwrap();

                // What to do if current closest nodes is empty?
                // It means current batch of candidates did not respond at all
                if let Some(closest_node) = closest_guard.peek() {
                    let no_closer_candidates =
                        cand_guard.iter().all(|c| c.0.dist >= closest_node.dist);
                    closest_guard.len() >= K && no_closer_candidates
                } else {
                    false // keep trying other candidates
                }
            };
            warn!(
                "AFTER - Current closest nodes size = {:#?}",
                closest_nodes.lock().unwrap().len()
            );

            if should_terminate {
                break;
            }
        }

        // Return final closest nodes
        let closest_guard = closest_nodes.lock().unwrap();
        info!("Computed K closest nodes to target {:#?}", target_node_id);
        let mut k_closest: Vec<NodeDistance> = closest_guard.iter().take(K).cloned().collect();
        k_closest.sort();

        k_closest
    }

    pub async fn recursive_get_peers(self: Arc<Self>, info_hash: Key) -> Vec<(IpAddr, u16)> {
        vec![]
    }

    // fyi: refresh periodically too besides only when joining
    // if no node lookup for bucket range has been done within 1hr
    pub async fn refresh_bucket(self: Arc<Self>, bucket_idx: usize) {
        error!("Refreshing bucket #{bucket_idx}");
        let node_id = self
            .context
            .routing_table
            .lock()
            .await
            .get_refresh_target(bucket_idx);
        self.recursive_find_nodes(node_id).await;
    }

    pub async fn announce_peer(self: Arc<Self>, info_hash: Key) {
        let closest_nodes = self.clone().recursive_find_nodes(info_hash.clone()).await;
        if closest_nodes.is_empty() {
            // store locally
            self.context
                .peer_store
                .lock()
                .await
                .entry(info_hash.clone())
                .or_default()
                .push(self.context.node.addr);
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
            "Starting Kademlia node {:#?} and listening on {:#?}",
            self.node_id, self.context.node.addr
        );
        loop {
            let mut buf = [0; 65507];
            info!("Listening for packet");
            let (len, addr) = self.socket.recv_from(&mut buf).await.unwrap();

            let addr = match addr {
                SocketAddr::V4(addr) => addr,
                _ => panic!("Only IPv4 addresses are supported"),
            };

            info!("Received packet of len = {len} from {:#?}", addr);

            let krpc_clone = Arc::clone(&self);
            tokio::spawn(async move {
                krpc_clone.handle_krpc_call(&buf, len, addr).await;
            });
        }
    }

    async fn handle_krpc_call(self: Arc<Self>, buf: &[u8; 65507], len: usize, addr: SocketAddrV4) {
        let query: KrpcMessage = serde_bencode::from_bytes(&buf[..len]).unwrap();

        info!("Received response from {:#?}", addr);

        if query.y == "r" {
            // this is a response to an earlier request we made
            // future is ready
            // let response: KrpcSuccessResponse = serde_bencoded::from_bytes(&buf[..len]).unwrap();
            let response: KrpcSuccessResponse = { serde_bencode::from_bytes(&buf[..len]).unwrap() };
            self.manager.resolve_transaction(query.t, response);
        } else {
            let query: KrpcRequest = match serde_bencode::from_bytes(&buf[..len]) {
                Ok(query) => query,
                Err(e) => {
                    error!("Failed to deserialize query: {:#?}", e);
                    return;
                }
            };

            info!("Received query from {:#?}", addr);
            info!("{:?}", query);

            let source_id = query.a.get("id").unwrap().into();
            let source_node = Node::from_addr(source_id, addr);

            let check_for_eviction = {
                let mut routing_table = self.context.routing_table.lock().await;
                routing_table.upsert_node(source_node.clone())
            };

            if check_for_eviction {
                info!(
                    "Checking for eviction due to source node {:?}'s query onto queried node {:?}",
                    source_id, self.context.node.id
                );
                loop {
                    let mut routing_table = self.context.routing_table.lock().await;
                    let mut curr_bucket =
                        routing_table.buckets[routing_table.find_bucket_idx(source_id)].clone();

                    let mut oldest_node = curr_bucket.front().unwrap().clone();

                    // if we loop through all the questionable nodes and encounter a node that isn't questionable, then we disregard the new node
                    if !oldest_node.is_questionable() {
                        info!("Oldest node not questionable, new source node is discarded");
                        break;
                    }

                    info!(
                        "Node {:?} sent a ping to node {:?} to check if it should be evicted",
                        self.context.node.id, oldest_node.id
                    );
                    let mut response = self.send_ping(oldest_node.addr).await;

                    // retry once
                    if response.is_none() {
                        info!("Node {:?} didn't respond to node {:?}'s ping and the ping is being retried", oldest_node.id, self.context.node.id);
                        response = self.send_ping(oldest_node.addr).await;
                    }

                    // if we get a response, we need to upsert the oldest node and update its last seen and continue our search
                    if response.is_some() {
                        info!("Node {:?} responded to node {:?}'s ping and is being updated in the bucket", oldest_node.id, self.context.node.id);
                        oldest_node.update_last_seen();
                        routing_table.upsert_node(oldest_node);
                    }
                    // if we don't get a reponse after second try, we must remove it and insert the current node
                    else {
                        info!("Node {:?} didn't respond to node {:?}'s ping and is being removed from the bucket to make place for node {:#?}", oldest_node.id, self.context.node.id, source_id);
                        curr_bucket.pop_front();
                        routing_table.upsert_node(source_node.clone());
                    }
                }
            } else {
                info!(
                    "Eviction not necessary, source node {:?} is succesfully upserted to {:?}'s bucket",
                    source_id, self.context.node.id
                );
            }

            self.clone().reset_timer(
                self.context
                    .routing_table
                    .lock()
                    .await
                    .find_bucket_idx(source_id) as usize,
            );

            let return_values: ResponseBody = match query.q.as_str() {
                "ping" => self.handle_ping().await,
                "find_node" => self.handle_find_node(&query).await,
                "get_peers" => self.handle_get_peers(&query, source_node.clone()).await,
                "announce_peer" => self.handle_announce_peer(&query, source_node.clone()).await,
                _ => panic!("Unknown query type"),
            };

            // Step 4: Send the response back
            let response = KrpcSuccessResponse::from_request(&query, return_values);
            self.send_response(response, source_node).await;
        }
    }

    pub async fn handle_ping(&self) -> ResponseBody {
        ResponseBody {
            id: self.node_id.clone(),
            values: vec![],
            nodes: vec![],
            token: None,
        }
    }

    pub async fn handle_find_node(&self, query: &KrpcRequest) -> ResponseBody {
        let target_node_id = query.a.get("target").unwrap().into();
        info!("Received find_nodes RPC for target {:#?}", target_node_id);
        let mut k_closest_nodes = self
            .context
            .routing_table
            .lock()
            .await
            .get_nodes(target_node_id);
        k_closest_nodes.truncate(K);

        ResponseBody {
            id: self.node_id.clone(),
            values: vec![],
            nodes: k_closest_nodes,
            token: None,
        }
    }

    async fn handle_get_peers(&self, query: &KrpcRequest, source_node: Node) -> ResponseBody {
        let info_hash = query.a.get("info_hash").unwrap().into();

        let mut body = ResponseBody {
            id: self.node_id.clone(),
            values: vec![],
            nodes: vec![],
            token: None,
        };

        let peer_store_guard = self.context.peer_store.lock().await;
        let peers = peer_store_guard.get(&info_hash);
        if let Some(peers) = peers {
            body.values = peers.clone();
        } else {
            let mut k_closest_nodes = self.context.routing_table.lock().await.get_nodes(info_hash);
            k_closest_nodes.truncate(K);
            body.nodes = k_closest_nodes;
        }

        // generate token (hash ip + secret)
        let mut hasher = Sha1::new();
        hasher.update(source_node.addr.ip().octets());
        hasher.update(*self.context.secret.lock().unwrap());

        let token: Vec<u8> = hasher.finalize().to_vec();

        body.token = Some(token);

        body
    }

    async fn handle_announce_peer(&self, query: &KrpcRequest, source_node: Node) -> ResponseBody {
        let info_hash = query.a.get("info_hash").unwrap().into();
        let token = query.a.get("token").unwrap();

        let mut hasher = Sha1::new();
        hasher.update(source_node.addr.ip().octets());
        hasher.update(*self.context.secret.lock().unwrap());

        let target_token = hasher.finalize().to_vec();

        if *token != target_token {
            error!("[AUTH] Token is not valid")
            // send failure message
        }

        info!("Storing info hash in peer store");
        self.context
            .peer_store
            .lock()
            .await
            .entry(info_hash)
            .or_default()
            .push(source_node.addr);

        info!(
            "[CONFIRM] {:#?}",
            self.context.peer_store.lock().await.get(&info_hash)
        );

        ResponseBody {
            id: self.node_id.clone(),
            values: vec![],
            nodes: vec![],
            token: None,
        }
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
