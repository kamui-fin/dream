fn main() {
    // get hex from .torrent file info hash

    let path = "archlinux.torrent";
    let meta_file =
        dream::metafile::Metafile::parse_torrent_file(std::path::PathBuf::from(path)).unwrap();

    let info_hash = meta_file.get_info_hash();
    let hex = hex::encode(info_hash);
    println!("{}", hex);
}
