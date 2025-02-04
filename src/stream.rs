/*

Launch HTTP streaming server supporting byte-range requests
Find appropriate piece(s) to download and put in front of queue
Wait for piece(s) to be downloaded
Supported range = file length
Get mimetype from video type
Send response with appropriate headers


Should all be in another tokio task
Communicate with engine over mpsc

*/

use futures::{StreamExt, TryStreamExt};
use hyper::body::{Body, Frame};

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::Result;
use hyper::{header, server::conn::http1, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use log::info;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use crate::config::CONFIG;
use crate::msg::ServerMsg;

static NOTFOUND: &[u8] = b"Not Found";

pub async fn start_server(
    engine_tx: Arc<mpsc::Sender<ServerMsg>>,
    output_dir: Arc<PathBuf>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = ([127, 0, 0, 1], CONFIG.network.stream_server_port).into();

    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);

    loop {
        let (tcp, _) = listener.accept().await?;

        let io = TokioIo::new(tcp);

        let engine_tx = engine_tx.clone();
        let output_dir = output_dir.clone();

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(|req: Request<hyper::body::Incoming>| {
                        video_handler(req, engine_tx.clone(), output_dir.clone())
                    }),
                )
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}

/// HTTP status code 404
fn not_found() -> Response<BoxBody<Bytes, std::io::Error>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(NOTFOUND.into()).map_err(|e| match e {}).boxed())
        .unwrap()
}

async fn handle_stream_request(
    engine_tx: Arc<mpsc::Sender<ServerMsg>>,
    start: u64,
    end: u64,
    info_hash: [u8; 20],
) -> std::result::Result<BoxBody<Bytes, std::io::Error>, Box<dyn std::error::Error>> {
    // consume engine_tx of ReadyData until has_more is false
    let (stream_tx, stream_rx) = mpsc::channel(2000);

    engine_tx
        .send(ServerMsg::StreamRequestRange {
            start,
            end,
            info_hash,
            response_tx: stream_tx,
        })
        .await?;

    let stream_body = ReceiverStream::new(stream_rx).map(|data_ready| {
        if data_ready.has_more {
            println!("[STREAM] got sum data lol");
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(data_ready.data))
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "No more data",
            ))
        }
    });

    let stream = StreamBody::new(stream_body.map_ok(Frame::data));

    Ok(BodyExt::boxed(stream))
}

async fn get_meta_file_bytes(info_hash: &str) -> anyhow::Result<Vec<u8>> {
    let url = format!(
        "http://{}:{}/videos/_search",
        CONFIG.network.elastic_search_ip, CONFIG.network.elastic_search_port
    );

    let query = serde_json::json!({
        "query": {
            "term": {
                "infohash": info_hash
            }
        }
    });

    let client = reqwest::Client::new();
    let response = client.post(&url).json(&query).send().await.unwrap();

    if response.status().is_success() {
        let json: serde_json::Value = response.json().await.unwrap();
        let hit = json["hits"]["hits"][0].as_object().unwrap();
        let meta_file_bytes = hit["_source"]["meta_file_bytes"].as_str().unwrap();
        let meta_file_bytes = hex::decode(meta_file_bytes).unwrap();

        Ok(meta_file_bytes)
    } else {
        Err(anyhow::anyhow!("Failed to get meta file bytes"))
    }
}

async fn video_handler(
    req: Request<impl Body>,
    engine_tx: Arc<mpsc::Sender<ServerMsg>>,
    output_dir: Arc<PathBuf>,
) -> Result<Response<BoxBody<Bytes, std::io::Error>>> {
    // Extract the `info_hash` from the path
    let path = req.uri().path().trim_start_matches('/');

    let info_hash_data = hex::decode(path).unwrap();
    let mut info_hash = [0; 20];
    info_hash.copy_from_slice(&info_hash_data);

    // get torrent file data from elastic search using infohash as primary key
    let meta_file_bytes = get_meta_file_bytes(path).await.unwrap();

    // send ADD_TORRENT to engine
    let (response_tx, response_rx) = oneshot::channel();
    engine_tx
        .send(ServerMsg::AddExternalTorrent {
            input_data: meta_file_bytes,
            output_dir: output_dir.to_path_buf(),
            response_tx,
        })
        .await
        .unwrap();
    // wait for response containing file size
    let file_size = response_rx.await.unwrap();

    let mut start = 0;
    let mut end = file_size - 1;

    if let Some(range_header) = req.headers().get(header::RANGE) {
        if let Ok(range_str) = range_header.to_str() {
            if let Some(range) = parse_range(range_str, file_size) {
                start = range.0;
                end = range.1;
            }
        }
    }

    info!(
        "Range: {}-{} <---> {} out of {}",
        start,
        end,
        end - start + 1,
        file_size
    );

    let content_length = end - start + 1;

    // Using the range, determine the necessary piece(s)
    // --> [client] MPSC SEND: StreamRequestRange(start, end, info_hash)
    // Engine pushes the piece(s) to the front of the queue
    // --> [engine] MPSC recv: find pieces encompassing range and move to front
    //     - PROBLEM: multiple clients fighting for different pieces to be downloaded first
    //     - If we can't find pieces in queue, create them and push to front
    // --> [client] MPSC recv: DataReady(start, end)
    //     - engine will keep sending pieces until we reach the end of the range
    //     - of course, we handle

    // consume engine_tx of ReadyData until has_more is false
    let body = handle_stream_request(engine_tx, start, end, info_hash)
        .await
        .unwrap();

    let response = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CONTENT_LENGTH, content_length)
        .header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, end, file_size),
        )
        .body(body)
        .unwrap();

    Ok(response)
}

fn parse_range(range: &str, file_size: u64) -> Option<(u64, u64)> {
    if !range.starts_with("bytes=") {
        return None;
    }

    let range = &range[6..];
    let parts: Vec<&str> = range.split('-').collect();

    if parts.len() != 2 {
        return None;
    }

    let start = parts[0].parse::<u64>().ok()?;
    let end = if parts[1].is_empty() {
        file_size - 1
    } else {
        parts[1].parse::<u64>().ok()?
    };

    if start > end || end >= file_size {
        return None;
    }

    Some((start, end))
}
