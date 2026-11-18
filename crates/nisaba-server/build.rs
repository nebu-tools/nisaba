fn main() {
    // The web client is embedded from web/dist-server. A server built before
    // the client still compiles; it answers "web client not built".
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist-server");
    let _ = std::fs::create_dir_all(&dir);
    println!("cargo:rerun-if-changed=../../web/dist-server");
}
