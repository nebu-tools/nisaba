//! `nisaba [--data DIR] [--listen ADDR] [--public-url URL] ...`; see
//! [`nisaba_server::config::USAGE`].

use std::time::{Duration, Instant};

use nisaba_server::config::{PublicAddress, Settings, USAGE};
use nisaba_server::export::{self, Exporter};
use nisaba_server::{http, store::Store};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The export child: no runtime, no logging, nothing but the compile.
    if args.first().map(String::as_str) == Some(export::SUBCOMMAND) {
        std::process::exit(export::child_main(&args[1..]));
    }
    let settings = match Settings::parse(args) {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(serve(settings));
}

fn exit(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

async fn serve(settings: Settings) {
    let started = Instant::now();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nisaba=info,nisaba_server=info".into()),
        )
        .init();
    let store = Store::open(&settings.data).unwrap_or_else(|e| {
        exit(&format!(
            "cannot open data directory {}: {e}",
            settings.data.display()
        ))
    });
    let listener = tokio::net::TcpListener::bind(&settings.listen)
        .await
        .unwrap_or_else(|e| exit(&format!("cannot listen on {}: {e}", settings.listen)));
    let addr = listener
        .local_addr()
        .unwrap_or_else(|e| exit(&e.to_string()));
    let public = PublicAddress::resolve(
        addr,
        settings.public_url.as_deref(),
        settings.allow_insecure_http,
    )
    .unwrap_or_else(|e| exit(&e));
    // Setup needs no code only when nothing but this machine can reach the
    // server: it listens on loopback and people use a loopback address.
    let local_only = addr.ip().is_loopback() && public.is_loopback();
    let setup_code = store
        .prepare_setup(!local_only)
        .unwrap_or_else(|e| exit(&e));
    let exporter = Exporter::new(Exporter::this_binary(), settings.export.clone());
    let transfer_bytes = usize::try_from(settings.sync_transfer_mb << 20)
        .unwrap_or_else(|_| exit("--sync-transfer-mb is too large for this system"));
    let app = http::App::new(store, public.clone(), exporter.clone(), transfer_bytes);
    app.rooms().spawn_sweeper(Duration::from_secs(60));
    // Tests and scripts read these lines: the port, the setup code, and the
    // public address, which always comes last.
    println!(
        "listening on http://{addr} ({} ms)",
        started.elapsed().as_millis()
    );
    if let Some(code) = setup_code {
        println!(
            "setup code {code}\n  Open {} and enter this code to create the owner account. \
             It works once; a restart before setup makes a new one.",
            public.origin()
        );
    }
    println!("{}", exporter.describe());
    println!("public address {}", public.origin());
    let (stopping, mut stopped) = tokio::sync::watch::channel(false);
    let shutdown = async move {
        stop_signal().await;
        tracing::info!("stopping");
        // Exports end at once, their children killed and reaped.
        exporter.stop();
        stopping.send_replace(true);
    };
    let serve = axum::serve(listener, http::router(app)).with_graceful_shutdown(shutdown);
    // Open sync sockets would hold a graceful shutdown for ever.
    let grace = async move {
        let _ = stopped.wait_for(|s| *s).await;
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    };
    tokio::select! {
        result = serve => if let Err(e) = result {
            exit(&format!("server stopped: {e}"));
        },
        () = grace => tracing::info!("stopped with connections still open"),
    }
}

/// How long a stopping server waits for open requests and sockets.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Ctrl-C, or SIGTERM as from systemd or `kill`.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
