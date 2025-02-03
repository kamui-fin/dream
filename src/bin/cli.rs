use anyhow::Context;
use clap::Parser;
use config::Config;
use dream::{
    config::{Cli, Commands, CONFIG}, dht::key::read_node_id, metafile::Metafile, utils::init_logger
};
use hex::encode;
use log::{info, warn};
use rand::Rng;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs,
    io::{self, Read, Write},
    path::Path,
};

#[derive(Serialize, Deserialize, Debug)]
struct VideoRecord {
    infohash: [u8; 20],
    title: String,
    node_id: String,
    meta_file_bytes: String,
}

// append to log basically
async fn add_record(client: &Client, record: VideoRecord) -> Result<(), Box<dyn Error>> {
    // if we want to have multiple indices and not just videos, we could dynamically configure that
    let url = format!(
        "http://{}:{}/videos/_doc",
        CONFIG.network.bootstrap_ip, CONFIG.network.elastic_search_port
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
        "http://{}/videos/_search",
        CONFIG.network.elastic_search_port
    );
    let body = serde_json::json!({
        "query": {
            "fuzzy": {
                "title": {
                    "value": query,
                    "fuzziness": "AUTO"
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

async fn upload_torrent(client: &Client, meta_file: &Metafile, title: &str) {
    let meta_file_bytes = encode(&meta_file.info.pieces);
    let infohash = meta_file.get_info_hash();

    // TODO: we need to use proper directories to store dream-related files
    let path = "node_id.bin";

    let node_id =  encode(read_node_id(path));

    let new_record = VideoRecord {
        infohash,
        meta_file_bytes,
        title: String::from(title),
        node_id,
    };

    add_record(client, new_record).await.unwrap();
}

fn ask_and_play(all_matches: &Vec<VideoRecord>){

    let mut input = String::new();

    println!("Please enter which result to play...");

    io::stdin().read_line(&mut input).expect("Invalid response, please try search again.");
    println!("Now playing result number {:}", input);

    let match_idx = input.parse::<usize>().expect("Not a valid number");
    if match_idx >= all_matches.len(){
        info!("Invalid index, try again later.");
        return
    }

    println!("Now playing selected match index...");
    start_stream(&all_matches[match_idx].infohash).unwrap();
}

async fn search(client: &Client, query: &str) {
    let all_matches = fuzzy_search_video_title(client, query).await.unwrap();
    info!(
        "Query results for query {:#?} are {:#?}",
        query, all_matches
    );
    
    ask_and_play(&all_matches);
}

fn start_stream(info_hash: &[u8; 20]) -> anyhow::Result<()>{

    let stream_link = format!(
        "http://localhost:{}/{}",
        CONFIG.network.stream_server_port,
        encode(info_hash)
    );

    let _mpv = std::process::Command::new("mpv")
        .arg(stream_link.clone())
        .spawn()
        .with_context(|| format!("Failed to start MPV with stream: {}", stream_link))?;

    Ok(())
}

#[tokio::main]
async fn main() {
    init_logger();

    let cli = Cli::parse();
    let client = Client::new();

    match &cli.command {
        Commands::Upload { path, title } => {
            let file_path = Path::new(path);

            if let Some(extension) = file_path.extension() {
                if extension == "torrent" {
                    let meta_file = Metafile::parse_torrent_file(file_path.to_path_buf()).unwrap();
                    upload_torrent(&client, &meta_file, title).await;
                } else if extension == "mp4" {
                    let meta_file =
                        Metafile::from_video(Path::new(file_path), CONFIG.torrent.piece_size, None);
                    upload_torrent(&client, &meta_file, title).await;
                } else {
                    warn!("Invalid arguments provided for command!");
                }
            }
        }
        Commands::Stream { query } => {
            search(&client, query).await;
        }
    }
}
