//! User-facing API: projects, instances, and admin-only node management.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use copper_api::*;
use rand::RngCore;

use crate::auth::Auth;
use crate::db::{self, InstanceRow, NodeDbStatus, Project};
use crate::error::{Error, Result};
use crate::{placement, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/me", get(me))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route("/v1/projects/{project}/instances", get(list_instances).post(create_instance))
        .route("/v1/projects/{project}/instances/{name}", get(get_instance).delete(delete_instance))
        .route("/v1/projects/{project}/instances/{name}/start", post(start_instance))
        .route("/v1/projects/{project}/instances/{name}/stop", post(stop_instance))
        .route("/v1/projects/{project}/instances/{name}/resize", post(resize_instance))
        .route("/v1/instances/{id}/forget", post(forget_instance))
        .route("/v1/images", get(list_images))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/nodes/{id}/approve", post(approve_node))
        .route("/v1/nodes/{id}/retire", post(retire_node))
        .route("/v1/nodes/{id}/policy", post(set_node_policy))
}

#[derive(serde::Serialize)]
struct Me {
    name: String,
    role: Role,
}

async fn me(Auth(user): Auth) -> Json<Me> {
    Json(Me { name: user.name, role: user.role })
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
    let projects = if auth.0.role == Role::Admin {
        db::list_all_projects(&conn)?
    } else {
        db::list_projects(&conn, auth.0.id)?.into_iter().map(|p| (p, auth.0.name.clone())).collect()
    };
    Ok(Json(projects.into_iter().map(|(p, owner)| ProjectOut { name: p.name, owner }).collect()))
}

async fn create_project(
    State(state): State<AppState>,
    auth: Auth,
    Json(req): Json<CreateProject>,
) -> Result<(StatusCode, Json<ProjectOut>)> {
    validate_name(&req.name)?;
    let conn = state.db.lock().unwrap();
    if db::project_by_name(&conn, auth.0.id, &req.name)?.is_some() {
        return Err(Error::conflict("project already exists"));
    }
    let p = db::create_project(&conn, auth.0.id, &req.name)?;
    Ok((StatusCode::CREATED, Json(ProjectOut { name: p.name, owner: auth.0.name })))
}

fn project(conn: &rusqlite::Connection, auth: &Auth, name: &str) -> Result<Project> {
    let (owner, name) = if let Some((owner, name)) = name.split_once(':') {
        if owner != auth.0.name && auth.0.role != Role::Admin { return Err(Error::forbidden()) }
        let user = db::user_by_name(conn, owner)?.ok_or_else(|| Error::not_found("project"))?;
        (user.id, name)
    } else { (auth.0.id, name) };
    db::project_by_name(conn, owner, name)?.ok_or_else(|| Error::not_found("project"))
}

// ---- instances -------------------------------------------------------------

fn to_api(conn: &rusqlite::Connection, row: InstanceRow, project: &str, admin: bool) -> Result<Instance> {
    let node_online = db::node(conn, row.node_id)?.is_some_and(|n| n.online());
    let observed = row.observed.as_ref();
    Ok(Instance {
        id: row.id,
        project: project.to_string(),
        name: row.name,
        state: InstanceState::derive(row.desired, observed.map(|o| o.state), node_online),
        vcpus: row.spec.vcpus,
        memory_mb: row.spec.memory_mb,
        arch: row.spec.arch,
        disk_gb: row.spec.boot_disk.size_gb,
        addresses: observed.map(|o| o.addresses.clone()).unwrap_or_default(),
        error: observed.and_then(|o| o.error.clone()),
        node: admin.then_some(row.node_id),
        spread: if admin { row.spread } else { None },
    })
}

async fn list_instances(State(state): State<AppState>, auth: Auth, Path(project): Path<String>) -> Result<Json<Vec<Instance>>> {
    let conn = state.db.lock().unwrap();
    let p = self::project(&conn, &auth, &project)?;
    let admin = auth.0.role == Role::Admin;
    db::list_instances_in_project(&conn, p.id)?
        .into_iter()
        .map(|r| to_api(&conn, r, &project, admin))
        .collect::<Result<Vec<_>>>()
        .map(Json)
}

async fn create_instance(
    State(state): State<AppState>,
    auth: Auth,
    Path(project): Path<String>,
    Json(req): Json<CreateInstance>,
) -> Result<(StatusCode, Json<Instance>)> {
    validate_name(&req.name)?;
    if req.vcpus == 0 || req.memory_mb < 128 || req.disk_gb == 0 {
        return Err(Error::bad_request("vcpus, memory_mb and disk_gb must be positive (memory at least 128 MB)"));
    }
    if !valid_image_name(&req.image) {
        return Err(Error::bad_request("image must be a filename stem using lowercase letters, digits, '.', '_' or '-', starting with a letter or digit"));
    }
    let admin = auth.0.role == Role::Admin;
    if (req.node.is_some() || req.spread.is_some()) && !admin {
        return Err(Error(StatusCode::FORBIDDEN, "placement hints are admin-only".into()));
    }
    if let Some(group) = &req.spread { validate_name(group)?; }
    let conn = state.db.lock().unwrap();
    let p = self::project(&conn, &auth, &project)?;
    if db::instance_by_name(&conn, p.id, &req.name)?.is_some() {
        return Err(Error::conflict("instance already exists"));
    }
    if let Some(node) = req.node {
        db::node(&conn, node)?.ok_or_else(|| Error::not_found("node"))?;
    }
    // The catalogue binds the name to a blob now, so the instance keeps that
    // exact image even if the name is re-imported later. Without an entry the
    // node must hold `<image>.img` itself.
    let sha256 = db::image(&conn, &req.image, req.arch)?.map(|i| i.sha256);
    let spec = InstanceSpec {
        vcpus: req.vcpus,
        memory_mb: req.memory_mb,
        arch: req.arch,
        boot_disk: BootDisk { image: req.image.clone(), sha256, size_gb: req.disk_gb },
        nics: vec![Nic { bridge: state.bridge.clone(), mac: random_mac() }],
        ssh_keys: req.ssh_keys.clone(),
    };
    // Placement and insert are one transaction so two creates can't both land
    // on the last slot of a node.
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    let hints = placement::Hints { node: req.node, spread: req.spread.as_deref().map(|g| (p.id, g)) };
    let node = placement::choose(&tx, &spec, &hints)?;
    let row = InstanceRow {
        id: InstanceId::new_v4(),
        project_id: p.id,
        name: req.name,
        node_id: node.id,
        desired: DesiredState::Running,
        spec,
        observed: None,
        spread: req.spread,
    };
    db::insert_instance(&tx, &row)?;
    db::bump_generation(&tx, node.id)?;
    tx.commit()?;
    state.notify_changed();
    tracing::info!(instance = %row.id, name = %row.name, node = %row.node_id, "instance created");
    Ok((StatusCode::CREATED, Json(to_api(&conn, row, &project, admin)?)))
}

/// Resize a stopped instance in place. It never moves: the new size must fit
/// on its node once its current allocation is released, or the request is
/// refused. Disks only grow; the guest grows its filesystem on next boot.
async fn resize_instance(
    State(state): State<AppState>,
    auth: Auth,
    Path((project, name)): Path<(String, String)>,
    Json(req): Json<ResizeInstance>,
) -> Result<Json<Instance>> {
    let conn = state.db.lock().unwrap();
    let row = load(&conn, &auth, &project, &name)?;
    let admin = auth.0.role == Role::Admin;
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
        if !placement::fits_in_place(&tx, &node, row.id, &spec)? {
            return Err(Error::conflict("the instance's node lacks capacity for the new size"));
        }
        db::set_spec(&tx, row.id, &spec)?;
        db::bump_generation(&tx, row.node_id)?;
        tx.commit()?;
        state.notify_changed();
        tracing::info!(instance = %row.id, vcpus = spec.vcpus, memory_mb = spec.memory_mb, disk_gb, "instance resized");
    }
    let row = db::instance(&conn, row.id)?.ok_or_else(|| Error::not_found("instance"))?;
    Ok(Json(to_api(&conn, row, &project, admin)?))
}

async fn list_images(State(state): State<AppState>, _auth: Auth) -> Result<Json<Vec<Image>>> {
    let conn = state.db.lock().unwrap();
    Ok(Json(db::list_images(&conn)?))
}

fn load(conn: &rusqlite::Connection, auth: &Auth, project: &str, name: &str) -> Result<InstanceRow> {
    let p = self::project(conn, auth, project)?;
    db::instance_by_name(conn, p.id, name)?.ok_or_else(|| Error::not_found("instance"))
}

async fn get_instance(
    State(state): State<AppState>,
    auth: Auth,
    Path((project, name)): Path<(String, String)>,
) -> Result<Json<Instance>> {
    let conn = state.db.lock().unwrap();
    let row = load(&conn, &auth, &project, &name)?;
    Ok(Json(to_api(&conn, row, &project, auth.0.role == Role::Admin)?))
}

fn set_desired(state: &AppState, auth: &Auth, project: &str, name: &str, desired: DesiredState) -> Result<Instance> {
    let conn = state.db.lock().unwrap();
    let row = load(&conn, auth, project, name)?;
    if row.desired == DesiredState::Absent {
        return Err(Error::conflict("instance is being deleted"));
    }
    if row.desired != desired {
        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        db::set_desired(&tx, row.id, desired)?;
        db::bump_generation(&tx, row.node_id)?;
        tx.commit()?;
        state.notify_changed();
        tracing::info!(instance = %row.id, ?desired, "desired state changed");
    }
    let row = db::instance(&conn, row.id)?.ok_or_else(|| Error::not_found("instance"))?;
    to_api(&conn, row, project, auth.0.role == Role::Admin)
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
        tx.commit()?;
        state.notify_changed();
        tracing::info!(instance = %row.id, "instance delete requested");
    }
    Ok(StatusCode::ACCEPTED)
}

// ---- nodes (admin) ---------------------------------------------------------

fn node_api(conn: &rusqlite::Connection, node: &db::NodeRow) -> Result<Node> {
    let instances = db::list_instances_on_node(conn, node.id)?;
    Ok(node.to_api(placement::allocated(instances.iter())))
}

async fn list_nodes(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Node>>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    db::list_nodes(&conn)?.iter().map(|n| node_api(&conn, n)).collect::<Result<Vec<_>>>().map(Json)
}

/// Overcommit is admin policy, per node. Lowering a ratio below what is
/// already allocated is allowed: nothing is evicted, new placements stop.
async fn set_node_policy(State(state): State<AppState>, auth: Auth, Path(id): Path<NodeId>, Json(req): Json<NodePolicy>) -> Result<Json<Node>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let node = db::node(&conn, id)?.ok_or_else(|| Error::not_found("node"))?;
    let oc = Overcommit {
        cpu: req.cpu_ratio.unwrap_or(node.overcommit.cpu),
        memory: req.memory_ratio.unwrap_or(node.overcommit.memory),
        disk: req.disk_ratio.unwrap_or(node.overcommit.disk),
    };
    if !oc.valid() {
        return Err(Error::bad_request(format!("ratios must be greater than 0 and at most {}", Overcommit::MAX)));
    }
    db::set_overcommit(&conn, id, &oc)?;
    tracing::info!(node = %id, cpu = oc.cpu, memory = oc.memory, disk = oc.disk, "overcommit changed");
    node_api(&conn, &db::node(&conn, id)?.unwrap()).map(Json)
}

async fn approve_node(State(state): State<AppState>, auth: Auth, Path(id): Path<NodeId>) -> Result<Json<Node>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let node = db::node(&conn, id)?.ok_or_else(|| Error::not_found("node"))?;
    if node.status == NodeDbStatus::Retired {
        return Err(Error::conflict("node is retired"));
    }
    if node.token_hash.is_none() { return Err(Error::conflict("node must register its credential before approval")) }
    db::set_node_status(&conn, id, NodeDbStatus::Approved)?;
    state.notify_changed();
    tracing::info!(node = %id, host = %node.hostname, "node approved");
    node_api(&conn, &db::node(&conn, id)?.unwrap()).map(Json)
}

/// Retiring a node marks every instance on it failed. Nothing is re-created.
async fn retire_node(State(state): State<AppState>, auth: Auth, Path(id): Path<NodeId>) -> Result<Json<Node>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let node = db::node(&conn, id)?.ok_or_else(|| Error::not_found("node"))?;
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    db::set_node_status(&tx, id, NodeDbStatus::Retired)?;
    db::fail_instances_on_node(&tx, id, "node retired")?;
    tx.commit()?;
    state.notify_changed();
    tracing::info!(node = %id, host = %node.hostname, "node retired");
    node_api(&conn, &db::node(&conn, id)?.unwrap()).map(Json)
}

// ---- helpers ---------------------------------------------------------------

fn validate_name(name: &str) -> Result<()> {
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

/// Forget tracking only after retirement; this never claims disks were erased.
async fn forget_instance(State(state): State<AppState>, auth: Auth, Path(id): Path<InstanceId>) -> Result<StatusCode> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    let row = db::instance(&tx, id)?.ok_or_else(|| Error::not_found("instance"))?;
    let node = db::node(&tx, row.node_id)?.ok_or_else(|| Error::not_found("node"))?;
    if node.status != NodeDbStatus::Retired { return Err(Error::conflict("retire the node before forgetting its instances")) }
    db::delete_instance(&tx, id)?;
    db::bump_generation(&tx, row.node_id)?;
    tx.commit()?;
    state.notify_changed();
    tracing::warn!(instance = %id, node = %row.node_id, "instance forgotten; host resources require manual cleanup");
    Ok(StatusCode::NO_CONTENT)
}
