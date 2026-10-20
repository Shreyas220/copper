//! kiln: SQLite source of truth, placement, user API, node API.

mod db;
mod node_api;
mod user_api;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "kiln")]
struct Opts {
    /// Address to listen on. Bind to the Tailscale address in production.
    #[arg(long, env = "KILN_LISTEN", default_value = "127.0.0.1:8400")]
    listen: SocketAddr,
    /// SQLite database path. systemd's StateDirectory gives /var/lib/copper.
    #[arg(long, env = "KILN_DB", default_value = "controller.db")]
    db: PathBuf,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let opts = Opts::parse();

    let conn = db::open(&opts.db)?;
    let state = AppState { db: Arc::new(Mutex::new(conn)) };

    let app = axum::Router::new()
        .merge(node_api::router())
        .merge(user_api::router())
        .with_state(state);

    tracing::info!(listen = %opts.listen, db = %opts.db.display(), "controller starting");
    let listener = tokio::net::TcpListener::bind(opts.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
