// infohash of torrent = key id
// starts off with one bucket with ID space range 0 - 2^160
// when bucket full of known good nodes, no more nodes may be added unless our own ID falls within the range of the bucket
// -> in that case, bucket is replaced by 2 new buckets each with half the range of the old bucket
// -> and nodes from old bucket and distributed among two new ones
// for new table with 1 bucket, the full bucket is always split into two new buckets covering ranges 0..2^159 and 2^159..2^160
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
struct RoutingTable {
    my_node: Node,
    secret: Arc<Mutex<[u8; 16]>>,
    // array of linked lists with NUM_BITS elements
    buckets: Vec<LinkedList<Node>>,
}

impl RoutingTable {
    fn new(my_node: Node, secret: Arc<Mutex<[u8; 16]>>) -> Self {
        Self {
            my_node,
            secret,
            buckets: Vec::with_capacity(NUM_BITS),
        }
    }

    fn get_all_nodes(&self) -> Vec<Node> {
        let mut all_nodes = vec![];
        for bucket in self.buckets.iter() {
            for node in bucket.iter() {
                all_nodes.push(node.clone());
            }
        }
        all_nodes
    }

    fn find_bucket_idx(&self, node_id: u32) -> u32 {
        let xor_result = node_id ^ self.my_node.id;
        xor_result.leading_zeros() - ((32 - NUM_BITS) as u32)
    }

    fn node_in_bucket(&self, bucket_idx: usize, node_id: u32) -> Option<&Node> {
        self.buckets[bucket_idx]
            .iter()
            .find(|&node| (node.id == node_id))
    }

    fn remove_node(&mut self, node_id: u32, bucket_idx: usize) {
        let mut new_list: LinkedList<Node> = LinkedList::new();

        while let Some(curr_front) = self.buckets[bucket_idx].pop_front() {
            if (curr_front).id != node_id {
                new_list.push_back(curr_front);
                self.buckets[bucket_idx].pop_front();
            }
        }

        self.buckets[bucket_idx] = new_list;
    }

    fn upsert_node(&mut self, node: Node) {
        let bucket_idx = self.find_bucket_idx(node.id) as usize;
        let already_exists = self.node_in_bucket(bucket_idx, node.id).is_none();
        let is_full = self.buckets[bucket_idx].len() >= K;

        if already_exists && !is_full {
            self.remove_node(node.id, bucket_idx);
            self.buckets[bucket_idx].push_back(node);
        } else if already_exists {
            // ping front of list and go from there
            // if ping
        } else {
            self.buckets[bucket_idx].push_back(node);
        }
    }

    fn get_refresh_target(&self, bucket_idx: usize) -> u32 {
        let start = 2u32.pow((NUM_BITS - bucket_idx - 1) as u32);
        let end = 2u32.pow((NUM_BITS - bucket_idx) as u32);

        let mut rng = rand::thread_rng();

        rng.gen_range(start..end)
    }
}
