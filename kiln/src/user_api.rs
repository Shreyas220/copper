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
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/nodes/{id}/approve", post(approve_node))
        .route("/v1/nodes/{id}/retire", post(retire_node))
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
}

async fn list_projects(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<ProjectOut>>> {
    let conn = state.db.lock().unwrap();
    Ok(Json(db::list_projects(&conn, auth.0.id)?.into_iter().map(|p| ProjectOut { name: p.name }).collect()))
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
    Ok((StatusCode::CREATED, Json(ProjectOut { name: p.name })))
}

fn project(conn: &rusqlite::Connection, auth: &Auth, name: &str) -> Result<Project> {
    db::project_by_name(conn, auth.0.id, name)?.ok_or_else(|| Error::not_found("project"))
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
    if req.image.is_empty() {
        return Err(Error::bad_request("image is required"));
    }
    let conn = state.db.lock().unwrap();
    let p = self::project(&conn, &auth, &project)?;
    if db::instance_by_name(&conn, p.id, &req.name)?.is_some() {
        return Err(Error::conflict("instance already exists"));
    }
    let spec = InstanceSpec {
        vcpus: req.vcpus,
        memory_mb: req.memory_mb,
        arch: req.arch,
        boot_disk: BootDisk { image: req.image.clone(), size_gb: req.disk_gb },
        nics: vec![Nic { bridge: state.bridge.clone(), mac: random_mac() }],
    };
    // Placement and insert are one transaction so two creates can't both land
    // on the last slot of a node.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| -> Result<InstanceRow> {
        let node = placement::choose(&conn, &spec)?;
        let row = InstanceRow {
            id: InstanceId::new_v4(),
            project_id: p.id,
            name: req.name.clone(),
            node_id: node.id,
            desired: DesiredState::Running,
            spec,
            observed: None,
        };
        db::insert_instance(&conn, &row)?;
        db::bump_generation(&conn, node.id)?;
        Ok(row)
    })();
    match outcome {
        Ok(row) => {
            conn.execute_batch("COMMIT")?;
            state.notify_changed();
            tracing::info!(instance = %row.id, name = %row.name, node = %row.node_id, "instance created");
            let admin = auth.0.role == Role::Admin;
            Ok((StatusCode::CREATED, Json(to_api(&conn, row, &project, admin)?)))
        }
        Err(e) => {
            conn.execute_batch("ROLLBACK")?;
            Err(e)
        }
    }
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
        db::set_desired(&conn, row.id, desired)?;
        db::bump_generation(&conn, row.node_id)?;
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
        db::set_desired(&conn, row.id, DesiredState::Absent)?;
        db::bump_generation(&conn, row.node_id)?;
        state.notify_changed();
        tracing::info!(instance = %row.id, "instance delete requested");
    }
    Ok(StatusCode::ACCEPTED)
}

// ---- nodes (admin) ---------------------------------------------------------

async fn list_nodes(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Node>>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    Ok(Json(db::list_nodes(&conn)?.iter().map(|n| n.to_api()).collect()))
}

async fn approve_node(State(state): State<AppState>, auth: Auth, Path(id): Path<NodeId>) -> Result<Json<Node>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let node = db::node(&conn, id)?.ok_or_else(|| Error::not_found("node"))?;
    if node.status == NodeDbStatus::Retired {
        return Err(Error::conflict("node is retired"));
    }
    db::set_node_status(&conn, id, NodeDbStatus::Approved)?;
    tracing::info!(node = %id, host = %node.hostname, "node approved");
    Ok(Json(db::node(&conn, id)?.unwrap().to_api()))
}

/// Retiring a node marks every instance on it failed. Nothing is re-created.
async fn retire_node(State(state): State<AppState>, auth: Auth, Path(id): Path<NodeId>) -> Result<Json<Node>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let node = db::node(&conn, id)?.ok_or_else(|| Error::not_found("node"))?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    db::set_node_status(&conn, id, NodeDbStatus::Retired)?;
    db::fail_instances_on_node(&conn, id, "node retired")?;
    conn.execute_batch("COMMIT")?;
    tracing::info!(node = %id, host = %node.hostname, "node retired");
    Ok(Json(db::node(&conn, id)?.unwrap().to_api()))
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
