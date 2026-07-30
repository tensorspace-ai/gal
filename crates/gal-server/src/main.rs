//! Gal — an Apache Wave-style collaboration server.

mod auth;
mod config;
mod db;
#[cfg(test)]
mod e2e;
mod http;
mod limit;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("GAL_LOG")
                .unwrap_or_else(|_| "gal_server=info,tower_http=warn".into()),
        )
        .with_target(false)
        .init();

    let config = Config::from_env()?;

    // `--healthcheck` probes an already-running instance and exits; it does not
    // start a server or touch the database directly.
    if std::env::args().any(|a| a == "--healthcheck") {
        return healthcheck(&config);
    }

    let storage = Storage::open(&config.database)
        .with_context(|| format!("could not open database at {}", config.database.display()))?;

    let user_count = storage.user_count().await?;
    let state = AppState::new(storage, config.clone());

    let app = http::router(state)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("could not bind {}", config.addr))?;

    tracing::info!("Gal listening on http://{}", config.addr);
    tracing::info!("database: {}", config.database.display());
    if user_count == 0 {
        tracing::info!("no accounts yet — open the URL above and sign up to create the first one");
    }

    // Connect-info gives handlers the peer address, which rate limiting needs.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("server error")?;
    Ok(())
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
