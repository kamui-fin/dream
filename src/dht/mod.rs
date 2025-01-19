use std::{net::IpAddr, str::FromStr, sync::Arc, thread};

use node::Node;
use tiny_http::{Request, Response, Server};
use tokio::runtime::{Handle, Runtime};
use utils::decode_node_id;

use crate::dht::{config::Args, kademlia::Kademlia};

pub mod config;
pub mod context;
pub mod kademlia;
pub mod node;
pub mod routing;
pub mod utils;

pub async fn start_dht(args: &Args) {
    let kademlia = Arc::new(Kademlia::init(args).await);
    let bootstrap = if let Some((ip, port)) = args.get_bootstrap() {
        let response = kademlia
            .send_ping(&format!("{}:{}", ip, port))
            .await
            .expect("Unable to communicate with boostrap node");
        let node_id = response.extract_id();
        Some(Node::new(
            node_id,
            IpAddr::from_str(&ip).expect("Unable to parse boostrap IP"),
            port,
        ))
    } else {
        None
    };

    let kademlia_clone = kademlia.clone();
    thread::spawn(move || {
        let server = Server::http(format!(
            "0.0.0.0:{}",
            1000 + kademlia_clone.context.node.port
        ))
        .unwrap();
        let rt = Runtime::new().unwrap();
        for request in server.incoming_requests() {
            rt.block_on(handle_http_request(kademlia_clone.clone(), request));
        }
    });

    kademlia.start_server(bootstrap).await;
}

async fn handle_http_request(kademlia: Arc<Kademlia>, request: Request) {
    let url = request.url().to_string();
    let method = request.method().as_str();

    match (method, url.as_str()) {
        ("GET", _) if url.starts_with("/peers/") => {
            let info_hash = url.trim_start_matches("/peers/");
            let peers = kademlia
                .recursive_get_peers(decode_node_id(info_hash.to_string()))
                .await;
            let peers = serde_json::to_string(&peers).unwrap();
            request
                .respond(Response::from_string(peers).with_header(
                    tiny_http::Header::from_str("Content-Type: application/json").unwrap(),
                ))
                .unwrap();
        }
        ("PUT", _) if url.starts_with("/announce/") => {
            let info_hash = url.trim_start_matches("/announce/");
            let handle = Handle::current();
            handle.spawn(kademlia.announce_peer(info_hash.into()));
            request
                .respond(Response::from_string(
                    "Announced as peer to requested torrent",
                ))
                .unwrap();
        }
        ("POST", _) if url.starts_with("/closest/") => {
            let id_str = url.trim_start_matches("/closest/");
            let id = decode_node_id(id_str.to_string());
            let closest_nodes = kademlia.recursive_find_nodes(id).await;

            let closest_nodes = serde_json::to_string(&closest_nodes).unwrap();

            request
                .respond(Response::from_string(closest_nodes).with_header(
                    tiny_http::Header::from_str("Content-Type: application/json").unwrap(),
                ))
                .unwrap();
        }
        ("GET", "/ping") => {
            let _ = url.trim_start_matches("/ping");

            let ping_response = kademlia.handle_ping().await;
            let ping_response = serde_json::to_string(&ping_response).unwrap();
            request
                .respond(Response::from_string(ping_response).with_header(
                    tiny_http::Header::from_str("Content-Type: application/json").unwrap(),
                ))
                .unwrap();
        }
        _ => {
            request
                .respond(Response::from_string("404 Not Found").with_status_code(404))
                .unwrap();
        }
    }
}
