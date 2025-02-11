use anyhow::Context;
use clap::Parser;
use dream::{
    config::{Cli, Commands, CONFIG},
    dht::key::{get_node_id_path, read_node_id},
    metafile::Metafile,
    stream::VideoRecord,
    utils::init_logger_debug,
};
use hex::encode;
use log::{error, info};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs,
    io::{self, Write},
    path::Path,
};

// append to log basically
async fn add_record(client: &Client, record: VideoRecord) -> Result<(), Box<dyn Error>> {
    // if we want to have multiple indices and not just videos, we could dynamically configure that
    let url = format!(
        "http://{}:{}/videos/_doc",
        CONFIG.network.elastic_search_ip, CONFIG.network.elastic_search_port
    );
    let response = client.post(&url).json(&record).send().await?;

    if response.status().is_success() {
        println!("Record added successfully: {:?}", record);
    } else {
        println!("Failed to add record: {:?}", response.text().await?);
    }

    Ok(())
}

async fn fuzzy_search_video_title(
    client: &Client,
    query: &str,
) -> anyhow::Result<Vec<VideoRecord>, Box<dyn Error>> {
    let url = format!(
        "http://{}:{}/videos/_search",
        CONFIG.network.elastic_search_ip, CONFIG.network.elastic_search_port
    );
    let body = serde_json::json!({
        "query": {
            "fuzzy": {
                "title": {
                    "value": query,
                    "fuzziness": "2"
                }
            }
        }
    });

    let response = client.post(&url).json(&body).send().await?;

    if response.status().is_success() {
        let json: serde_json::Value = response.json().await?;
        let hits = json["hits"]["hits"].as_array().unwrap();
        let records: Vec<VideoRecord> = hits
            .iter()
            .map(|hit| serde_json::from_value(hit["_source"].clone()).unwrap())
            .collect();

        Ok(records)
    } else {
        println!("Failed to perform search: {:?}", response.text().await?);
        Ok(Vec::new())
    }
}

async fn announce_to_dht(infohash: String) {
    if CONFIG.dht.enabled == false {
        panic!("DHT is not enabled");
    }
    let url = format!(
        "http://localhost:{}/announce/{}",
        CONFIG.network.dht_port + 1000,
        infohash
    );
    let client = Client::new();
    let response = client.post(url).send().await.unwrap();

    if response.status().is_success() {
        info!("Announced to DHT successfully: {:?}", infohash);
    } else {
        error!(
            "Failed to announce to DHT: {:?}",
            response.text().await.unwrap()
        );
    }
}

async fn upload_torrent(
    client: &Client,
    file_path: Option<&Path>,
    meta_file: &Metafile,
    title: &str,
) {
    let meta_file_bytes = encode(serde_bencode::to_bytes(meta_file).unwrap());
    let infohash = encode(meta_file.get_info_hash());
    let node_id = encode(read_node_id(get_node_id_path()));

    if let Some(file_path) = file_path {
        // move the video file to output/info_hash
        let output_dir = Path::new(&CONFIG.general.output_dir).join(Path::new(&infohash));
        if !output_dir.exists() {
            fs::create_dir_all(&output_dir).unwrap();
        }
        fs::rename(file_path, output_dir.join(file_path.file_name().unwrap())).unwrap();

        // also means we need to announce to the DHT
        announce_to_dht(infohash.clone()).await;
    }
    let new_record = VideoRecord {
        infohash,
        meta_file_bytes,
        title: String::from(title),
        node_id,
    };

    add_record(client, new_record).await.unwrap();
}

fn ask_and_play(all_matches: &Vec<VideoRecord>) {
    let mut input = String::new();

    io::stdin()
        .read_line(&mut input)
        .expect("Invalid response, please try search again.");

    println!("Now playing result number {:}", input);

    let match_idx = input.trim().parse::<usize>().expect("Not a valid number");
    if match_idx >= all_matches.len() {
        info!("Invalid index");
        return;
    }
    start_stream(&all_matches[match_idx].infohash).unwrap();
}

async fn search(client: &Client, query: &str) {
    let all_matches = fuzzy_search_video_title(client, query).await.unwrap();

    println!("Found {} matches", all_matches.len());
    for (idx, record) in all_matches.iter().enumerate() {
        println!("  [{}]: {}", idx, record.title);
    }
    print!("What file to stream [0..{}]: ", all_matches.len() - 1);
    io::stdout().flush().unwrap();
    ask_and_play(&all_matches);
}

fn start_stream(info_hash: &str) -> anyhow::Result<()> {
    let stream_link = format!(
        "http://localhost:{}/{}",
        CONFIG.network.stream_server_port, info_hash
    );

    let _mpv = std::process::Command::new(&CONFIG.stream.video_player)
        .arg(stream_link.clone())
        .spawn()
        .with_context(|| format!("Failed to start MPV with stream: {}", stream_link))?;

    Ok(())
}

#[tokio::main]
async fn main() {
    init_logger_debug();

    let cli = Cli::parse();
    let client = Client::new();

    match &cli.command {
        Commands::Upload { path, title } => {
            let file_path = Path::new(path);

            if let Some(extension) = file_path.extension() {
                if extension == "torrent" {
                    let meta_file = Metafile::parse_torrent_file(file_path.to_path_buf()).unwrap();
                    upload_torrent(&client, None, &meta_file, title).await;
                } else {
                    let meta_file =
                        Metafile::from_video(Path::new(file_path), CONFIG.torrent.piece_size, None);
                    upload_torrent(&client, Some(file_path), &meta_file, title).await;
                }
            }
        }
        Commands::Stream { query } => {
            search(&client, query).await;
        }
    }
}
