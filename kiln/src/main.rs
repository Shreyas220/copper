//! kiln: the copper control plane. SQLite source of truth, placement,
//! user API, node API.

mod auth;
mod db;
mod error;
mod node_api;
mod placement;
mod user_api;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use copper_api::Role;

#[derive(Parser, Debug)]
#[command(name = "kiln", version)]
struct Opts {
    /// SQLite database path. systemd's StateDirectory gives /var/lib/copper.
    #[arg(long, env = "KILN_DB", default_value = "kiln.db")]
    db: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the daemon (default).
    Serve {
        /// Address to listen on. Bind to the Tailscale address in production.
        #[arg(long, env = "KILN_LISTEN", default_value = "127.0.0.1:8400")]
        listen: SocketAddr,
        /// Bridge that instance NICs attach to on every node.
        #[arg(long, env = "KILN_BRIDGE", default_value = "br0")]
        bridge: String,
    },
    /// Create a user (if needed) and print a fresh API token for them.
    /// This is how the first admin is bootstrapped; it needs write access to the DB file.
    Token {
        #[arg(long)]
        user: String,
        #[arg(long, value_enum, default_value = "user")]
        role: RoleArg,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum RoleArg {
    Admin,
    User,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    /// Bumped whenever any node's desired set changes; long-pollers wake and re-check.
    pub changed: tokio::sync::watch::Sender<u64>,
    pub bridge: String,
}

impl AppState {
    pub fn notify_changed(&self) {
        self.changed.send_modify(|v| *v += 1);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let opts = Opts::parse();
    let conn = db::open(&opts.db)?;

    match opts.cmd.unwrap_or(Cmd::Serve { listen: "127.0.0.1:8400".parse()?, bridge: "br0".into() }) {
        Cmd::Token { user, role } => {
            let role = match role {
                RoleArg::Admin => Role::Admin,
                RoleArg::User => Role::User,
            };
            let token = auth::issue_token(&conn, &user, role)?;
            println!("{token}");
            Ok(())
        }
        Cmd::Serve { listen, bridge } => {
            let (changed, _) = tokio::sync::watch::channel(0);
            let state = AppState { db: Arc::new(Mutex::new(conn)), changed, bridge };
            let app = axum::Router::new()
                .merge(node_api::router())
                .merge(user_api::router())
                .with_state(state);
            tracing::info!(%listen, db = %opts.db.display(), "kiln listening");
            let listener = tokio::net::TcpListener::bind(listen).await?;
            axum::serve(listener, app).await?;
            Ok(())
        }
    }
}
