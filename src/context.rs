use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpListener,
};

/// Stores and maintains important runtime objects for the DHT
struct RuntimeContext {
    routing_table: Arc<Mutex<RoutingTable>>,
    peer_store: Arc<Mutex<HashMap<String, Vec<Node>>>>,
    node: Node,
    secret: Arc<Mutex<[u8; 16]>>,
}

impl RuntimeContext {
    fn init(args: Args) -> Self {
        let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
        let peer_store = Arc::new(Mutex::new(HashMap::<String, Vec<Node>>::new()));
        let node = Node::new(
            local_ip().unwrap(),
            args.port,
            args.id.unwrap_or_else(|| {
                let mut rng = rand::thread_rng();
                rng.gen_range(0..64)
            }),
        );
        let secret = Arc::new(Mutex::new(gen_secret()));

        Self {
            routing_table,
            peer_store,
            node,
            secret,
        }
    }

    fn token_regeneration(&self) {
        // change secret every 10 min
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(600)).await;
                let mut sec = secret_clone.lock().unwrap();
                *sec = gen_secret();
            }
        });
    }
    // fn
}

/// Interfacing with the DHT from an external client
/// Primarily for testing the network
/// To be implemented as a simple HTTP server supporting:
///     - PING
///     - SRC
///     - GET
///     - PUT
async fn start_testing_interface(port: u16) {
    todo!()
}
