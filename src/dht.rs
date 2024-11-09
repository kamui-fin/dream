use crate::{config::Args, kademlia::Kademlia};
use futures::executor::block_on;
use std::{str::FromStr, sync::Arc, thread};
use tokio::runtime::{Handle, Runtime};
use tiny_http::{Server, Response, Request};

pub async fn start_dht(args: &Args) {
    let kademlia = Arc::new(Kademlia::init(args).await);

    kademlia.start_server(args.get_bootstrap());

    let server = Server::http(format!("0.0.0.0:{}", 1000 + args.udp_port)).unwrap();
    for request in server.incoming_requests() {
        let url = request.url().to_string();
        let method = request.method().as_str();
        let kademlia_clone = kademlia.clone();

        match (method, url.as_str()) {
            ("GET", "/ping") => {
                request.respond(Response::from_string("pong")).unwrap();
                let ip = url.trim_start_matches("/ping");
                
            }
            ("GET", _) if url.starts_with("/get/") => {
                let info_hash = url.trim_start_matches("/get/");
                let peers = kademlia_clone.recursive_get_peers(info_hash.parse().unwrap()).await;
                let peers = serde_json::to_string(&peers).unwrap();
                request.respond(Response::from_string(peers).with_header(tiny_http::Header::from_str("Content-Type: application/json").unwrap())).unwrap();  // Replace with actual value
            }
            ("PUT", _) if url.starts_with("/put/") => {
                let info_hash = url.trim_start_matches("/put/");
                    request.respond(Response::from_string("Stored key-value")).unwrap();  // Replace with actual response
                } else {
                    request.respond(Response::from_string("Invalid PUT format").with_status_code(400)).unwrap();
                }
            }
            ("GET", _) if url.starts_with("/closest/") => {
                let id_str = url.trim_start_matches("/closest/");
                if let Ok(id) = id_str.parse::<u32>() {
                    
                    request.respond(Response::from_string("Closest nodes")).unwrap();  // Replace with actual node data
                } else {
                    request.respond(Response::from_string("Invalid ID").with_status_code(400)).unwrap();
                }
            }
            _ => {
                request.respond(Response::from_string("404 Not Found").with_status_code(404)).unwrap();
            }
        }
    }
}

pub async fn start_n_nodes(n: usize) {
    // start the first node in the current thread
    let handle = thread::spawn(|| {
        let rt = Runtime::new().unwrap();
        rt.block_on(start_dht(&Args {
            id: Some(1),
            udp_port: 8080,
            ..Args::default()
        }));
    });

    // spawn additional nodes in separate threads
    let mut handles = vec![handle];
    for i in 1..n {
        let udp_port = 8080 + i as u16;
        let handle = thread::spawn(move || {
            let rt = Runtime::new().unwrap();
            rt.block_on(start_dht(&Args {
                id: None,
                udp_port,
                bootstrap_id: Some(1),
                bootstrap_ip: Some("127.0.0.1".to_owned()),
                bootstrap_port: Some(8080),
            }));
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }
}
