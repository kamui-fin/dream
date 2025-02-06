# Dream

[![License](https://img.shields.io/badge/license-GPLv3-blue)](LICENSE)

Dream is a peer-to-peer streaming platform powered by BitTorrent and a decentralized tracker with Kademlia DHT.

## **Why Dream?**

The current video streaming ecosystem is dominated by centralized platforms. While convenient, these systems come with significant drawbacks:

-   **Bottlenecks**: Centralized servers can struggle under high demand.
-   **Censorship**: Content can be removed or restricted due to external pressures.
-   **High Costs**: Maintaining large-scale server infrastructure is expensive.

Dream addresses these challenges by decentralizing video streaming. By leveraging the efficiency of BitTorrent and the robustness of Kademlia DHT, Dream empowers users to share and stream content directly without intermediaries. The result? A more open, democratic, and scalable streaming solution.

## **Core Features**

-   **Decentralized Tracker**: Uses Kademlia DHT for peer discovery, eliminating the need for central servers.
-   **Efficient Streaming**: Streams video directly from peers using the BitTorrent protocol for seamless playback.
-   **On-Demand HTTP Streaming**: Fully compatible with any video player that supports HTTP.
-   **Private Networks**: Enables private deployments for home or institutional use.
-   **Simple CLI Interface**: `dream-cli` provides an intuitive command-line tool for browsing, uploading, and streaming videos.

## Getting Started

Here are a couple prerequisites to get setup before moving on:

-   [ElasticSearch](https://www.elastic.co/guide/en/elasticsearch/reference/current/setup.html) server running on port 9200
-   [Rust](https://rustup.rs/) toolchain
-   A video player like [mpv](https://mpv.io/)

### **Installation**

Follow these steps to set up Dream:

1. Clone the repository and build the project:

```
git clone https://github.com/kamui-fin/dream.git
cd dream
cargo build --release cp target/release/dream-dht /usr/local/bin
cp target/release/dream-torrent /usr/local/bin
cp target/release/dream-cli /usr/local/bin
```

2. Configure settings:

```ck
mkdir ~/.config/dream
cp config.toml ~/.config/dream
```

Update the following variables in `~/.config/dream/config.toml`:

-   `network/elastic_search_ip`: IP of the bootstrap server (leave as `127.0.0.1` if starting a new network).
-   `general/output_dir`: Directory where downloaded video files are stored.
-   `dht/enabled`: If you are willing to port forward (allowing you to use DHT), enable this. 

3. (optional, but recommended) Forward necessary ports on your router:

-   BitTorrent and DHT (default: 6881)
-   ElasticSearch (default: 9200) if acting as a bootstrap node.

4. Start the required services:

```
dream-dht &
dream-torrent
```

### Quick Start

The architecture of dream is relatively simple. There are mainly three services that should be ran 24/7:

1. Kademlia DHT (`dream-dht`) - This functions as the decentralized BitTorrent tracker, allowing the BT engine to fetch peers associated with a video to download and upload. It essentially manages the p2p network by itself.
2. BitTorrent engine (`dream-torrent`) - Similar to `transmission`, just another BitTorrent client. Essentially ensures that only the necessary data is downloaded from peers.
3. HTTP Stream server (part of `dream-torrent`) It gets video data from the BT engine on-demand, and supplies it to any video player supporting HTTP.

### Command-Line Interface (dream-cli)

The CLI simplifies interaction with Dream's network. Examples:

-   Upload a video file:

```
dream-cli upload video.mp4 "title"
```

-   Upload an external torrent file:

```
dream-cli upload video.torrent "title"
```

-   Stream a video by searching its title:

```
dream-cli stream "query"
```

### Bootstrap Node

A bootstrap node serves as the entry point to a Dream network. Bootstrap nodes currently maintain an index of all available videos for search functionality (e.g., fuzzy search). Future updates aim to decentralize this indexing process for greater scalability. 

### Port Forwarding

This step is important if you're trying to upload videos to the network. It allows you to run the DHT, which distributes important information about peers to download your file. 

If you simply wish to use Dream with existing torrents, you can skip this step. 

## Contributing

By no means did we include fully featured implementations of the protocols, so feel free to open a PR and contribute! We're also open to any bug reports or feature ideas.

There's a lot of potential and work to be done in the p2p space, so we're excited to discover more!

## Roadmap

Here’s what’s next for Dream:

1.  Optimize speed through caching and dynamic load balancing
2.  Modularize codebase for easier integration into other projects
3.  Improve error handling for better debugging experiences
4.  Add support for UDP trackers
5.  Reduce required port forwarding to a single port
6.  Decentralize the video index for improved scalability

## License

Distributed under the GPLv3 License. See `LICENSE` for more information.
