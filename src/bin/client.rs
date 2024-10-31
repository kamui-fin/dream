use bendy::decoding::{FromBencode };
use bendy::encoding::ToBencode;
use tokio::net::{UdpSocket};
use std::collections::HashMap;
use std::env;
use std::hash::Hash;

#[tokio::main]
async fn main(){
    let mut bencode_dict: HashMap<&str, &str> = HashMap::new();
    bencode_dict.insert("q", "ping");
    bencode_dict.insert("id", "client");
    let bencode_dict = bencode_dict.to_bencode().unwrap();

    let socket = UdpSocket::bind(format!("127.0.0.1:8081")).await.unwrap();
    socket.send_to(&bencode_dict, "127.0.0.1:8080").await.unwrap();

    let mut buf = [0; 2048];
    let (amt, src) = socket.recv_from(&mut buf).await.unwrap();
    
    let buf = HashMap::<String, String>::from_bencode(&buf);
    println!("{:#?}", buf);
}
