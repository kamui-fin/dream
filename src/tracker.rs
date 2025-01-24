use anyhow::Result;
use http_req::request;
use serde::Deserialize;
use url::{form_urlencoded, Url};

use crate::{
    metafile::Metafile,
    peer::{
        session::{deserialize_peers, ConnectionInfo},
        DREAM_ID,
    },
    PORT,
};
#[derive(Debug)]
pub enum Event {
    Started,
    Completed,
    Stopped,
    Empty,
}

#[derive(Debug)]
pub struct TrackerRequest {
    pub info_hash: String,
    pub uploaded: usize,
    pub downloaded: usize,
    pub left: usize,
    pub event: Option<Event>,
}

impl TrackerRequest {
    pub fn new(torrent_file: &Metafile) -> Self {
        let left = torrent_file.info.length.unwrap_or_default() as usize;

        Self {
            info_hash: form_urlencoded::byte_serialize(&torrent_file.get_info_hash()).collect(),
            uploaded: 0,
            downloaded: 0,
            left,
            event: None,
        }
    }

    pub fn with_event(self, event: Event) -> Self {
        Self {
            event: Some(event),
            ..self
        }
    }

    pub fn to_url(&self, tracker_url: &str) -> String {
        let port_ascii = PORT.to_string();
        let uploaded_ascii = self.uploaded.to_string();
        let downloaded_ascii = self.downloaded.to_string();
        let left_ascii = self.left.to_string();

        let mut url = Url::parse(tracker_url).expect("Invalid tracker URL");

        url.query_pairs_mut()
            .append_pair("peer_id", &DREAM_ID)
            .append_pair("port", &port_ascii)
            .append_pair("uploaded", &uploaded_ascii)
            .append_pair("downloaded", &downloaded_ascii)
            .append_pair("left", &left_ascii)
            .append_pair("compact", "1");

        if let Some(event) = &self.event {
            let event_repr = match event {
                Event::Started => "started",
                Event::Completed => "completed",
                Event::Empty => "empty",
                Event::Stopped => "stopped",
            };
            url.query_pairs_mut().append_pair("event", event_repr);
        }

        format!("{}&info_hash={}", url, self.info_hash)
    }
}

#[derive(Deserialize, Debug)]
pub struct TrackerResponse {
    pub interval: u64,
    #[serde(deserialize_with = "deserialize_peers")]
    pub peers: Vec<ConnectionInfo>,
}

pub fn get_peers_from_tracker(tracker_url: &str, body: TrackerRequest) -> Result<TrackerResponse> {
    let get_url = body.to_url(tracker_url);

    let mut body = Vec::new();
    let _ = request::get(get_url, &mut body)?;
    let res: TrackerResponse = serde_bencoded::from_bytes(&body)?;
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dht_tracker() {
        let torrent = Metafile::parse_torrent_file("archlinux.torrent").unwrap();
        let info_hash = torrent.get_info_hash();
        println!("{}", hex::encode(&info_hash));
    }
}
