use crate::{config::Args, kademlia::Kademlia};
use futures::executor::block_on;
use std::{sync::Arc, thread};
use tokio::runtime::{Handle, Runtime};

pub async fn start_dht(args: &Args) {
    let kademlia = Arc::new(Kademlia::init(args).await);
    kademlia.start_server(args.get_bootstrap()).await;
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
