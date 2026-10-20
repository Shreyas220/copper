//! ingot: runs on every compute node and reconciles the hypervisor
//! toward the last desired state it received from the controller.

mod identity;
mod reconcile;
mod state;

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "ingot")]
struct Opts {
    /// Kiln base URL, reached over the tailnet.
    #[arg(long, env = "INGOT_KILN")]
    controller: String,
    /// State directory. systemd's StateDirectory gives /var/lib/copper.
    #[arg(long, env = "INGOT_STATE_DIR", default_value = ".")]
    state_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let opts = Opts::parse();

    let node_id = identity::load_or_create(&opts.state_dir)?;
    tracing::info!(%node_id, controller = %opts.controller, "agent starting");

    // Reconcile toward the cached desired state before talking to anyone,
    // so VMs come back after a host reboot even if the controller is down.
    let cached = state::load_desired(&opts.state_dir)?;
    if let Some(desired) = &cached {
        reconcile::run(desired).await?;
    }

    let client = reqwest::Client::new();
    let desired_loop = tokio::spawn(desired_loop(client.clone(), opts.controller.clone(), node_id, opts.state_dir.clone()));
    let report_loop = tokio::spawn(report_loop(client, opts.controller, node_id));

    tokio::select! {
        r = desired_loop => r??,
        r = report_loop => r??,
    }
    Ok(())
}

/// Register until approved, then long-poll desired state. Each new generation
/// is persisted before it is acted on. Retries forever with backoff.
async fn desired_loop(
    _client: reqwest::Client,
    _controller: String,
    _node_id: copper_api::NodeId,
    _state_dir: PathBuf,
) -> anyhow::Result<()> {
    loop {
        // TODO: POST /nodes/register until Approved.
        // TODO: GET /nodes/{id}/desired?after=<gen>, save, reconcile.
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// Report observed state every 10 s and after every state change.
async fn report_loop(
    _client: reqwest::Client,
    _controller: String,
    _node_id: copper_api::NodeId,
) -> anyhow::Result<()> {
    loop {
        // TODO: snapshot hypervisor, POST /nodes/{id}/observed. Only the latest matters.
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
