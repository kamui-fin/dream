use std::{
    collections::{BinaryHeap, HashMap, HashSet},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{
    config::{Args, ALPHA, K, NUM_BITS},
    context::RuntimeContext,
    krpc::Krpc,
    node::{Node, NodeDistance},
    utils::gen_secret,
};
use tokio::{net::UdpSocket, task::JoinSet, time::sleep};

type Peer = (IpAddr, u16);

pub async fn start_dht(args: &Args) {
    let context = Arc::new(RuntimeContext::init(args));
    let krpc = Arc::new(Krpc::init(context.clone()).await);

    // 1. enter with a bootstrap contact or init new network
    join_dht_network(&context, args.get_bootstrap(), &krpc).await;

    // 2. start maintenance tasks
    periodic_republish(context.clone(), krpc.clone());
    periodic_token_regeneration(&context);

    // 3. start dht server
    krpc.listen().await;
}

fn periodic_republish(context: Arc<RuntimeContext>, krpc: Arc<Krpc>) {
    let log_clone = context.announce_log.clone();
    // republish k-v pairs every 1 hr
    tokio::spawn(async move {
        let krpc = krpc.clone();
        loop {
            sleep(Duration::from_secs(60 * 60)).await;
            for info_hash in log_clone.iter() {
                send_announce_peer(info_hash.clone(), &context, &krpc).await;
            }
        }
    });
}

fn periodic_token_regeneration(context: &Arc<RuntimeContext>) {
    let secret_clone = context.secret.clone();
    // change secret every 10 min
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(600)).await;
            let mut sec = secret_clone.lock().unwrap();
            *sec = gen_secret();
        }
    });
}

pub async fn join_dht_network(
    context: &RuntimeContext,
    bootstrap_node: Option<Node>,
    krpc: &Arc<Krpc>,
) {
    if bootstrap_node.is_none() {
        return;
    }
    // 1. initialize k-bucket with another known node
    context
        .routing_table
        .lock()
        .unwrap()
        .upsert_node(bootstrap_node.unwrap());

    // 2. run find_nodes on itself to fill k-bucket table
    let k_closest_nodes = recursive_find_nodes(context.node.id, &context, krpc).await; // assuming sorted by distance

    // 3. refresh buckets past closest node bucket
    let closest_idx = context
        .routing_table
        .lock()
        .unwrap()
        .find_bucket_idx(k_closest_nodes[0].node.id);

    for idx in (closest_idx + 1)..(NUM_BITS as u32) {
        refresh_bucket(idx as usize, context, &krpc).await;
    }
}

// fyi: refresh periodically too besides only when joining
// if no node lookup for bucket range has been done within 1hr
async fn refresh_bucket(bucket_idx: usize, context: &RuntimeContext, krpc: &Arc<Krpc>) {
    let node_id = context
        .routing_table
        .lock()
        .unwrap()
        .get_refresh_target(bucket_idx);
    recursive_find_nodes(node_id, context, krpc).await;
}

pub async fn send_announce_peer(info_hash: String, context: &RuntimeContext, krpc: &Arc<Krpc>) {
    let closest_nodes = recursive_find_nodes(info_hash.parse().unwrap(), context, krpc).await;
    for node_dist in closest_nodes {
        let node = node_dist.node.clone();
        let info_hash = info_hash.clone();
        let krpc = krpc.clone();
        tokio::spawn(async move {
            krpc.send_announce_peer(node, info_hash).await;
        });
    }
}

fn select_initial_nodes(target_node_id: u32, context: &RuntimeContext) -> Vec<NodeDistance> {
    let mut alpha_set = vec![];
    let routing_table_clone = context.routing_table.lock().unwrap();
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

    alpha_set
}

fn update_closest_nodes(
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

pub async fn recursive_find_nodes(
    target_node_id: u32,
    context: &RuntimeContext,
    krpc: &Arc<Krpc>,
) -> Vec<NodeDistance> {
    let mut set = JoinSet::new();
    let mut already_queried = HashSet::new();

    // Select initial nodes
    let mut alpha_set = select_initial_nodes(target_node_id, context);
    alpha_set.truncate(ALPHA);

    let max_heap: Arc<Mutex<BinaryHeap<NodeDistance>>> = Arc::new(Mutex::new(BinaryHeap::new()));
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
            let krpc = krpc.clone();

            already_queried.insert(distance_node.node.id);

            set.spawn(async move {
                let k_closest_nodes = krpc
                    .send_find_node(distance_node.node.clone(), target_node_id)
                    .await;
                update_closest_nodes(&max_heap, &within_heap, k_closest_nodes, target_node_id);
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

        alpha_set = new_closest[0..ALPHA].to_vec();
    }

    let max_heap = max_heap.lock().unwrap();
    max_heap.clone().into_sorted_vec()
}

// duplication from find_node, but subtle and important differences
// rather duplicate than make one function do many different things
pub async fn recursive_get_peers(
    info_hash: u32,
    context: &RuntimeContext,
    krpc: &Arc<Krpc>,
) -> Vec<(IpAddr, u16)> {
    // TODO: nodes that fail to respond quickly are removed from consideration until and unless they do respond.
    let mut set = JoinSet::new();
    let mut already_queried = HashSet::new();
    // pick α nodes from closest non-empty k-bucket, even if less than α entries
    let mut alpha_set = select_initial_nodes(info_hash, context);
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
            let max_heap = Arc::clone(&max_heap);
            let krpc = krpc.clone();
            // updated k closest
            already_queried.insert(distance_node.node.id);
            set.spawn(async move {
                let get_peers_res = krpc
                    .send_get_peers(distance_node.node.clone(), info_hash.to_string())
                    .await;
                if let Some(peers) = get_peers_res.peers {
                    return Some(peers);
                }
                let k_closest_nodes = get_peers_res.nodes.unwrap();
                update_closest_nodes(&max_heap, &within_heap, k_closest_nodes, info_hash);
                None
            });
        }
        // JOIN all these tasks
        while let Some(result) = set.join_next().await {
            if let Ok(result) = result {
                if let Some(peers) = result {
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

