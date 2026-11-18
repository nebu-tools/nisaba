//! `nisaba serve`: start, run, and stop without losing a write.
//!
//! On `SIGTERM` or Ctrl-C the server stops taking connections, answers
//! health checks with 503, ends every export (its child killed and reaped),
//! and closes each sync socket with 1012 after the frame it is on, so an
//! update that has arrived is written and acknowledged first. It waits up
//! to [`SHUTDOWN_GRACE`] for open requests and sockets, then for every room
//! to finish the write or compaction it is in, and checkpoints the
//! database.

use std::time::{Duration, Instant};

use crate::config::{LogFormat, PublicAddress, Settings};
use crate::export::Exporter;
use crate::http::{self, App, Options};
use crate::limits::SHUTDOWN_GRACE;
use crate::store::Store;

/// Logs go to standard error, as text or JSON lines. `RUST_LOG` picks the
/// level; the default is `info`. No log line holds a password, a session
/// token, an email address or document text: requests are logged by method
/// and path, people by their account id.
pub fn init_logs(format: LogFormat) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "nisaba=info,nisaba_server=info".into());
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    let _ = match format {
        LogFormat::Text => builder.try_init(),
        LogFormat::Json => builder.json().flatten_event(true).try_init(),
    };
}

/// Runs the server until it is told to stop.
pub async fn serve(settings: Settings) -> Result<(), String> {
    let started = Instant::now();
    init_logs(settings.log_format);
    #[cfg(unix)]
    let _file_size = ignore_file_size_signal()?;
    let store = Store::open(&settings.data).map_err(|e| {
        format!(
            "cannot open the data directory {}: {e}",
            settings.data.display()
        )
    })?;
    let swept = store.sweep_assets().map_err(|e| e.to_string())?;
    if swept > 0 {
        tracing::info!(files = swept, "removed asset files no project holds");
    }
    let listener = tokio::net::TcpListener::bind(&settings.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", settings.listen))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let public = PublicAddress::resolve(
        addr,
        settings.public_url.as_deref(),
        settings.allow_insecure_http,
    )?;
    // Setup needs no code only when nothing but this machine can reach the
    // server: it listens on loopback and people use a loopback address.
    let local_only = addr.ip().is_loopback() && public.is_loopback();
    let setup_code = store
        .prepare_setup(!local_only)
        .map_err(|e| e.to_string())?;
    let exporter = Exporter::new(Exporter::this_binary(), settings.export.clone());
    let transfer_bytes = usize::try_from(settings.sync_transfer_mb << 20)
        .map_err(|_| "sync.transfer_mb is too large for this system".to_owned())?;
    let app = App::new(
        store.clone(),
        Options {
            public: public.clone(),
            exporter: exporter.clone(),
            transfer_bytes,
            passwords: settings.passwords,
            packages: settings.packages.clone(),
        },
    )?;
    app.rooms().spawn_sweeper(Duration::from_secs(60));
    if !crate::web::built() {
        tracing::warn!("this server was built without the web client; only the API works");
    }
    // Tests and scripts read these lines: the port, the setup code, and the
    // public address, which always comes last.
    println!(
        "listening on http://{addr} ({} ms)",
        started.elapsed().as_millis()
    );
    if let Some(code) = setup_code {
        println!(
            "setup code {code}\n  Open {} and enter this code to create the administrator \
             account. It works once; a restart before setup makes a new one.",
            public.origin()
        );
    }
    println!("{}", exporter.describe());
    println!("public address {}", public.origin());
    let (stopping, mut stopped) = tokio::sync::watch::channel(false);
    let shutdown = {
        let app = app.clone();
        async move {
            stop_signal().await;
            tracing::info!("stopping");
            app.stop();
            stopping.send_replace(true);
        }
    };
    let serve = axum::serve(listener, http::router(app.clone())).with_graceful_shutdown(shutdown);
    let grace = async move {
        let _ = stopped.wait_for(|s| *s).await;
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    };
    tokio::select! {
        result = serve => result.map_err(|e| format!("the server stopped: {e}"))?,
        () = grace => tracing::info!("stopped with connections still open"),
    }
    // Each socket closes after the frame it is on, and waits for the
    // client's answer to its close frame.
    if tokio::time::timeout(SHUTDOWN_GRACE, app.sockets_closed())
        .await
        .is_err()
    {
        tracing::warn!("a sync socket was still open when the server stopped");
    }
    // Every room finishes the write it is in; then the log is folded into
    // the database file.
    if tokio::time::timeout(SHUTDOWN_GRACE, app.rooms().quiesce())
        .await
        .is_err()
    {
        tracing::warn!("a room was still writing when the server stopped");
    }
    if let Err(e) = store.checkpoint() {
        tracing::warn!(error = %e, "cannot checkpoint the database");
    }
    tracing::info!("stopped");
    Ok(())
}

/// Past a file size limit, the kernel sends `SIGXFSZ`, which would end the
/// process. With a handler the write fails instead, and the server answers
/// "storage full" as for a full disk.
#[cfg(unix)]
fn ignore_file_size_signal() -> Result<tokio::signal::unix::Signal, String> {
    // SIGXFSZ is 25 on Linux and the BSDs.
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(25))
        .map_err(|e| format!("cannot handle SIGXFSZ: {e}"))
}

/// Ctrl-C, or SIGTERM as from systemd or `kill`.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
