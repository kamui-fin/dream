use clap::{Arg, Args, Command, Parser, Subcommand};
use hex::encode;
use log::{info, warn};
use rand::Rng;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{error::Error, fs, io::{self, Read, Write}, path::{Path, PathBuf}};
use dream::{config::{ip, piece_size, Cli, Commands}, metafile::Metafile};

// struct for storing the data
#[derive(Serialize, Deserialize, Debug)]
struct VideoRecord {
    infohash: [u8; 20],
    title: String,
    node_id: String,
    meta_file_bytes: String
}

// append to log basically
async fn add_record(client: &Client, record: VideoRecord) -> Result<(), Box<dyn Error>> {
    // TODO: if we want to have multiple indices and not just movies, we could dynamically configure that
    let url = format!("http://{}/videos/_doc", ip);
    let response = client
        .post(&url)
        .json(&record)
        .send()
        .await?;

    if response.status().is_success() {
        println!("Record added successfully: {:?}", record);
    } else {
        println!("Failed to add record: {:?}", response.text().await?);
    }

    Ok(())
}

// fuzzy serch on movie
async fn fuzzy_search_movie_title(client: &Client, query: &str) -> Result<Vec<VideoRecord>, Box<dyn Error>> {
    let url = format!("http://{}/videos/_search", ip);
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

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await?;

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

async fn upload_torrent(client: &Client, meta_file: &Metafile, title: &str){
    let meta_file_bytes = encode(&meta_file.info.pieces);
    let infohash = meta_file.get_info_hash();
    
    let path = "node_id.bin";

    let node_id = if Path::new(path).exists() {
        let mut file = fs::File::open(path).expect("Unable to open file");
        let mut id:[u8; 20] = [0u8; 20];
        file.read_exact(&mut id).expect("Unable to read data");
        encode(id)
    } else {
        let mut rng = rand::thread_rng();
        let mut id = [0u8; 20];
        rng.fill(&mut id);
        let mut file = fs::File::create(path).expect("Unable to create file");
        file.write_all(&id).expect("Unable to write data");
        encode(id)
    };

    let new_record = VideoRecord{
        infohash,
        meta_file_bytes,
        title: String::from(title),
        node_id
    };

    add_record(client, new_record).await.unwrap();
}

async fn search(client: &Client, query: &str){
    let all_matches = fuzzy_search_movie_title(client, query).await.unwrap();
    info!("Query results for query {:#?} are {:#?}", query, all_matches);
    info!("Now playing the closest match...");
    start_stream(&all_matches[0].infohash);
}

fn start_stream(info_hash: &[u8;20]){
    let stream_link = format!("http://localhost:3000/{}", encode(info_hash));

    let _mpv = std::process::Command::new("mpv")
        .arg(stream_link)
        .spawn()
        .expect("Failed to start MPV");
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    let cli = Cli::parse();
    let client = Client::new();
    
    match &cli.command {
        Commands::Upload { path , title} => {
            let file_path = Path::new(path);

            match file_path.extension(){
                Some(extension) => {
                    if extension == "torrent"{
                        let meta_file = Metafile::parse_torrent_file(file_path.to_path_buf()).unwrap();
                        upload_torrent(&client, &meta_file, title).await;
                    } else if extension == "mp4"{
                        let meta_file = Metafile::from_video(&Path::new(file_path), piece_size, None);
                        upload_torrent(&client, &meta_file, title).await;
                    } else {
                        warn!("Invalid arguments provided for command!");
                    }
                },
                None => {}
            }
        }
        Commands::Search { query } => {
            search(&client, query).await;
        }
    }
}