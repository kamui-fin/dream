use std::{collections::LinkedList, hash::RandomState};

use log::info;
use num_bigint::{BigUint, RandBigInt};
use rand::Rng;
use serde::Serialize;

use crate::dht::{config::K, node::Node};

use super::utils::{xor_id, NodeId, ID_SIZE};

// if any nodes are **known** to be bad, it gets replaced by new node
//
// if questionable nodes not seen in the last 15 min, least recently seen is pinged
// -> until one fails to respond or all nodes are good
// -> but if fails to respond, try once more before discarding node and replacing with new good node
//
// need a "last changed" property for each bucket to indicate freshness
// -> when node is pinged and responds, when node is added to bucket, when node in a bucket is replaced with another node
//    -> the bucket last changed property should b e refreshed
//    -> by picking random id in the range of the bucket and run find_nodes
//    -> nodes that are able to receive queries from other nodes dno't need to refresh buckets often
//    -> but nodes that can't need to refresh periodically  so good nodes are available when DHT is needed
#[derive(Serialize, Clone)]
pub struct RoutingTable {
    node_id: NodeId,
    // array of linked lists with NUM_BITS elements
    pub buckets: Vec<LinkedList<Node>>,
}

fn leading_zeros(node_id: NodeId) -> usize {
    let mut count = 0;
    for byte in node_id.iter() {
        if *byte == 0 {
            count += 8;
        } else {
            count += byte.leading_zeros() as usize;
            break;
        }
    }
    count
}

impl RoutingTable {
    pub fn new(node_id: NodeId) -> Self {
        Self {
            node_id,
            buckets: vec![LinkedList::new(); ID_SIZE],
        }
    }

    pub fn get_all_nodes(&self) -> Vec<Node> {
        let mut all_nodes = vec![];
        for bucket in self.buckets.iter() {
            for node in bucket.iter() {
                all_nodes.push(node.clone());
            }
        }
        all_nodes
    }

    pub fn find_bucket_idx(&self, node_id: NodeId) -> usize {
        let xor_result = xor_id(&node_id, &self.node_id);
        leading_zeros(xor_result)
    }

    pub fn node_in_bucket(&self, bucket_idx: usize, node_id: NodeId) -> Option<&Node> {
        self.buckets[bucket_idx]
            .iter()
            .find(|&node| (node.id == node_id))
    }

    pub fn remove_node(&mut self, node_id: NodeId, bucket_idx: usize) {
        let mut new_list: LinkedList<Node> = LinkedList::new();

        while let Some(curr_front) = self.buckets[bucket_idx].pop_front() {
            if (curr_front).id != node_id {
                new_list.push_back(curr_front);
                self.buckets[bucket_idx].pop_front();
            }
        }

        self.buckets[bucket_idx] = new_list;
    }

    pub fn upsert_node(&mut self, node: Node) -> bool {
        let bucket_idx = self.find_bucket_idx(node.id);
        let already_exists = self.node_in_bucket(bucket_idx, node.id).is_some();
        let is_full = self.buckets[bucket_idx].len() >= K;
        // info!("Attempting to add node {} to routing table to bucket {bucket_idx}. Already exists? {already_exists}", node.id);

        if already_exists {
            self.remove_node(node.id, bucket_idx);
            self.buckets[bucket_idx].push_back(node);

            return false; // since it already exists, eviction not necessary
        } else if !is_full {
            self.buckets[bucket_idx].push_back(node);

            return false; // since the bucket isn't full, eviction not necessary
        }

        // eviction check is necessary since bucket is full and node doesn't already exist
        true
    }

    // TODO: test
    pub fn get_refresh_target(&self, bucket_idx: usize) -> NodeId {
        let start = BigUint::from(1u8) << (ID_SIZE * 8 - bucket_idx - 1);
        let end = BigUint::from(1u8) << ((ID_SIZE * 8 - bucket_idx) as u32);

        let mut rng = rand::thread_rng();

        let node_id = rng.gen_biguint_range(&start, &end);
        let node_id_bytes = node_id.to_bytes_be();

        let num_bytes = node_id_bytes.len().min(20);
        let mut res_node_id: NodeId = [0; 20];
        res_node_id[20 - num_bytes..]
            .copy_from_slice(&node_id_bytes[node_id_bytes.len() - num_bytes..]);

        res_node_id
    }

    pub fn get_nodes(&self, target_node_id: NodeId) -> Vec<Node> {
        let mut nodes = self.get_all_nodes();
        nodes.sort_by_key(|node| xor_id(&node.id, &target_node_id));
        nodes.truncate(K);

        if let Some(first_match) = nodes.first() {
            if first_match.id == target_node_id {
                return vec![first_match.clone()];
            }
        }

        nodes
    }
}
