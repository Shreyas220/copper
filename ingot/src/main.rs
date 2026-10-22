//! ingot: the copper node agent. Reconciles the node's hypervisor toward the
//! last desired state received from kiln, and reports what it observes.

mod client;
#[cfg(not(target_os = "macos"))]
mod console;
mod disk;
#[cfg(not(target_os = "macos"))]
mod host;
mod hv;
mod identity;
mod images;
#[cfg(not(target_os = "macos"))]
mod libvirt;
mod reconcile;
mod state;
#[cfg(any(target_os = "macos", feature = "tart"))]
mod tart;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use copper_api::*;

use crate::client::{Kiln, NodeClient};
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
    /// macOS only: the host interface VMs bridge onto (Tart's --net-bridged).
    /// On Linux the bridge comes from kiln in each NIC spec.
    #[arg(long, env = "INGOT_BRIDGE", default_value = "en0")]
    #[allow(dead_code)]
    bridge: String,
    /// CPU threads kept back for the host and anything else running on it.
    #[arg(long, env = "INGOT_RESERVE_VCPUS", default_value_t = 2)]
    reserve_vcpus: u32,
    /// Memory kept back for the host, in MB.
    #[arg(long, env = "INGOT_RESERVE_MEMORY_MB", default_value_t = 4096)]
    reserve_memory_mb: u64,
    /// Disk space kept back for the host, in GiB.
    #[arg(long, env = "INGOT_RESERVE_DISK_GB", default_value_t = 0)]
    reserve_disk_gb: u64,
}

const REPORT_EVERY: Duration = Duration::from_secs(10);
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "ingot stopped");
            if client::terminal(&e) || e.downcast_ref::<Retired>().is_some() {
                std::process::ExitCode::from(78)
            } else { std::process::ExitCode::FAILURE }
        }
    }
}

#[derive(Debug)]
struct Retired;
impl std::fmt::Display for Retired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("node retired") }
}
impl std::error::Error for Retired {}

async fn run() -> anyhow::Result<()> {
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
    let hostname = hostname();
    let arch = match std::env::consts::ARCH {
        "x86_64" => Arch::X86_64,
        "aarch64" => Arch::Aarch64,
        other => anyhow::bail!("unsupported architecture {other}"),
    };
    tracing::info!(%node_id, %hostname, kiln = %opts.kiln, "ingot starting");

    let disks = disk::Disks { dir: opts.state_dir.join("disks"), images: images_dir };
    let (images, fetches) = images::Images::new(disks.hashed_dir());
    let images = Arc::new(images);
    let hv = native_hypervisor(&opts, disks)?;
    let reserve = Capacity { vcpus: opts.reserve_vcpus, memory_mb: opts.reserve_memory_mb, disk_gb: opts.reserve_disk_gb };
    let reconciler = Arc::new(Mutex::new(Reconciler::new(hv, reserve).with_images(images.clone())));

    // Loading the cache validates it, but only a fresh authenticated response
    // authorizes resource changes. Existing VMs keep running while disconnected.
    let cached = state::load_desired(&opts.state_dir)?;
    let token = identity::load_or_create_token(&opts.state_dir)?;
    let fingerprint = copper_api::fingerprint(&sha256_hex(&token));
    let kiln = Kiln::new(&opts.kiln, node_id, token);
    // Downloads are only requested by reconciliation, which only runs on a
    // fresh authorized desired state.
    tokio::spawn(images::fetch_loop(kiln.clone(), images, fetches));
    #[cfg(not(target_os = "macos"))]
    tokio::spawn(console::console_loop(kiln.clone(), opts.libvirt.clone()));
    tracing::info!(%fingerprint, "node key; kiln shows the same when you approve");
    let register = RegisterRequest { node_id, hostname, arch, capacity: snapshot_capacity(&reconciler, cached.clone()).await };
    register_until_approved(&kiln, &register).await?;

    let (updates, rx) = tokio::sync::watch::channel(ControllerState::default());
    let (failures, errors) = tokio::sync::mpsc::channel(1);
    tokio::select! {
        result = desired_loop(kiln.clone(), opts.state_dir, cached, register, updates, errors) => result,
        result = reconcile_loop(kiln, rx, failures, reconciler) => result,
    }
}

#[cfg(not(target_os = "macos"))]
fn native_hypervisor(opts: &Opts, disks: disk::Disks) -> anyhow::Result<hv::Native> {
    libvirt::Libvirt::connect(&opts.libvirt, disks)
}

#[cfg(target_os = "macos")]
fn native_hypervisor(opts: &Opts, disks: disk::Disks) -> anyhow::Result<hv::Native> {
    let _ = &opts.libvirt; // meaningless on macOS; accepted so one env file works everywhere
    tart::Tart::new(disks, opts.bridge.clone())
}

fn sha256_hex(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).split('.').next().unwrap_or_default().to_string()
}

async fn snapshot_capacity(reconciler: &Arc<Mutex<Reconciler>>, cached: Option<DesiredNodeState>) -> Capacity {
    let r = reconciler.clone();
    tokio::task::spawn_blocking(move || {
        let r = r.lock().unwrap();
        cached.as_ref().map_or_else(|| r.capacity(), |inventory| r.capacity_for(inventory))
    }).await.unwrap()
}

async fn register_until_approved(kiln: &impl NodeClient, req: &RegisterRequest) -> anyhow::Result<()> {
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
            Ok(RegistrationStatus::Retired) => return Err(Retired.into()),
            Err(e) if client::terminal(&e) => return Err(e.context("node credential rejected")),
            Err(e) => tracing::warn!(error = %e, "kiln unreachable; retrying"),
        }
        tokio::time::sleep(RETRY_BACKOFF).await;
    }
}

/// Only the polling task publishes connection state. Revisions identify
/// individual responses, including repeated responses for the same generation.
#[derive(Clone, Default)]
struct ControllerState {
    revision: u64,
    desired: Option<DesiredNodeState>,
}

struct ReportFailure {
    revision: u64,
    error: anyhow::Error,
}

/// Polling stays independent of reconciliation and its retry timer.
async fn desired_loop(
    kiln: impl NodeClient,
    state_dir: PathBuf,
    mut cached: Option<DesiredNodeState>,
    registration: RegisterRequest,
    updates: tokio::sync::watch::Sender<ControllerState>,
    mut failures: tokio::sync::mpsc::Receiver<ReportFailure>,
) -> anyhow::Result<()> {
    // Always fetch immediately at startup, including after a controller restore.
    let mut after = None;
    let mut revision = 0;
    loop {
        let response = tokio::select! {
            biased;
            failure = failures.recv() => {
                let failure = failure.context("reconciliation worker stopped")?;
                // A newer desired response already restored connectivity. A
                // failure from an older report cannot revoke that response.
                if failure.revision != revision { continue }
                Err(failure.error)
            }
            response = kiln.desired(after) => response,
        };
        revision += 1;
        match response {
            Ok(d) => {
                if cached.as_ref() != Some(&d) {
                    state::save_desired(&state_dir, &d)?;
                    tracing::info!(generation = d.generation, instances = d.instances.len(), "new desired state");
                    cached = Some(d.clone());
                }
                after = Some(d.generation);
                updates.send_replace(ControllerState { revision, desired: Some(d) });
            }
            Err(e) => {
                updates.send_replace(ControllerState { revision, desired: None });
                after = None;
                if client::terminal(&e) { return Err(e.context("node no longer authorized; reconciliation stopped")) }
                if client::pending(&e) {
                    tracing::warn!("node pending; returning to registration");
                    register_until_approved(&kiln, &registration).await?;
                } else {
                    tracing::warn!(error = %e, "controller request failed; preserving existing workloads");
                    tokio::time::sleep(RETRY_BACKOFF).await;
                }
            }
        }
    }
}

async fn reconcile_loop<H: hv::Hypervisor + Send + 'static>(
    kiln: impl NodeClient,
    mut rx: tokio::sync::watch::Receiver<ControllerState>,
    failures: tokio::sync::mpsc::Sender<ReportFailure>,
    reconciler: Arc<Mutex<Reconciler<H>>>,
) -> anyhow::Result<()> {
    let mut timer = tokio::time::interval(REPORT_EVERY);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ready = true;
    loop {
        if !ready {
            tokio::select! {
                _ = timer.tick() => {},
                result = rx.changed() => { result?; },
            }
        }
        ready = false;
        let controller = rx.borrow_and_update().clone();
        let Some(report) = reconcile_snapshot(reconciler.clone(), controller.desired).await? else { continue };
        if let Err(error) = kiln.observed(&report).await {
            failures.send(ReportFailure { revision: controller.revision, error }).await?;
            // Wait for the polling task to either suspend us or supply a newer
            // response. Never continue acting on the failed report's snapshot.
            rx.changed().await?;
            ready = true;
        }
    }
}

async fn reconcile_snapshot<H: hv::Hypervisor + Send + 'static>(
    reconciler: Arc<Mutex<Reconciler<H>>>,
    authorized: Option<DesiredNodeState>,
) -> anyhow::Result<Option<ObservedNodeState>> {
    let Some(desired) = authorized else { return Ok(None) };
    Ok(Some(tokio::task::spawn_blocking(move || {
        let mut r = reconciler.lock().unwrap();
        r.reconcile(&desired);
        r.observe()
    }).await?))
}
