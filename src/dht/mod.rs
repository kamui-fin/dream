use std::{
    net::SocketAddrV4,
    str::FromStr,
    sync::Arc,
    thread,
};

use config::Args;
use easy_upnp::{add_ports, PortMappingProtocol, UpnpConfig};
use node::Node;
use tiny_http::{Request, Response, Server};
use tokio::{
    net::lookup_host,
    runtime::{Handle, Runtime},
};

use crate::dht::kademlia::Kademlia;

pub mod compact;
pub mod config;
pub mod context;
pub mod kademlia;
pub mod key;
pub mod node;
pub mod routing;

const LEASE_DURATION: u32 = 3600; // 1 hour lease

pub async fn setup_upnp() -> Result<(), Box<dyn std::error::Error>> {
    let config = UpnpConfig {
        address: None, // Use default network interface
        port: 6881,
        protocol: PortMappingProtocol::UDP,
        duration: LEASE_DURATION,
        comment: "Kademlia DHT".to_string(),
    };

    // Add port mapping
    match add_ports(vec![config]).next() {
        Some(Ok(_)) => {
            log::info!("UPnP port forwarding established on port {}", 6881);

            // Setup cleanup on exit
            // ctrlc::set_handler(move || {
            //     cleanup_upnp().expect("Failed to clean up UPnP mapping");
            //     std::process::exit(0);
            // })?;
        }
        Some(Err(e)) => log::warn!("UPnP setup failed: {}", e),
        None => log::warn!("No UPnP gateway found"),
    }

    Ok(())
}

pub async fn start_dht(args: &Args) {
    setup_upnp().await.unwrap();

    let kademlia = Arc::new(Kademlia::init(args).await);

    let bootstrap = if let Some(addr) = args.bootstrap.clone() {
        let ip = lookup_host((addr, 0))
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
            .send_ping_init(&format!("{}:{}", ip, 6881))
            .await
            .expect("Unable to communicate with boostrap node");
        let node_id = response.extract_id();

        Some(Node::new(node_id, ip, 6881))
    } else {
        None
    };

    let kademlia_clone = kademlia.clone();
    thread::spawn(move || {
        let server = Server::http(format!(
            "0.0.0.0:{}",
            1000 + kademlia_clone.context.node.addr.port()
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
