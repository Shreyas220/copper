//! kiln: the copper control plane. SQLite source of truth, placement, the
//! node API, the user API and the web console.
//!
//! - `node_api`, `console` (node side): what ingot talks to.
//! - `user_api`, `nodes_api`, `ops_api`, `people_api`: what cu and the
//!   console talk to.
//! - `ui`: the console's static files, built into the binary.

mod auth;
mod catalog;
mod console;
mod db;
mod error;
mod keys_api;
mod metrics;
mod node_api;
mod nodes_api;
mod ops_api;
mod people_api;
mod placement;
mod sweeper;
mod ui;
mod user_api;
#[cfg(test)]
mod tests;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::{Parser, Subcommand};
use copper_api::{Arch, Role};

#[derive(Parser, Debug)]
#[command(name = "kiln", version)]
struct Opts {
    /// SQLite database path. systemd's StateDirectory gives /var/lib/copper.
    #[arg(long, env = "KILN_DB", default_value = "kiln.db")]
    db: PathBuf,
    /// Image catalogue directory. Defaults to `catalog` next to the database.
    #[arg(long, env = "KILN_CATALOG")]
    catalog: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

const DEFAULT_LISTEN: &str = "127.0.0.1:8400";

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the daemon (default).
    Serve {
        /// Addresses to listen on, comma-separated. Bind the Tailscale address,
        /// and the LAN address too if home machines should reach kiln directly.
        #[arg(long, env = "KILN_LISTEN", value_delimiter = ',', default_value = DEFAULT_LISTEN)]
        listen: Vec<SocketAddr>,
        /// Bridge that instance NICs attach to on every node.
        #[arg(long, env = "KILN_BRIDGE", default_value = "br0")]
        bridge: String,
    },
    /// Create a user (if needed) and print a fresh API token for them.
    /// This is how the first admin is bootstrapped; it needs write access to the DB file.
    Token {
        #[arg(long)]
        user: String,
        /// Preserve an existing role unless explicitly supplied. New users default to user.
        #[arg(long, value_enum)]
        role: Option<RoleArg>,
    },
    /// Print a one-time link to set a console password. Use it to sign in
    /// the first admin; later invites come from the console itself.
    Invite {
        /// The user, new or existing. Existing users get a password reset.
        #[arg(long)]
        user: Option<String>,
        #[arg(long, value_enum)]
        role: Option<RoleArg>,
        /// Create this project for a new user.
        #[arg(long)]
        project: Option<String>,
        /// Where people reach kiln, to print a whole link.
        #[arg(long, default_value = "http://kiln:8400")]
        url: String,
    },
    /// Manage the image catalogue nodes fetch base images from.
    #[command(subcommand)]
    Image(ImageCmd),
}

#[derive(Subcommand, Debug)]
enum ImageCmd {
    /// Add a qcow2 cloud image under a name and arch, e.g. ubuntu-24.04 x86_64.
    Import {
        #[arg(long)]
        name: String,
        #[arg(long, value_enum)]
        arch: ArchArg,
        file: PathBuf,
    },
    /// List catalogue entries.
    Ls,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum RoleArg {
    Admin,
    User,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum ArchArg {
    #[value(name = "x86_64")]
    X86_64,
    #[value(name = "aarch64")]
    Aarch64,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    /// Bumped whenever any node's desired set changes; long-pollers wake and re-check.
    pub changed: tokio::sync::watch::Sender<u64>,
    pub bridge: String,
    pub catalog: PathBuf,
    pub metrics: Arc<metrics::Metrics>,
    pub consoles: Arc<console::Consoles>,
}

impl AppState {
    pub fn new(conn: rusqlite::Connection, bridge: String, catalog: PathBuf) -> Self {
        Self {
            db: Arc::new(Mutex::new(conn)),
            changed: tokio::sync::watch::channel(0).0,
            bridge,
            catalog,
            metrics: Arc::default(),
            consoles: Arc::default(),
        }
    }

    pub fn router(&self) -> axum::Router {
        axum::Router::new()
            .merge(node_api::router())
            .merge(console::router())
            .merge(user_api::router())
            .merge(nodes_api::router())
            .merge(ops_api::router())
            .merge(people_api::router())
            .merge(keys_api::router())
            .merge(ui::router())
            .with_state(self.clone())
    }
}

/// Loopback and the tailnet are the trust boundaries kiln was designed for.
/// Anything else carries bearer tokens in plain HTTP across that network.
fn trusted_network(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64),
        IpAddr::V6(v6) => v6.is_loopback() || v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
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
    let catalog = opts.catalog.clone().unwrap_or_else(|| {
        opts.db.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new(".")).join("catalog")
    });

    let cmd = match opts.cmd {
        Some(cmd) => cmd,
        None => Cmd::Serve {
            listen: std::env::var("KILN_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.into())
                .split(',').map(|a| a.trim().parse().context("parsing KILN_LISTEN")).collect::<anyhow::Result<_>>()?,
            bridge: std::env::var("KILN_BRIDGE").unwrap_or_else(|_| "br0".into()),
        },
    };
    let role_of = |role: Option<RoleArg>| {
        role.map(|role| match role {
            RoleArg::Admin => Role::Admin,
            RoleArg::User => Role::User,
        })
    };
    match cmd {
        Cmd::Token { user, role } => {
            let token = auth::issue_token(&conn, &user, role_of(role), "cli")?;
            println!("{token}");
            Ok(())
        }
        Cmd::Invite { user, role, project, url } => {
            let req = copper_api::CreateInvite { name: user, role: role_of(role), project };
            let invite = people_api::make_invite(&conn, &req, None).map_err(|e| anyhow::anyhow!("{}", e.1))?;
            println!("{}/#/invite/{}", url.trim_end_matches('/'), invite.code);
            eprintln!("works once, for 24 hours");
            Ok(())
        }
        Cmd::Image(ImageCmd::Import { name, arch, file }) => {
            let arch = match arch { ArchArg::X86_64 => Arch::X86_64, ArchArg::Aarch64 => Arch::Aarch64 };
            let image = catalog::import(&conn, &catalog, &name, arch, &file)?;
            println!("{} {} {}", image.name, serde_json::to_string(&image.arch)?.trim_matches('"'), image.sha256);
            Ok(())
        }
        Cmd::Image(ImageCmd::Ls) => {
            for i in db::list_images(&conn)? {
                println!("{:<24} {:<8} {} {:>6} MB", i.name, serde_json::to_string(&i.arch)?.trim_matches('"'), i.sha256, i.size_bytes / (1024 * 1024));
            }
            Ok(())
        }
        Cmd::Serve { listen, bridge } => {
            anyhow::ensure!(!listen.is_empty(), "nothing to listen on");
            let state = AppState::new(conn, bridge, catalog);
            let app = state.router();
            tokio::spawn(sweeper::run(state));
            let mut servers = tokio::task::JoinSet::new();
            for addr in listen {
                if !trusted_network(addr.ip()) {
                    tracing::warn!(%addr, "serving plain HTTP outside loopback and the tailnet: user and node tokens cross this network unencrypted");
                }
                let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
                tracing::info!(%addr, db = %opts.db.display(), "kiln listening");
                let app = app.clone();
                servers.spawn(async move { axum::serve(listener, app).await });
            }
            // Any listener failing takes the daemon down; systemd restarts it.
            if let Some(result) = servers.join_next().await {
                result??;
            }
            Ok(())
        }
    }
}
