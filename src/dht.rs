// Number of bits for our IDs
const NUM_BITS: usize = 6;
// Max number of entries in K-bucket
const K: usize = 4;
// Max concurrent requests
const ALPHA: usize = 3;

async fn join_dht_network(context: RuntimeContext, bootstrap_node: Option<Node>) {
    if bootstrap_node.is_none() {
        return;
    }
    // 1. initialize k-bucket with another known node
    routing_table.lock().unwrap().upsert_node(bootstrap_node);

    // 2. run find_nodes on itself to fill k-bucket table
    let k_closest_nodes = recursive_find_nodes(our_node.id, &routing_table, &socket).await; // assuming sorted by distance

    // 3. refresh buckets past closest node bucket
    let closest_idx = routing_table
        .lock()
        .unwrap()
        .find_bucket_idx(k_closest_nodes[0].node.id);
    for idx in (closest_idx + 1)..(NUM_BITS as u32) {
        refresh_bucket(&routing_table, idx as usize, &socket).await;
    }
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

async fn recursive_find_nodes(
    target_node_id: u32,
    routing_table: &Arc<Mutex<RoutingTable>>,
    socket: &Arc<UdpSocket>,
) -> Vec<NodeDistance> {
    let mut set = JoinSet::new();

    let mut already_queried = HashSet::new();

    // pick α nodes from closest non-empty k-bucket, even if less than α entries
    let mut alpha_set = vec![];

    let routing_table_clone = routing_table.lock().unwrap();
    let mut bucket_idx = routing_table_clone.find_bucket_idx(target_node_id);
    while routing_table_clone.buckets[bucket_idx as usize].is_empty() {
        bucket_idx = (bucket_idx + 1) % routing_table_clone.buckets.len() as u32;
    }
    for node in routing_table_clone.buckets[bucket_idx as usize].iter() {
        alpha_set.push(NodeDistance {
            node: node.clone(),
            dist: node.id ^ target_node_id,
        });
    }

    let mut current_closest = alpha_set.clone();

    alpha_set.truncate(ALPHA);

    let max_heap: Arc<Mutex<BinaryHeap<NodeDistance>>> = Arc::new(Mutex::new(BinaryHeap::new()));
    let within_heap: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));

    for distance_node in alpha_set.iter().cloned() {
        within_heap.lock().unwrap().insert(distance_node.node.id);
        max_heap.lock().unwrap().push(distance_node);
    }

    loop {
        // send parallel find_node to all of em
        for distance_node in alpha_set.iter().cloned() {
            let within_heap = Arc::clone(&within_heap);
            let routing_table = Arc::clone(routing_table);
            let max_heap = Arc::clone(&max_heap);
            let socket = Arc::clone(socket);
            // updated k closest
            already_queried.insert(distance_node.node.id);
            set.spawn(async move {
                let my_id = routing_table.lock().unwrap().my_node.id;
                let k_closest_nodes = send_find_node(&socket, &distance_node.node, my_id).await;
                for node in k_closest_nodes {
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
            });
        }
        // JOIN all these tasks
        while set.join_next().await.is_some() {}

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

        current_closest = new_closest;
        alpha_set = current_closest[0..ALPHA].to_vec();

        // TODO: nodes that fail to respond quickly are removed from consideration until and unless they do respond.
    }

    current_closest
}

async fn send_find_node(socket: &UdpSocket, target_node: &Node, my_id: u32) -> Vec<Node> {
    let mut arguments = HashMap::new();
    arguments.insert("id".into(), my_id.to_string());
    arguments.insert("target".into(), target_node.id.to_string());

    let find_node_query = KrpcRequest {
        t: gen_trans_id(),
        y: "q".into(),
        q: "find_node".into(),
        a: arguments,
    };
    let find_node_query = serde_bencode::to_bytes(&find_node_query).unwrap();
    socket
        .send_to(
            &find_node_query,
            format!("{}:{}", target_node.ip, target_node.port),
        )
        .await
        .unwrap();

    let mut buf = [0; 2048];
    let (amt, src) = socket.recv_from(&mut buf).await.unwrap();

    let response: KrpcSuccessResponse = serde_bencode::from_bytes(&buf[..amt]).unwrap();
    println!("Received {:#?} from {}", response, src);

    let serialized_nodes = response.r.get("nodes");

    deserialize_compact_node(serialized_nodes)
}
