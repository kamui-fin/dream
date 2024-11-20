use log::info;
use serde::Serialize;
use std::collections::LinkedList;

use rand::Rng;

use crate::dht::{
    config::{K, NUM_BITS},
    node::Node,
};

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
    node_id: u32,
    // array of linked lists with NUM_BITS elements
    pub buckets: Vec<LinkedList<Node>>,
}

impl RoutingTable {
    pub fn new(node_id: u32) -> Self {
        Self {
            node_id,
            buckets: vec![LinkedList::new(); NUM_BITS],
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

    pub fn find_bucket_idx(&self, node_id: u32) -> usize {
        let xor_result = node_id ^ self.node_id;
        (xor_result.leading_zeros() - ((32 - NUM_BITS) as u32)) as usize
    }

    pub fn node_in_bucket(&self, bucket_idx: usize, node_id: u32) -> Option<&Node> {
        self.buckets[bucket_idx]
            .iter()
            .find(|&node| (node.id == node_id))
    }

    pub fn remove_node(&mut self, node_id: u32, bucket_idx: usize) {
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
        info!("Adding node {} to routing table to bucket {bucket_idx}. Already exists? {already_exists}", node.id);

        if already_exists && !is_full {
            self.remove_node(node.id, bucket_idx);
            self.buckets[bucket_idx].push_back(node);
        } else if !already_exists && is_full {
            // ping front of list and go from there
            // if ping
        } else {
            self.buckets[bucket_idx].push_back(node);
        }

        false
    }

    pub fn get_refresh_target(&self, bucket_idx: usize) -> u32 {
        let start = 2u32.pow((NUM_BITS - bucket_idx - 1) as u32);
        let end = 2u32.pow((NUM_BITS - bucket_idx) as u32);

        let mut rng = rand::thread_rng();

        rng.gen_range(start..end)
    }

    pub fn get_nodes(&self, target_node_id: u32) -> Vec<Node> {
        let mut nodes = self.get_all_nodes();
        nodes.sort_by_key(|node| node.id ^ target_node_id);
        nodes.truncate(K);

        if let Some(first_match) = nodes.first() {
            if first_match.id == target_node_id {
                return vec![first_match.clone()];
            }
        }

        nodes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bucket_finder() {
        let rt = RoutingTable::new(0);
        assert_eq!(rt.find_bucket_idx(0b001101), 2);
        assert_eq!(rt.find_bucket_idx(0b000001), 5);
    }
}