//! ingot: the copper node agent. Reconciles libvirt toward the last desired
//! state received from kiln, and reports what it observes.

mod client;
mod disk;
mod hv;
mod identity;
mod reconcile;
mod state;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use copper_api::*;

use crate::client::Kiln;
use crate::reconcile::Reconciler;

#[derive(Parser, Debug, Clone)]
#[command(name = "ingot", version)]
struct Opts {
    /// Kiln base URL. The default relies on the kiln machine being named
    /// `kiln` on the tailnet, so MagicDNS resolves it from anywhere.
    #[arg(long, env = "INGOT_KILN", default_value = "http://kiln:8400")]
    kiln: String,
    /// State directory: node id, cached desired state, disks.
    /// systemd's StateDirectory gives /var/lib/copper.
    #[arg(long, env = "INGOT_STATE_DIR", default_value = "/var/lib/copper")]
    state_dir: PathBuf,
    /// Base images, one <name>.img per image.
    #[arg(long, env = "INGOT_IMAGES_DIR")]
    images_dir: Option<PathBuf>,
    #[arg(long, env = "INGOT_LIBVIRT", default_value = "qemu:///system")]
    libvirt: String,
    /// CPU threads kept back for the host and anything else running on it.
    #[arg(long, env = "INGOT_RESERVE_VCPUS", default_value_t = 2)]
    reserve_vcpus: u32,
    /// Memory kept back for the host, in MB.
    #[arg(long, env = "INGOT_RESERVE_MEMORY_MB", default_value_t = 4096)]
    reserve_memory_mb: u64,
}

const REPORT_EVERY: Duration = Duration::from_secs(10);
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let opts = Opts::parse();
    std::fs::create_dir_all(&opts.state_dir).with_context(|| format!("creating {}", opts.state_dir.display()))?;
    let images_dir = opts.images_dir.clone().unwrap_or_else(|| opts.state_dir.join("images"));

    let node_id = identity::load_or_create(&opts.state_dir)?;
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string();
    let arch = match std::env::consts::ARCH {
        "x86_64" => Arch::X86_64,
        "aarch64" => Arch::Aarch64,
        other => anyhow::bail!("unsupported architecture {other}"),
    };
    tracing::info!(%node_id, %hostname, kiln = %opts.kiln, "ingot starting");

    let hv = hv::Libvirt::connect(&opts.libvirt)?;
    let reserve = Capacity { vcpus: opts.reserve_vcpus, memory_mb: opts.reserve_memory_mb, disk_gb: 0 };
    let reconciler = Arc::new(Mutex::new(Reconciler::new(hv, opts.state_dir.join("disks"), images_dir, reserve)));

    // Reconcile toward the cached desired state before talking to anyone, so
    // VMs come back after a host reboot even if kiln is unreachable.
    let desired = Arc::new(Mutex::new(state::load_desired(&opts.state_dir)?));
    if let Some(d) = desired.lock().unwrap().clone() {
        tracing::info!(generation = d.generation, "reconciling from cached desired state");
        run_reconcile(&reconciler, &d).await;
    }

    let kiln = Kiln::new(&opts.kiln, node_id);
    let register = RegisterRequest { node_id, hostname, arch, capacity: snapshot_capacity(&reconciler).await };
    register_until_approved(&kiln, &register).await?;

    let a = tokio::spawn(desired_loop(kiln.clone(), opts.state_dir.clone(), desired.clone(), reconciler.clone()));
    let b = tokio::spawn(report_loop(kiln, desired, reconciler));
    tokio::select! {
        r = a => r??,
        r = b => r??,
    }
    Ok(())
}

async fn run_reconcile(reconciler: &Arc<Mutex<Reconciler>>, desired: &DesiredNodeState) {
    let r = reconciler.clone();
    let d = desired.clone();
    tokio::task::spawn_blocking(move || r.lock().unwrap().reconcile(&d)).await.unwrap();
}

async fn snapshot_capacity(reconciler: &Arc<Mutex<Reconciler>>) -> Capacity {
    let r = reconciler.clone();
    tokio::task::spawn_blocking(move || r.lock().unwrap().capacity()).await.unwrap()
}

async fn register_until_approved(kiln: &Kiln, req: &RegisterRequest) -> anyhow::Result<()> {
    let mut announced = false;
    loop {
        match kiln.register(req).await {
            Ok(RegistrationStatus::Approved) => {
                tracing::info!("node approved");
                return Ok(());
            }
            Ok(RegistrationStatus::Pending) => {
                if !announced {
                    tracing::warn!(node = %req.node_id, "registered; waiting for admin approval (cu node approve {})", req.node_id);
                    announced = true;
                }
            }
            Ok(RegistrationStatus::Retired) => anyhow::bail!("this node has been retired by the admin; refusing to run"),
            Err(e) => tracing::warn!(error = %e, "kiln unreachable; retrying"),
        }
        tokio::time::sleep(RETRY_BACKOFF).await;
    }
}

/// Long-poll desired state. Each new generation is persisted before it is
/// acted on. Also re-runs reconcile periodically so crashed VMs restart and
/// failed starts get their retries.
async fn desired_loop(
    kiln: Kiln,
    state_dir: PathBuf,
    desired: Arc<Mutex<Option<DesiredNodeState>>>,
    reconciler: Arc<Mutex<Reconciler>>,
) -> anyhow::Result<()> {
    loop {
        let after = desired.lock().unwrap().as_ref().map(|d| d.generation);
        match kiln.desired(after).await {
            Ok(d) => {
                let is_new = after != Some(d.generation);
                if is_new {
                    state::save_desired(&state_dir, &d)?;
                    tracing::info!(generation = d.generation, instances = d.instances.len(), "new desired state");
                    *desired.lock().unwrap() = Some(d.clone());
                }
                run_reconcile(&reconciler, &d).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "desired-state poll failed");
                let cached = desired.lock().unwrap().clone();
                if let Some(d) = cached {
                    run_reconcile(&reconciler, &d).await;
                }
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
        }
    }
}

/// Report observed state every REPORT_EVERY. Only the latest report matters,
/// so a failed send is simply superseded by the next one.
async fn report_loop(
    kiln: Kiln,
    desired: Arc<Mutex<Option<DesiredNodeState>>>,
    reconciler: Arc<Mutex<Reconciler>>,
) -> anyhow::Result<()> {
    loop {
        let generation_seen = desired.lock().unwrap().as_ref().map(|d| d.generation).unwrap_or(0);
        let r = reconciler.clone();
        let (instances, capacity) = tokio::task::spawn_blocking(move || {
            let r = r.lock().unwrap();
            (r.observe(), r.capacity())
        })
        .await
        .unwrap();
        let report = ObservedNodeState { generation_seen, instances, capacity };
        if let Err(e) = kiln.observed(&report).await {
            tracing::warn!(error = %e, "report failed");
        }
        tokio::time::sleep(REPORT_EVERY).await;
    }
}
