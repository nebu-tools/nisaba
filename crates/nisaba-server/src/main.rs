//! `nisaba --data DIR --listen ADDR`

use std::path::PathBuf;
use std::time::Instant;

use nisaba_server::{http, store::Store, sync::Rooms};

#[tokio::main]
async fn main() {
    let started = Instant::now();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nisaba=info,nisaba_server=info".into()),
        )
        .init();
    let mut data = PathBuf::from("nisaba-data");
    let mut listen = String::from("127.0.0.1:8080");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data" => data = args.next().map(PathBuf::from).unwrap_or(data),
            "--listen" => listen = args.next().unwrap_or(listen),
            other => {
                eprintln!("unknown argument {other}\nusage: nisaba [--data DIR] [--listen ADDR]");
                std::process::exit(2);
            }
        }
    }
    let store = match Store::open(&data) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("cannot open data directory {}: {e}", data.display());
            std::process::exit(1);
        }
    };
    let app = http::App {
        rooms: Rooms::new(store.clone()),
        store,
    };
    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on {listen}: {e}");
            std::process::exit(1);
        }
    };
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or(listen);
    // Tests and scripts read this line to find the port.
    println!(
        "listening on http://{addr} ({} ms)",
        started.elapsed().as_millis()
    );
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if let Err(e) = axum::serve(listener, http::router(app))
        .with_graceful_shutdown(shutdown)
        .await
    {
        eprintln!("server stopped: {e}");
        std::process::exit(1);
    }
}
