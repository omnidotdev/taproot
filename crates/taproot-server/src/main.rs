mod config;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use taproot_core::db::SqlxBackend;
use taproot_core::introspect::types::IntrospectOptions;
use taproot_core::subscription::{ChangeBroadcaster, ChangeNotification};
use taproot_core::{build_schema, introspect};
use tracing::info;

/// Taproot — auto-generate a `GraphQL` API from Postgres.
#[derive(Parser)]
#[command(name = "taproot", version)]
struct Cli {
    /// Path to the YAML configuration file.
    #[arg(short, long, default_value = "taproot.yaml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Load configuration
    let mut cfg = config::Config::from_file(&cli.config)
        .with_context(|| format!("failed to load config from {}", cli.config.display()))?;

    // Connect to database
    info!(url = %mask_url(&cfg.database.url), "connecting to database");
    let backend = SqlxBackend::connect(&cfg.database.url)
        .await
        .context("failed to connect to database")?;

    let db: Arc<dyn taproot_core::db::Database> = Arc::new(backend);

    // Introspect the database
    let introspect_opts = IntrospectOptions {
        schemas: cfg.database.schemas.clone(),
        exclude_tables: cfg.database.exclude_tables.clone(),
        smart_comments: true,
    };

    info!(schemas = ?cfg.database.schemas, "introspecting database");
    let schemas = introspect(db.as_ref(), &introspect_opts)
        .await
        .context("failed to introspect database")?;

    let table_count: usize = schemas.iter().map(|s| s.tables.len()).sum();
    info!(table_count, "introspection complete");

    // Create subscription broadcaster
    let broadcaster = if cfg.subscriptions.enabled {
        cfg.schema.subscriptions = true;
        Some(ChangeBroadcaster::new(256))
    } else {
        None
    };

    // Build GraphQL schema
    let gql_schema = build_schema(&schemas, &db, &cfg.schema, &[], broadcaster.as_ref())
        .context("failed to build GraphQL schema")?;

    // Start subscription listener if enabled
    if let Some(ref broadcaster) = broadcaster {
        let db_url = cfg.database.url.clone();
        let channel = cfg.subscriptions.notify_channel.clone();
        let bc = broadcaster.clone();

        tokio::spawn(async move {
            if let Err(e) = listen_task(&db_url, &channel, &bc).await {
                tracing::error!(error = %e, "subscription listener failed");
            }
        });
    }

    // Build and start server
    let router = server::build_router(gql_schema, &cfg.server, &cfg.auth);

    let listener = tokio::net::TcpListener::bind(&cfg.server.listen)
        .await
        .with_context(|| format!("failed to bind to {}", cfg.server.listen))?;

    info!(
        listen = %cfg.server.listen,
        playground = cfg.server.playground,
        "taproot server started"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    tracing::info!("server stopped");

    Ok(())
}

/// Wait for a SIGTERM or SIGINT signal to initiate graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    tracing::info!("shutdown signal received, draining connections");
}

/// Background task that listens for Postgres NOTIFY events and broadcasts them
async fn listen_task(
    db_url: &str,
    channel: &str,
    broadcaster: &ChangeBroadcaster,
) -> anyhow::Result<()> {
    use sqlx::postgres::PgListener;

    let mut listener = PgListener::connect(db_url).await?;
    listener.listen(channel).await?;
    tracing::info!(channel, "subscription listener started");

    loop {
        let notification = listener.recv().await?;
        if let Some(change) = ChangeNotification::from_payload(notification.payload()) {
            tracing::debug!(
                table = %change.table,
                op = ?change.op,
                id = %change.id,
                "received change notification"
            );
            broadcaster.send(change);
        } else {
            tracing::warn!(
                payload = notification.payload(),
                "invalid notification payload"
            );
        }
    }
}

/// Mask the password portion of a database URL for safe logging.
fn mask_url(url: &str) -> String {
    // Replace password between `://user:` and `@`
    if let Some(at_pos) = url.find('@')
        && let Some(colon_pos) = url[..at_pos].rfind(':')
        && let Some(scheme_end) = url.find("://")
        && colon_pos > scheme_end + 3
    {
        return format!("{}:***{}", &url[..colon_pos], &url[at_pos..]);
    }
    url.to_string()
}
