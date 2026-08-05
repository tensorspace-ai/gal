//! Gal — an Apache Wave-style collaboration server.

mod auth;
mod config;
mod db;
#[cfg(test)]
mod e2e;
mod http;
mod limit;
mod metrics;
mod oidc;
mod state;
mod ws;

use anyhow::{Context, Result};
use config::Config;
use db::Storage;
use state::AppState;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

/// Probe a running server's `/healthz` and exit 0 or 1.
///
/// Exists so a container image needs no shell and no curl to be health-checked.
/// Deliberately written with `std::net` rather than an HTTP client: the check
/// must not be able to fail for reasons of its own.
fn healthcheck(config: &Config) -> Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let timeout = Duration::from_secs(3);
    let mut stream = TcpStream::connect_timeout(&config.addr, timeout)
        .with_context(|| format!("could not connect to {}", config.addr))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    write!(
        stream,
        "GET /healthz HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        config.addr
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    let status = response.lines().next().unwrap_or_default();
    if status.contains(" 200 ") {
        Ok(())
    } else {
        anyhow::bail!("health check failed: {status}")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Read before logging is set up, so a bad value is reported rather than
    // logged in a format the operator did not choose.
    let config = Config::from_env()?;

    let filter = tracing_subscriber::EnvFilter::try_from_env("GAL_LOG")
        .unwrap_or_else(|_| "gal_server=info,tower_http=warn".into());
    if config.log_json {
        // One JSON object per line, for anything that ships logs somewhere. The
        // human format is unparseable by design — it is aligned for reading.
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_env_filter(filter)
            .with_target(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .init();
    }

    install_panic_logger();

    // `--healthcheck` probes an already-running instance and exits; it does not
    // start a server or touch the database directly.
    if std::env::args().any(|a| a == "--healthcheck") {
        return healthcheck(&config);
    }

    let storage = Storage::open(&config.database)
        .with_context(|| format!("could not open database at {}", config.database.display()))?;

    let user_count = storage.user_count().await?;
    let state = AppState::new(storage, config.clone());

    let app = http::router(state.clone())
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        // A backstop against a request that never finishes arriving. It bounds
        // producing the response, and a WebSocket's response is the 101 — the
        // socket itself is not held to it. Generous, because it also covers a
        // ten-megabyte upload over a bad connection.
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(120),
        ));

    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("could not bind {}", config.addr))?;

    tracing::info!("Gal listening on http://{}", config.addr);
    tracing::info!("database: {}", config.database.display());
    if user_count == 0 {
        tracing::info!("no accounts yet — open the URL above and sign up to create the first one");
    }

    // Connect-info gives handlers the peer address, which rate limiting needs.
    let winding_down = state.clone();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        tracing::info!("shutting down: asking open sockets to close");
        // Told here rather than after `serve` returns, so the sockets wind down
        // while axum is finishing its in-flight HTTP requests instead of after.
        winding_down.begin_shutdown();
    })
    .await
    .context("server error")?;

    // axum's graceful shutdown does not cover WebSockets: an upgraded socket is
    // served by a task it spawned and stopped tracking, so without this the
    // process exits while every one of them is mid-frame.
    let still_open = state.drain_connections(SHUTDOWN_GRACE).await;
    if still_open > 0 {
        tracing::warn!(
            connections = still_open,
            "exiting with sockets still open after the grace period"
        );
    } else {
        tracing::info!("all sockets closed");
    }
    Ok(())
}

/// How long to wait for open sockets to close on the way out.
///
/// Long enough for a connection to finish the command it is in and shut down
/// tidily; short enough that a deploy is not held up by one wedged client.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Send panics to `tracing` as well as to stderr.
///
/// A panic in a connection task is contained rather than fatal, and nothing
/// joins that task, so without this the process carries on and the only record
/// of the bug is a line on stderr in a different format from every other line —
/// invisible to anything parsing the log. The default hook still runs, so a
/// panic on the main thread reports exactly as it did before.
fn install_panic_logger() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(
            location = %info.location().map(|l| l.to_string()).unwrap_or_default(),
            thread = std::thread::current().name().unwrap_or("unnamed"),
            "panic: {}",
            info.payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("(non-string panic payload)"),
        );
        default_hook(info);
    }));
}

/// Resolve on Ctrl-C or SIGTERM so container stops are clean.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down");
}
