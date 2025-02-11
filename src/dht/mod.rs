use std::{net::SocketAddrV4, str::FromStr, sync::Arc, thread};

use easy_upnp::{add_ports, PortMappingProtocol, UpnpConfig};
use node::Node;
use tiny_http::{Request, Response, Server};
use tokio::{
    net::lookup_host,
    runtime::{Handle, Runtime},
};

use crate::{config::CONFIG, dht::kademlia::Kademlia};

pub mod compact;
pub mod context;
pub mod kademlia;
pub mod key;
pub mod node;
pub mod routing;

pub async fn start_dht() {
    let kademlia = Arc::new(Kademlia::init().await);

    let bootstrap = {
        let ip = lookup_host(&CONFIG.network.dht_bootstrap)
            .await
            .expect("Unable to resolve boostrap node addr")
            .next()
            .unwrap()
            .ip();

        let ip = match ip {
            std::net::IpAddr::V4(ip) => ip,
            _ => panic!("Only IPv4 addresses are supported"),
        };

        let response = kademlia
            .send_ping_init(&CONFIG.network.dht_bootstrap)
            .await
            .expect("Unable to communicate with boostrap node");
        let node_id = response.extract_id();

        Node::new(node_id, ip, CONFIG.network.dht_port)
    };

    let kademlia_clone = kademlia.clone();
    thread::spawn(move || {
        let server = Server::http(format!("0.0.0.0:{}", CONFIG.network.dht_api_port)).unwrap();
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
                .recursive_get_peers(info_hash.to_string().into())
                .await;
            let peers = serde_json::to_string(&peers).unwrap();
            request
                .respond(Response::from_string(peers).with_header(
                    tiny_http::Header::from_str("Content-Type: application/json").unwrap(),
                ))
                .unwrap();
        }
        ("POST", _) if url.starts_with("/announce/") => {
            let info_hash = url.trim_start_matches("/announce/");
            let info_hash = info_hash.to_string().into();
            let handle = Handle::current();
            handle.spawn(kademlia.announce_peer(info_hash));
            request
                .respond(Response::from_string(
                    "Announced as peer to requested torrent",
                ))
                .unwrap();
        }
        ("POST", _) if url.starts_with("/closest/") => {
            let id_str = url.trim_start_matches("/closest/");
            let id = id_str.to_string().into();
            let closest_nodes = kademlia.recursive_find_nodes(id).await;

            let closest_nodes = serde_json::to_string(&closest_nodes).unwrap();

            request
                .respond(Response::from_string(closest_nodes).with_header(
                    tiny_http::Header::from_str("Content-Type: application/json").unwrap(),
                ))
                .unwrap();
        }
        ("GET", "/ping") => {
            let ip_port = url.trim_start_matches("/ping");

            if let Ok(addr) = SocketAddrV4::from_str(ip_port) {
                let ping_response = kademlia.send_ping(addr).await;
                let ping_response = serde_json::to_string(&ping_response).unwrap();
                request
                    .respond(Response::from_string(ping_response).with_header(
                        tiny_http::Header::from_str("Content-Type: application/json").unwrap(),
                    ))
                    .unwrap();
            } else {
                request
                    .respond(Response::from_string("Invalid address").with_status_code(400))
                    .unwrap();
            }
        }
        _ => {
            request
                .respond(Response::from_string("404 Not Found").with_status_code(404))
                .unwrap();
        }
    }
}
