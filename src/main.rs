use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use cleanrr::{
    config::Config,
    metrics::Metrics,
    service::run_cleaner,
    web::{HealthState, router},
};
use tokio::{net::TcpListener, task::JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cleanrr: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let config = Config::load()?;
    init_logging();

    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("could not bind {}", config.listen_addr))?;
    let address = listener.local_addr()?;
    let cancellation = CancellationToken::new();
    let health = HealthState::new();
    let metrics = Metrics::new();
    let app = router(health.clone(), metrics.clone());

    let mut cleaners = JoinSet::new();
    for (name, server) in config.servers.clone() {
        cleaners.spawn(run_cleaner(
            name,
            server,
            config.clone(),
            metrics.clone(),
            cancellation.child_token(),
        ));
    }

    health.set_ready(true);
    info!(%address, servers = config.servers.len(), version = env!("CARGO_PKG_VERSION"), "cleanrr started");

    let server_cancellation = cancellation.clone();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(server_cancellation.cancelled_owned())
        .into_future();
    tokio::pin!(server);

    let mut server_finished = false;
    let outcome: Result<()> = tokio::select! {
        signal = shutdown_signal() => signal,
        result = &mut server => {
            server_finished = true;
            match result {
                Ok(()) => bail!("HTTP server stopped unexpectedly"),
                Err(error) => Err(error).context("HTTP server failed"),
            }
        }
        result = cleaners.join_next() => {
            match result {
                Some(Ok(())) => bail!("a cleaner stopped unexpectedly"),
                Some(Err(error)) => Err(error).context("a cleaner task failed"),
                None => bail!("all cleaner tasks stopped unexpectedly"),
            }
        }
    };

    health.set_ready(false);
    cancellation.cancel();
    info!("shutdown started");

    let drain = async {
        while let Some(result) = cleaners.join_next().await {
            if let Err(error) = result {
                error!(%error, "cleaner task failed during shutdown");
            }
        }
        if !server_finished {
            server.await.context("HTTP server failed during shutdown")?;
        }
        Result::<()>::Ok(())
    };

    const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, drain).await {
        Ok(result) => result?,
        Err(_) => bail!(
            "graceful shutdown exceeded {} seconds",
            SHUTDOWN_TIMEOUT.as_secs_f64()
        ),
    }

    outcome?;
    info!("shutdown complete");
    Ok(())
}

fn init_logging() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("cleanrr=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .init();
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("could not register SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("could not register Ctrl-C handler")?,
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .context("could not register Ctrl-C handler")?;

    Ok(())
}
