use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use entitle_server::cli::{Cli, Command, run_admin};
use entitle_server::config::Config;
use entitle_server::{Adapters, AppState, Cipher, MIGRATOR, fx, outbox, router};
use sqlx::postgres::PgPoolOptions;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        None | Some(Command::Serve) => serve().await,
        Some(Command::Admin(args)) => {
            match run_admin(args).await? {
                serde_json::Value::String(message) => println!("{message}"),
                value => println!("{}", serde_json::to_string_pretty(&value)?),
            }
            Ok(())
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if std::env::var("ENTITLE_LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    let config = Config::from_env()?;
    let cipher = Arc::new(Cipher::from_base64(&config.encryption_key)?);
    let db = PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await
        .context("connecting to postgres")?;
    MIGRATOR.run(&db).await.context("running migrations")?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let worker = tokio::spawn(outbox::run_worker(
        db.clone(),
        cipher.clone(),
        config.webhook_allow_private,
        shutdown_rx.clone(),
    ));
    let fx_worker = tokio::spawn(fx::run_worker(db.clone(), shutdown_rx));

    let state = AppState {
        db,
        cipher,
        admin_token: config.admin_token.into(),
        adapters: Adapters::real(),
        webhook_allow_private: config.webhook_allow_private,
        trust_proxy: config.trust_proxy,
        public_url: config.public_url.map(Into::into),
    };
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;
    tracing::info!(addr = %config.bind, "entitle listening");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    let _ = shutdown_tx.send(true);
    worker.await?;
    fx_worker.await?;
    tracing::info!("shut down cleanly");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
