use std::collections::LinkedList;

use num_bigint::{BigUint, RandBigInt};
use serde::Serialize;

use crate::{config::CONFIG, dht::node::Node};

use super::key::{Key, ID_SIZE};

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
    node_id: Key,
    // array of linked lists with NUM_BITS elements
    pub buckets: Vec<LinkedList<Node>>,
}

fn leading_zeros(node_id: Key) -> usize {
    let mut count = 0;
    for byte in node_id.0.iter() {
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
    pub fn new(node_id: Key) -> Self {
        Self {
            node_id,
            buckets: vec![LinkedList::new(); ID_SIZE * 8],
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

    pub fn find_bucket_idx(&self, node_id: Key) -> usize {
        let xor_result = node_id.distance(&self.node_id);
        leading_zeros(xor_result)
    }

    pub fn node_in_bucket(&self, bucket_idx: usize, node_id: Key) -> Option<&Node> {
        self.buckets[bucket_idx]
            .iter()
            .find(|&node| (node.id == node_id))
    }

    pub fn remove_node_in_bucket(&mut self, node_id: Key, bucket_idx: usize) {
        let mut new_list: LinkedList<Node> = LinkedList::new();

        // TODO: fix if removing node_id = our_node_id
        while let Some(curr_front) = self.buckets[bucket_idx].pop_front() {
            if (curr_front).id != node_id {
                new_list.push_back(curr_front);
                self.buckets[bucket_idx].pop_front();
            }
        }

        self.buckets[bucket_idx] = new_list;
    }

    pub fn remove_node(&mut self, node_id: Key) {
        let bucket_idx = self.find_bucket_idx(node_id);
        self.remove_node_in_bucket(node_id, bucket_idx);
    }

    pub fn upsert_node(&mut self, node: Node) -> bool {
        let bucket_idx = self.find_bucket_idx(node.id);
        let already_exists = self.node_in_bucket(bucket_idx, node.id).is_some();
        let is_full = self.buckets[bucket_idx].len() >= CONFIG.dht.k_bucket_size;
        // info!("Attempting to add node {} to routing table to bucket {bucket_idx}. Already exists? {already_exists}", node.id);

        if already_exists {
            self.remove_node_in_bucket(node.id, bucket_idx);
            self.buckets[bucket_idx].push_back(node);

            return false; // since it already exists, eviction not necessary
        } else if !is_full {
            self.buckets[bucket_idx].push_back(node);

            return false; // since the bucket isn't full, eviction not necessary
        }

        // eviction check is necessary since bucket is full and node doesn't already exist
        true
    }

    pub fn get_refresh_target(&self, bucket_idx: usize) -> Key {
        let start = BigUint::from(1u8) << (ID_SIZE * 8 - bucket_idx - 1);
        let end = BigUint::from(1u8) << ((ID_SIZE * 8 - bucket_idx) as u32);

        let mut rng = rand::thread_rng();

        let node_id = rng.gen_biguint_range(&start, &end);
        let node_id_bytes = node_id.to_bytes_be();

        let num_bytes = node_id_bytes.len().min(20);
        let mut res_node_id = [0; 20];
        res_node_id[20 - num_bytes..]
            .copy_from_slice(&node_id_bytes[node_id_bytes.len() - num_bytes..]);

        res_node_id.into()
    }

    pub fn get_nodes(&self, target_node_id: Key) -> Vec<Node> {
        let mut nodes = self.get_all_nodes();
        nodes.sort_by_key(|node| node.id.distance(&target_node_id));

        if let Some(first_match) = nodes.first() {
            if first_match.id == target_node_id {
                return vec![first_match.clone()];
            }
        }

        nodes
    }
}
