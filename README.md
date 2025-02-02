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

## Installation

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

-   `bootstrap_ip`: IP of the bootstrap server (use `localhost` if starting a new network).
-   `output_dir`: Directory where downloaded video files are stored.
-   `private_dht`: Enable this if you want to restrict the network to private use.

3. Forward necessary ports on your router:

-   BitTorrent (default: 6881)
-   DHT (default: 8999)
-   ElasticSearch (default: 9200) if acting as a bootstrap node.

4. Start the required services:

```
dream-dht &
dream-torrent
```

### Quick Start

To compile `dream`, all you have to do is clone the source code and run `cargo build`:

```sh
git clone https://github.com/kamui-fin/dream.git
cd dream
cargo build --release

cp target/release/dream-dht /usr/local/bin
cp target/release/dream-torrent /usr/local/bin
cp target/release/dream-cli /usr/local/bin
```

Next up, we need to configure some important settings. Copy over the sample config file:

```sh
mkdir ~/.config/dream
cp config.toml ~/.config/dream
```

Here is a list of the required variables to modify:

1. bootstrap_ip: IP of the bootstrap server if you're joining a dream network. If not passed in, this is set to localhost, indicating that you are the bootstrap node for a new network.
2. output_dir: The directory where video files get stored.
3. private_dht: If you simply want to keep the dht within a private network, you can enable this. It all depends on how you wish to use dream.

There are numerous other configurable parameters for dream within `config.toml`.

Next up, you'll need to forward the bittorrent (default 6881) and dht (default 8999) ports on your router. If you are planning on being a bootstrap node, you must also port forward elastic-search (default 9200). This allows peers to connect to you without getting restricted by the NAT.

The last step is to run the two necessary components:

```
dream-dht &
dream-torrent
```

## Usage

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

A bootstrap node serves as the entry point to a Dream network by providing peers with initial connection information. In addition to this role, bootstrap nodes currently maintain an index of all available videos for search functionality (e.g., fuzzy search). Future updates aim to decentralize this indexing process for greater scalability.

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
