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

use std::{collections::HashMap, net::{IpAddr, SocketAddr}, sync::Arc};
use sha1::Digest;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use tokio::net::UdpSocket;

use crate::{
    context::RuntimeContext, node::Node, utils::deserialize_compact_node, utils::gen_trans_id,
};

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
pub struct KrpcRequest {
    t: String,
    y: String,

    q: String,
    a: HashMap<String, String>,
}

impl KrpcRequest {
    pub fn new(request_type: &str, arguments: HashMap<String, String>) -> Self {
        let request = KrpcRequest {
            t: gen_trans_id(),
            y: "q".into(),
            q: request_type.into(),
            a: arguments,
        };

        request
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

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct KrpcErrorResponse {
    t: String,
    y: String,

    e: (u8, String),
}

pub struct Krpc {
    pub socket: Arc<UdpSocket>,
    pub context: Arc<RuntimeContext>,
    node_id: u32,
}

impl Krpc {
    pub async fn init(context: Arc<RuntimeContext>) -> Self {
        let socket = Arc::new(
            UdpSocket::bind(format!("127.0.0.1:{}", context.node.port))
                .await
                .unwrap(),
        );
        let node_id = context.node.id;

        Self {
            socket,
            node_id,
            context,
        }
    }

    pub async fn send_request(&self, query: KrpcRequest, addr: &str) -> KrpcSuccessResponse {
        let query = serde_bencode::to_bytes(&query).unwrap();
        self.socket.send_to(&query, addr).await.unwrap();

        let mut buf = [0; 2048];
        let (amt, _) = self.socket.recv_from(&mut buf).await.unwrap();
        let response: KrpcSuccessResponse = serde_bencode::from_bytes(&buf[..amt]).unwrap();

        response
    }

    pub async fn send_response(&self, response: KrpcSuccessResponse, source_node: Node) {
        let addr = SocketAddr::new(source_node.ip, source_node.port);
        let response = serde_bencode::to_bytes(&response).unwrap();
        self.socket.send_to(&response, addr).await.unwrap();
    }

    pub async fn send_ping(&self, addr: &str) -> KrpcSuccessResponse {
        let mut arguments = HashMap::new();
        arguments.insert("id".into(), "client".into());
        let request = KrpcRequest::new("ping", arguments);
        let response = self.send_request(request, addr).await;
        response
    }

    pub async fn send_find_node(&self, target_node: &Node) -> Vec<Node> {
        let mut arguments = HashMap::new();
        arguments.insert("id".into(), self.node_id.to_string());
        arguments.insert("target".into(), target_node.id.to_string());

        let request = KrpcRequest::new("ping", arguments);
        let addr = format!("{}:{}", target_node.ip, target_node.port);

        let response: KrpcSuccessResponse = self.send_request(request, &addr).await;
        println!("Received {:#?} from {}", response, addr);

        let serialized_nodes = response.r.get("nodes");
        deserialize_compact_node(serialized_nodes)
    }

    pub async fn listen(self: Arc<Self>) {
        loop {
            let mut buf = [0; 2048];
            let (len, addr) = self.socket.recv_from(&mut buf).await.unwrap();

            let krpc_clone = Arc::clone(&self); 
            tokio::spawn(async move {
                krpc_clone.handle_krpc_call(&buf, len, addr).await;
            });
        }
    }

    async fn handle_krpc_call(&self, buf: &[u8; 2048], len: usize, addr: SocketAddr) {
        let query: KrpcRequest = serde_bencode::from_bytes(&buf[..len]).unwrap();

        let source_id = query.a.get("id").unwrap().parse().unwrap();
        let source_node = Node {
            id: source_id,
            ip: addr.ip(),
            port: addr.port(),
        };

        // Update the routing table's status of the source node
        let needs_evicting = self.context.routing_table.lock().unwrap().upsert_node(source_node.clone());
        

        let return_values = match query.q.as_str() {
            "ping" => self.handle_ping().await,
            "find_node" => self.handle_find_node(&query).await,
            "get_peers" => self.handle_get_peers(&query, source_node.clone()).await,
            "announce_peer" => self.handle_announce_peer(&query, source_node.clone()).await,
            _ => HashMap::new()
        };

        let response = KrpcSuccessResponse::from_request(&query, return_values);
        self.send_response(response, source_node.clone()).await;
    }

    async fn handle_ping(&self) -> HashMap<String, String> {
        HashMap::from([(String::from("id"), self.node_id.to_string())])
    }

    pub async fn handle_find_node(&self, query: &KrpcRequest) -> HashMap<String, String> {
        let target_node_id = query.a.get("target").unwrap().parse().unwrap();
        let k_closest_nodes = self.context.routing_table.lock().unwrap().get_nodes(target_node_id);
        let compact_node_info = k_closest_nodes
            .iter()
            .map(|node| node.get_node_compact_format())
            .collect::<Vec<String>>()
            .concat();
        HashMap::from([("id".to_string(), self.node_id.to_string()), ("nodes".to_string(), compact_node_info)])
    }

    async fn handle_get_peers(&self, query: &KrpcRequest, source_node: Node) -> HashMap<String, String> {
        let info_hash = query.a.get("info_hash").unwrap();

        let mut return_values = HashMap::from([("id".into(), self.node_id.to_string())]);

        let peer_store_guard = self.context.peer_store.lock().unwrap();
        let peers = peer_store_guard.get(info_hash);
        if let Some(peers) = peers {
            let values = peers
                .iter()
                .map(|peer| peer.get_peer_compact_format())
                .collect::<Vec<String>>()
                .concat();
            return_values.insert("values".into(), values); // this won't work rn
        } else {
            let k_closest_nodes = self.context.routing_table.lock().unwrap().get_nodes(info_hash.parse().unwrap());
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
        let token = format!("{:x}", hasher.finalize());

        return_values.insert("token".into(), token);
        return_values
    }

    async fn handle_announce_peer(&self, query: &KrpcRequest, source_node: Node) -> HashMap<String, String> {
        let info_hash = query.a.get("info_hash").unwrap();
        let token = query.a.get("token").unwrap();

        let mut hasher = Sha1::new();
        if let IpAddr::V4(querying_ip) = source_node.ip {
            hasher.update(querying_ip.octets());
        }
        hasher.update(*self.context.secret.lock().unwrap());

        let target_token = format!("{:x}", hasher.finalize());

        if *token != target_token {
            // send failure message
        }

        self.context.peer_store
            .lock()
            .unwrap()
            .entry(info_hash.clone())
            .or_default()
            .push(source_node);

        HashMap::from([("id".into(), self.node_id.to_string())])
    }
}
