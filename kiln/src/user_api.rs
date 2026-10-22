//! Projects and instances: what users create, start, stop, resize and reach.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use copper_api::*;
use rand::RngCore;

use crate::auth::Auth;
use crate::db::{self, InstanceRow, NewEvent, NodeDbStatus, Project};
use crate::error::{Error, Result};
use crate::metrics::{Metrics, Subject};
use crate::{placement, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/projects", get(list_projects).post(create_project))
        .route("/v1/projects/{project}/instances", get(list_instances).post(create_instance))
        .route("/v1/projects/{project}/instances/check", post(check_instance))
        .route("/v1/projects/{project}/instances/{name}", get(get_instance).delete(delete_instance))
        .route("/v1/projects/{project}/instances/{name}/start", post(start_instance))
        .route("/v1/projects/{project}/instances/{name}/stop", post(stop_instance))
        .route("/v1/projects/{project}/instances/{name}/resize", post(resize_instance))
        .route("/v1/projects/{project}/instances/{name}/ssh", get(ssh_tunnel))
        .route("/v1/instances/{id}/forget", post(forget_instance))
}

// ---- projects --------------------------------------------------------------

#[derive(serde::Deserialize)]
struct CreateProject {
    name: String,
}

#[derive(serde::Serialize)]
struct ProjectOut {
    name: String,
    owner: String,
}

async fn list_projects(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<ProjectOut>>> {
    let conn = state.db.lock().unwrap();
    let projects = if auth.is_admin() {
        db::list_all_projects(&conn)?
    } else {
        db::list_projects(&conn, auth.0.id)?.into_iter().map(|p| (p, auth.0.name.clone())).collect()
    };
    Ok(Json(projects.into_iter().map(|(p, owner)| ProjectOut { name: p.name, owner }).collect()))
}

async fn create_project(State(state): State<AppState>, auth: Auth, Json(req): Json<CreateProject>) -> Result<(StatusCode, Json<ProjectOut>)> {
    validate_name(&req.name)?;
    let conn = state.db.lock().unwrap();
    if db::project_by_name(&conn, auth.0.id, &req.name)?.is_some() {
        return Err(Error::conflict("project already exists"));
    }
    let p = db::create_project(&conn, auth.0.id, &req.name)?;
    Ok((StatusCode::CREATED, Json(ProjectOut { name: p.name, owner: auth.0.name })))
}

/// A project as addressed in a URL: `name` for your own, `owner:name` for an
/// admin looking at someone else's.
pub struct ProjectRef {
    pub project: Project,
    pub owner: String,
    pub path: String,
}

pub fn project(conn: &rusqlite::Connection, auth: &Auth, path: &str) -> Result<ProjectRef> {
    let (owner, name) = match path.split_once(':') {
        Some((owner, name)) => {
            if owner != auth.0.name && !auth.is_admin() {
                return Err(Error::forbidden());
            }
            (db::user_by_name(conn, owner)?.ok_or_else(|| Error::not_found("project"))?, name)
        }
        None => (auth.0.clone(), path),
    };
    let project = db::project_by_name(conn, owner.id, name)?.ok_or_else(|| Error::not_found("project"))?;
    Ok(ProjectRef { project, owner: owner.name, path: path.to_string() })
}

// ---- instances -------------------------------------------------------------

pub fn to_api(conn: &rusqlite::Connection, metrics: &Metrics, row: InstanceRow, project: &ProjectRef, admin: bool) -> Result<Instance> {
    let node = db::node(conn, row.node_id)?;
    let node_online = node.as_ref().is_some_and(|n| n.online());
    let observed = row.observed.as_ref();
    let state = InstanceState::derive(row.desired, observed.map(|o| o.state), node_online);
    let running = state == InstanceState::Running;
    Ok(Instance {
        id: row.id,
        project: project.path.clone(),
        name: row.name,
        state,
        vcpus: row.spec.vcpus,
        memory_mb: row.spec.memory_mb,
        arch: row.spec.arch,
        disk_gb: row.spec.boot_disk.size_gb,
        addresses: observed.map(|o| o.addresses.clone()).unwrap_or_default(),
        error: observed.and_then(|o| o.error.clone()),
        node: admin.then_some(row.node_id),
        spread: if admin { row.spread } else { None },
        node_name: if admin { node.map(|n| n.hostname) } else { None },
        image: row.spec.boot_disk.image.clone(),
        created_at: row.created_at,
        owner: project.owner.clone(),
        running_since: row.running_since.filter(|_| running),
        usage: metrics.latest(Subject::Instance(row.id)).filter(|_| running),
    })
}

pub fn load(conn: &rusqlite::Connection, auth: &Auth, project: &str, name: &str) -> Result<InstanceRow> {
    let p = self::project(conn, auth, project)?;
    db::instance_by_name(conn, p.project.id, name)?.ok_or_else(|| Error::not_found("instance"))
}

async fn list_instances(State(state): State<AppState>, auth: Auth, Path(project): Path<String>) -> Result<Json<Vec<Instance>>> {
    let conn = state.db.lock().unwrap();
    let p = self::project(&conn, &auth, &project)?;
    db::list_instances_in_project(&conn, p.project.id)?
        .into_iter()
        .map(|r| to_api(&conn, &state.metrics, r, &p, auth.is_admin()))
        .collect::<Result<Vec<_>>>()
        .map(Json)
}

async fn get_instance(State(state): State<AppState>, auth: Auth, Path((project, name)): Path<(String, String)>) -> Result<Json<Instance>> {
    let conn = state.db.lock().unwrap();
    let p = self::project(&conn, &auth, &project)?;
    let row = db::instance_by_name(&conn, p.project.id, &name)?.ok_or_else(|| Error::not_found("instance"))?;
    Ok(Json(to_api(&conn, &state.metrics, row, &p, auth.is_admin())?))
}

/// Validate a create request and turn it into a spec and placement hints.
fn spec_for(conn: &rusqlite::Connection, state: &AppState, auth: &Auth, req: &CreateInstance) -> Result<InstanceSpec> {
    validate_name(&req.name)?;
    if req.vcpus == 0 || req.memory_mb < 128 || req.disk_gb == 0 {
        return Err(Error::bad_request("vcpus, memory_mb and disk_gb must be positive (memory at least 128 MB)"));
    }
    if !valid_image_name(&req.image) {
        return Err(Error::bad_request("image must be a filename stem using lowercase letters, digits, '.', '_' or '-', starting with a letter or digit"));
    }
    if (req.node.is_some() || req.spread.is_some()) && !auth.is_admin() {
        return Err(Error(StatusCode::FORBIDDEN, "placement hints are admin-only".into()));
    }
    if let Some(group) = &req.spread {
        validate_name(group)?;
    }
    validate_ssh_keys(&req.ssh_keys)?;
    if let Some(node) = req.node {
        db::node(conn, node)?.ok_or_else(|| Error::not_found("node"))?;
    }
    // The catalogue binds the name to a blob now, so the instance keeps that
    // exact image even if the name is re-imported later. Without an entry the
    // node must hold `<image>.img` itself.
    let sha256 = db::image(conn, &req.image, req.arch)?.map(|i| i.sha256);
    Ok(InstanceSpec {
        vcpus: req.vcpus,
        memory_mb: req.memory_mb,
        arch: req.arch,
        boot_disk: BootDisk { image: req.image.clone(), sha256, size_gb: req.disk_gb },
        nics: vec![Nic { bridge: state.bridge.clone(), mac: random_mac() }],
        ssh_keys: req.ssh_keys.clone(),
    })
}

/// Keys as `ssh-keygen` writes them to `~/.ssh/id_*.pub`: one line, a key
/// type, then base64. Anything else, a private key above all, never reaches
/// a node.
fn validate_ssh_keys(keys: &[String]) -> Result<()> {
    let public = |k: &str| {
        let mut parts = k.split_whitespace();
        let (Some(kind), Some(blob)) = (parts.next(), parts.next()) else { return false };
        let known = ["ssh-", "ecdsa-sha2-", "sk-ssh-", "sk-ecdsa-"].iter().any(|p| kind.starts_with(p));
        !k.contains(['\n', '\r']) && known && blob.len() >= 16 && blob.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
    };
    match keys.iter().find(|k| !public(k)) {
        Some(_) => Err(Error::bad_request("ssh keys must be OpenSSH public keys, one line each, like ~/.ssh/id_ed25519.pub")),
        None => Ok(()),
    }
}

/// The dry run behind the console's "won't fit" panel. It needs no ssh key,
/// so the console can check a shape before one is pasted.
async fn check_instance(State(state): State<AppState>, auth: Auth, Path(project): Path<String>, Json(req): Json<CreateInstance>) -> Result<Json<FitCheck>> {
    let conn = state.db.lock().unwrap();
    let p = self::project(&conn, &auth, &project)?;
    let spec = spec_for(&conn, &state, &auth, &req)?;
    let hints = placement::Hints { node: req.node, spread: req.spread.as_deref().map(|g| (p.project.id, g)) };
    Ok(Json(placement::check(&conn, &p.project, &spec, &hints, auth.is_admin())?))
}

async fn create_instance(State(state): State<AppState>, auth: Auth, Path(project): Path<String>, Json(req): Json<CreateInstance>) -> Result<(StatusCode, Json<Instance>)> {
    let conn = state.db.lock().unwrap();
    let p = self::project(&conn, &auth, &project)?;
    let spec = spec_for(&conn, &state, &auth, &req)?;
    // Keys are only written at first boot and the images have no password, so
    // a VM made without one could never be logged into.
    if spec.ssh_keys.is_empty() {
        return Err(Error::bad_request("a vm needs at least one ssh key; without one nobody can ever log in"));
    }
    if db::instance_by_name(&conn, p.project.id, &req.name)?.is_some() {
        return Err(Error::conflict("instance already exists"));
    }
    // Placement and insert are one transaction so two creates can't both land
    // on the last slot of a node.
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    if let Some(reason) = placement::quota_refusal(&tx, &p.project, &spec, None)? {
        return Err(Error::conflict(reason));
    }
    let hints = placement::Hints { node: req.node, spread: req.spread.as_deref().map(|g| (p.project.id, g)) };
    let node = placement::choose(&tx, &spec, &hints)?;
    let row = InstanceRow {
        id: InstanceId::new_v4(),
        project_id: p.project.id,
        name: req.name.clone(),
        node_id: node.id,
        desired: DesiredState::Running,
        spec,
        observed: None,
        spread: req.spread.clone(),
        created_at: db::now(),
        running_since: None,
    };
    db::insert_instance(&tx, &row)?;
    db::bump_generation(&tx, node.id)?;
    let detail = format!("{} vcpu · {} MB · {} GB · {}", row.spec.vcpus, row.spec.memory_mb, row.spec.boot_disk.size_gb, row.spec.boot_disk.image);
    db::record(&tx, NewEvent::new("created", detail).instance(row.project_id, row.id, &row.name).node(node.id).by(&auth.0.name))?;
    tx.commit()?;
    state.notify_changed();
    tracing::info!(instance = %row.id, name = %row.name, node = %row.node_id, "instance created");
    Ok((StatusCode::CREATED, Json(to_api(&conn, &state.metrics, row, &p, auth.is_admin())?)))
}

/// Resize a stopped instance in place. It never moves: the new size must fit
/// on its node once its current allocation is released, or the request is
/// refused. Disks only grow; the guest grows its filesystem on next boot.
async fn resize_instance(State(state): State<AppState>, auth: Auth, Path((project, name)): Path<(String, String)>, Json(req): Json<ResizeInstance>) -> Result<Json<Instance>> {
    let conn = state.db.lock().unwrap();
    let p = self::project(&conn, &auth, &project)?;
    let row = db::instance_by_name(&conn, p.project.id, &name)?.ok_or_else(|| Error::not_found("instance"))?;
    let node = db::node(&conn, row.node_id)?.ok_or_else(|| Error::not_found("node"))?;
    let current = InstanceState::derive(row.desired, row.observed.as_ref().map(|o| o.state), node.online());
    if row.desired != DesiredState::Stopped || current != InstanceState::Stopped {
        return Err(Error::conflict("stop the instance and wait until it is stopped before resizing"));
    }
    let mut spec = row.spec.clone();
    spec.vcpus = req.vcpus.unwrap_or(spec.vcpus);
    spec.memory_mb = req.memory_mb.unwrap_or(spec.memory_mb);
    let disk_gb = req.disk_gb.unwrap_or(spec.boot_disk.size_gb);
    if spec.vcpus == 0 || spec.memory_mb < 128 {
        return Err(Error::bad_request("vcpus must be positive and memory at least 128 MB"));
    }
    if disk_gb < spec.boot_disk.size_gb {
        return Err(Error::bad_request("disks only grow; shrinking would destroy data"));
    }
    spec.boot_disk.size_gb = disk_gb;
    if spec != row.spec {
        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        if let Some(reason) = placement::quota_refusal(&tx, &p.project, &spec, Some(row.id))? {
            return Err(Error::conflict(reason));
        }
        if !placement::fits_in_place(&tx, &node, row.id, &spec)? {
            return Err(Error::conflict("the instance's node lacks capacity for the new size"));
        }
        db::set_spec(&tx, row.id, &spec)?;
        db::bump_generation(&tx, row.node_id)?;
        db::record(&tx, NewEvent::new("resized", describe_resize(&row.spec, &spec)).instance(row.project_id, row.id, &row.name).by(&auth.0.name))?;
        tx.commit()?;
        state.notify_changed();
        tracing::info!(instance = %row.id, vcpus = spec.vcpus, memory_mb = spec.memory_mb, disk_gb, "instance resized");
    }
    let row = db::instance(&conn, row.id)?.ok_or_else(|| Error::not_found("instance"))?;
    Ok(Json(to_api(&conn, &state.metrics, row, &p, auth.is_admin())?))
}

/// e.g. `vcpu 2 → 4 · disk 10 → 20 GB`, naming only what changed.
fn describe_resize(old: &InstanceSpec, new: &InstanceSpec) -> String {
    let mut parts = vec![];
    if old.vcpus != new.vcpus { parts.push(format!("vcpu {} → {}", old.vcpus, new.vcpus)) }
    if old.memory_mb != new.memory_mb { parts.push(format!("memory {} → {} MB", old.memory_mb, new.memory_mb)) }
    if old.boot_disk.size_gb != new.boot_disk.size_gb { parts.push(format!("disk {} → {} GB", old.boot_disk.size_gb, new.boot_disk.size_gb)) }
    parts.join(" · ")
}

fn set_desired(state: &AppState, auth: &Auth, project: &str, name: &str, desired: DesiredState) -> Result<Instance> {
    let conn = state.db.lock().unwrap();
    let p = self::project(&conn, auth, project)?;
    let row = db::instance_by_name(&conn, p.project.id, name)?.ok_or_else(|| Error::not_found("instance"))?;
    if row.desired == DesiredState::Absent {
        return Err(Error::conflict("instance is being deleted"));
    }
    if row.desired != desired {
        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        db::set_desired(&tx, row.id, desired)?;
        db::bump_generation(&tx, row.node_id)?;
        let (kind, detail) = match desired {
            DesiredState::Running => ("start", "asked to start"),
            _ => ("stop", "asked to stop · acpi, then off after 60 s"),
        };
        db::record(&tx, NewEvent::new(kind, detail).instance(row.project_id, row.id, &row.name).by(&auth.0.name))?;
        tx.commit()?;
        state.notify_changed();
        tracing::info!(instance = %row.id, ?desired, "desired state changed");
    }
    let row = db::instance(&conn, row.id)?.ok_or_else(|| Error::not_found("instance"))?;
    to_api(&conn, &state.metrics, row, &p, auth.is_admin())
}

async fn start_instance(State(state): State<AppState>, auth: Auth, Path((project, name)): Path<(String, String)>) -> Result<Json<Instance>> {
    Ok(Json(set_desired(&state, &auth, &project, &name, DesiredState::Running)?))
}

async fn stop_instance(State(state): State<AppState>, auth: Auth, Path((project, name)): Path<(String, String)>) -> Result<Json<Instance>> {
    Ok(Json(set_desired(&state, &auth, &project, &name, DesiredState::Stopped)?))
}

/// Delete is final. Marks the instance absent; the row disappears once the node
/// confirms the VM and its disk are gone.
async fn delete_instance(State(state): State<AppState>, auth: Auth, Path((project, name)): Path<(String, String)>) -> Result<StatusCode> {
    let conn = state.db.lock().unwrap();
    let row = load(&conn, &auth, &project, &name)?;
    if row.desired != DesiredState::Absent {
        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        db::set_desired(&tx, row.id, DesiredState::Absent)?;
        db::bump_generation(&tx, row.node_id)?;
        let detail = format!("its {} GB boot disk goes with it", row.spec.boot_disk.size_gb);
        db::record(&tx, NewEvent::new("delete", detail).instance(row.project_id, row.id, &row.name).by(&auth.0.name))?;
        tx.commit()?;
        state.notify_changed();
        tracing::info!(instance = %row.id, "instance delete requested");
    }
    Ok(StatusCode::ACCEPTED)
}

/// Forget tracking only after retirement; this never claims disks were erased.
async fn forget_instance(State(state): State<AppState>, auth: Auth, Path(id): Path<InstanceId>) -> Result<StatusCode> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    let row = db::instance(&tx, id)?.ok_or_else(|| Error::not_found("instance"))?;
    let node = db::node(&tx, row.node_id)?.ok_or_else(|| Error::not_found("node"))?;
    if node.status != NodeDbStatus::Retired {
        return Err(Error::conflict("retire the node before forgetting its instances"));
    }
    db::delete_instance(&tx, id)?;
    db::bump_generation(&tx, row.node_id)?;
    let detail = format!("tracking dropped; its disk may still be on {}", node.hostname);
    db::record(&tx, NewEvent::new("forgotten", detail).instance(row.project_id, row.id, &row.name).node(node.id).by(&auth.0.name))?;
    tx.commit()?;
    state.notify_changed();
    tracing::warn!(instance = %id, node = %row.node_id, "instance forgotten; host resources require manual cleanup");
    Ok(StatusCode::NO_CONTENT)
}

// ---- ssh -------------------------------------------------------------------

/// HTTP upgrade protocol for `cu ssh`: after `101`, the connection carries
/// raw bytes to and from the instance's SSH port.
pub const TUNNEL_PROTOCOL: &str = "copper-tcp";
const SSH_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Relay the caller to their running instance's SSH port, so `cu ssh` works
/// from anywhere cu reaches kiln, the tailnet included, without routing the
/// home LAN onto the tailnet. Only the project's owner (or an admin) gets in,
/// and SSH itself still authenticates the key.
async fn ssh_tunnel(State(state): State<AppState>, auth: Auth, Path((project, name)): Path<(String, String)>, mut req: axum::extract::Request) -> Result<axum::response::Response> {
    use axum::http::header::{CONNECTION, UPGRADE};
    if req.headers().get(UPGRADE).and_then(|v| v.to_str().ok()) != Some(TUNNEL_PROTOCOL) {
        return Err(Error(StatusCode::UPGRADE_REQUIRED, format!("this endpoint needs Upgrade: {TUNNEL_PROTOCOL}; use cu ssh")));
    }
    let (id, candidates) = {
        let conn = state.db.lock().unwrap();
        let p = self::project(&conn, &auth, &project)?;
        let row = db::instance_by_name(&conn, p.project.id, &name)?.ok_or_else(|| Error::not_found("instance"))?;
        let id = row.id;
        let inst = to_api(&conn, &state.metrics, row, &p, false)?;
        if inst.state != InstanceState::Running {
            let state = serde_json::to_string(&inst.state)?;
            return Err(Error::conflict(format!("instance is {}, not running", state.trim_matches('"'))));
        }
        let candidates: Vec<std::net::IpAddr> = inst.addresses.iter().filter_map(|a| a.parse().ok()).filter(|ip| relay_allowed(*ip)).collect();
        (id, candidates)
    };
    if candidates.is_empty() {
        return Err(Error::conflict("instance has no LAN address yet; try again once it has booted"));
    }
    // A guest can report several addresses (docker bridges, say); take the
    // first that answers on port 22.
    let mut stream = None;
    for ip in &candidates {
        let target = std::net::SocketAddr::new(*ip, 22);
        if let Ok(Ok(s)) = tokio::time::timeout(SSH_CONNECT_TIMEOUT, tokio::net::TcpStream::connect(target)).await {
            stream = Some((s, target));
            break;
        }
    }
    let Some((mut stream, target)) = stream else {
        return Err(Error(StatusCode::BAD_GATEWAY, "the instance's SSH port does not answer yet".into()));
    };
    let on_upgrade = hyper::upgrade::on(&mut req);
    let user = auth.0.name.clone();
    tracing::info!(%user, instance = %id, %target, "ssh tunnel opened");
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let mut client = hyper_util::rt::TokioIo::new(upgraded);
                let result = tokio::io::copy_bidirectional(&mut client, &mut stream).await;
                tracing::info!(%user, instance = %id, ?result, "ssh tunnel closed");
            }
            Err(e) => tracing::warn!(%user, instance = %id, error = %e, "ssh tunnel upgrade failed"),
        }
    });
    axum::response::Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(CONNECTION, "upgrade")
        .header(UPGRADE, TUNNEL_PROTOCOL)
        .body(axum::body::Body::empty())
        .map_err(|e| Error::from(anyhow::Error::from(e)))
}

/// Guest agents report addresses, so they are untrusted. Relay only to
/// private LAN addresses that aren't this host's own: nothing a bridged
/// guest couldn't reach by itself. Loopback, the tailnet and link-local
/// addresses are never targets.
pub fn relay_allowed(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    let private = match ip {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00 && !crate::trusted_network(ip),
    };
    // Binding succeeds only for an address assigned to this host.
    private && std::net::UdpSocket::bind(std::net::SocketAddr::new(ip, 0)).is_err()
}

// ---- helpers ---------------------------------------------------------------

pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok { Ok(()) } else { Err(Error::bad_request("name must be 1-63 chars of [a-z0-9-], not starting or ending with -")) }
}

/// Locally administered MAC in the QEMU/KVM range.
fn random_mac() -> String {
    let mut b = [0u8; 3];
    rand::rng().fill_bytes(&mut b);
    format!("52:54:00:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2])
}
