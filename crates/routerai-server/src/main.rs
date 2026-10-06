//! RouterAi HTTP API + embedded Web Console.

mod api;
mod credentials;
mod static_files;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use clap::Parser;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::api::AppState;
use crate::credentials::{build_ai_client, data_dir, routerai_db_path};
use routerai::{RouterRuntime, SqliteStore};
use routerai_adapters::webhook::{HttpWebhookSink, WebhookSinkConfig};

#[derive(Debug, Parser)]
#[command(name = "routerai-server", about = "RouterAi API + Web Console")]
struct Args {
    /// Listen port.
    #[arg(long, short = 'p', env = "ROUTERAI_PORT", default_value_t = 8080)]
    port: u16,

    /// Listen host / interface.
    #[arg(long, env = "ROUTERAI_HOST", default_value = "127.0.0.1")]
    host: String,

    /// Full bind address (`host:port`). Overrides `--host` / `--port` when set.
    #[arg(long, env = "ROUTERAI_BIND")]
    bind: Option<String>,

    /// Serve Console from this directory instead of the embedded build (dev override).
    #[arg(long, env = "ROUTERAI_WEB_DIR")]
    web_dir: Option<PathBuf>,

    /// Directory for secrets + credentials metadata (`ROUTERAI_DATA_DIR`).
    #[arg(long, env = "ROUTERAI_DATA_DIR")]
    data_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let args = Args::parse();
    let addr: SocketAddr = if let Some(bind) = &args.bind {
        bind.parse()?
    } else {
        format!("{}:{}", args.host, args.port).parse()?
    };

    let data = args.data_dir.unwrap_or_else(data_dir);
    tokio::fs::create_dir_all(&data).await?;
    let ai = build_ai_client(&data).await?;
    tracing::info!(
        data_dir = %data.display(),
        providers = ai.provider_summaries().len(),
        "AiClient ready"
    );

    let db_path = routerai_db_path(&data);
    let store = SqliteStore::connect(&format!("sqlite://{}?mode=rwc", db_path.display())).await?;
    tracing::info!(db = %db_path.display(), "RouterAi SQLite store ready");

    let runtime = Arc::new(
        RouterRuntime::builder()
            .ai(Arc::clone(&ai))
            .store(Arc::new(store))
            .build()
            .await?,
    );

    let sink = Arc::new(HttpWebhookSink::new(
        runtime.sinks().clone(),
        WebhookSinkConfig::default(),
    )?);
    runtime.sinks().add_sink(sink).await;

    let webhook_secret = std::env::var("ROUTERAI_WEBHOOK_SECRET").ok();
    let state = AppState {
        runtime,
        webhook_secret,
        credentials_path: credentials::credentials_path(&data),
    };

    let api = api::router(state);
    let app = build_app(api, args.web_dir.as_deref())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, console = static_files::has_console(), "routerai-server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn build_app(api: Router, web_dir: Option<&std::path::Path>) -> Router {
    if let Some(dir) = web_dir {
        let index = dir.join("index.html");
        return api.fallback_service(ServeDir::new(dir).not_found_service(ServeFile::new(index)));
    }
    api.fallback(get(static_files::static_handler))
}
