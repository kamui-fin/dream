use std::{
    collections::{BinaryHeap, HashMap, HashSet},
    sync::{Arc, Mutex},
};

use tokio::{net::UdpSocket, task::JoinSet};
use crate::{
    config::{ALPHA, K, NUM_BITS},
    context::RuntimeContext,
    krpc::{Krpc},
    node::{Node, NodeDistance},
};

pub async fn join_dht_network(context: &RuntimeContext, bootstrap_node: Option<Node>, krpc: &Arc<Krpc>) {
    if bootstrap_node.is_none() {
        return;
    }
    // 1. initialize k-bucket with another known node
    context.routing_table.lock().unwrap().upsert_node(bootstrap_node.unwrap());

    // 2. run find_nodes on itself to fill k-bucket table
    let k_closest_nodes = recursive_find_nodes(context.node.id, &context, krpc).await; // assuming sorted by distance

    // 3. refresh buckets past closest node bucket
    let closest_idx = context.routing_table
        .lock()
        .unwrap()
        .find_bucket_idx(k_closest_nodes[0].node.id);

    for idx in (closest_idx + 1)..(NUM_BITS as u32) {
        refresh_bucket(idx as usize, context, &krpc).await;
    }
}

// fyi: refresh periodically too besides only when joining
// if no node lookup for bucket range has been done within 1hr
async fn refresh_bucket(
    bucket_idx: usize,
    context: &RuntimeContext,
    krpc: &Arc<Krpc>,
) {
    let node_id = context.routing_table.lock().unwrap().get_refresh_target(bucket_idx);
    recursive_find_nodes(node_id, context, krpc).await;
}

pub async fn recursive_find_nodes(
    target_node_id: u32,
    context: &RuntimeContext,
    krpc: &Arc<Krpc>
) -> Vec<NodeDistance> {
    // TODO: nodes that fail to respond quickly are removed from consideration until and unless they do respond.
    let mut set = JoinSet::new();

    let mut already_queried = HashSet::new();

    // pick α nodes from closest non-empty k-bucket, even if less than α entries
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
            let max_heap = Arc::clone(&max_heap);
            let krpc = krpc.clone();
            // updated k closest
            already_queried.insert(distance_node.node.id);
            set.spawn(async move {
                let k_closest_nodes = krpc.send_find_node(&distance_node.node).await;
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
    }
    current_closest
}