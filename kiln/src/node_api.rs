//! Kiln <-> ingot protocol. See docs/design.md.
//!
//! Agents pin a credential at registration and must be approved by an admin.
//! Subsequent calls authenticate that credential, independently of node UUIDs.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum::http::{HeaderMap, StatusCode};
use copper_api::*;
use serde::Deserialize;

use crate::db::{self, NodeDbStatus, RegisterNodeOutcome};
use crate::auth::bearer;
use crate::error::{Error, Result};
use crate::metrics::Subject;
use crate::AppState;

/// How long a desired-state long-poll waits before returning unchanged.
const LONG_POLL: Duration = Duration::from_secs(25);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/nodes/register", post(register))
        .route("/nodes/{id}/desired", get(desired))
        .route("/nodes/{id}/observed", post(observed))
        .route("/nodes/{id}/images/{sha256}", get(image))
}

/// Stream a catalogue blob to an approved node. Nodes verify the hash
/// themselves; kiln only serves blobs something still refers to.
async fn image(
    State(state): State<AppState>,
    Path((id, sha256)): Path<(NodeId, String)>,
    headers: HeaderMap,
) -> Result<axum::response::Response> {
    use tokio::io::AsyncReadExt;
    {
        let conn = state.db.lock().unwrap();
        authenticate(&conn, id, &headers)?;
        if !valid_sha256(&sha256) || !db::blob_referenced(&conn, &sha256)? {
            return Err(Error::not_found("image"));
        }
    }
    let path = state.catalog.join(format!("{sha256}.img"));
    let file = tokio::fs::File::open(&path).await.map_err(|e| {
        tracing::error!(file = %path.display(), error = %e, "catalogue blob missing");
        Error::not_found("image")
    })?;
    let len = file.metadata().await.map_err(|e| Error::from(anyhow::Error::from(e)))?.len();
    // One read per chunk; an error ends the stream so the node sees a short body.
    let body = futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut buf = vec![0u8; 1 << 20];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(axum::body::Bytes::from(buf)), Some(file)))
            }
            Err(e) => Some((Err(e), None)),
        }
    });
    tracing::info!(node = %id, image = %sha256, bytes = len, "serving image");
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
        .header(axum::http::header::CONTENT_LENGTH, len)
        .body(axum::body::Body::from_stream(body))
        .map_err(|e| Error::from(anyhow::Error::from(e)))?)
}

async fn register(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<RegisterRequest>) -> Result<Json<RegisterResponse>> {
    let conn = state.db.lock().unwrap();
    let token = bearer(&headers)?;
    if !valid_node_token(token) { return Err(Error::unauthorized()) }
    let first_time = db::node(&conn, req.node_id)?.is_none();
    let token_hash = crate::auth::hash(token);
    let status = match db::register_node(&conn, &req, &token_hash)? {
        RegisterNodeOutcome::Registered(NodeDbStatus::Pending) => RegistrationStatus::Pending,
        RegisterNodeOutcome::Registered(NodeDbStatus::Approved) => RegistrationStatus::Approved,
        RegisterNodeOutcome::Registered(NodeDbStatus::Retired) => RegistrationStatus::Retired,
        RegisterNodeOutcome::CredentialMismatch => return Err(Error::unauthorized()),
    };
    tracing::info!(node = %req.node_id, host = %req.hostname, ?status, "node registered");
    if first_time {
        let detail = format!("{} wants in · key {}", req.hostname, fingerprint(&token_hash));
        db::record(&conn, db::NewEvent::new("node wants in", detail).node(req.node_id))?;
    }
    Ok(Json(RegisterResponse { status }))
}

#[derive(Deserialize)]
struct DesiredQuery {
    /// Long-poll: block until generation > after, or the timeout passes.
    after: Option<Generation>,
}

fn read_desired(state: &AppState, id: NodeId, headers: &HeaderMap) -> Result<DesiredNodeState> {
    let conn = state.db.lock().unwrap();
    let node = authenticate(&conn, id, headers)?;
    let network = node.subnet.map(|n| state.vm_net.node(n));
    let mut map = db::network_map(&conn, id)?;
    map.peers = db::mesh_peers(&conn, &state.vm_net, id)?;
    // Every project with a VM here is in the map.
    let project = |p| map.projects.iter().find(|n| n.id == p).map(|n| n.name.clone()).unwrap_or_default();
    let instances = db::list_instances_on_node(&conn, id)?
        .into_iter()
        .map(|r| DesiredInstance { id: r.id, project: project(r.project_id), name: r.name, state: r.desired, spec: r.spec })
        .collect();
    Ok(DesiredNodeState { generation: node.generation, instances, network, map })
}

async fn desired(
    State(state): State<AppState>,
    Path(id): Path<NodeId>,
    Query(q): Query<DesiredQuery>,
    headers: HeaderMap,
) -> Result<Json<DesiredNodeState>> {
    let mut rx = state.changed.subscribe();
    let deadline = tokio::time::sleep(LONG_POLL);
    tokio::pin!(deadline);
    loop {
        let d = read_desired(&state, id, &headers)?;
        match q.after {
            Some(after) if d.generation <= after => {}
            _ => return Ok(Json(d)),
        }
        tokio::select! {
            _ = rx.changed() => continue,
            _ = &mut deadline => return Ok(Json(d)),
        }
    }
}

async fn observed(
    State(state): State<AppState>,
    Path(id): Path<NodeId>,
    headers: HeaderMap,
    Json(report): Json<ObservedNodeState>,
) -> Result<()> {
    let conn = state.db.lock().unwrap();
    let node = authenticate(&conn, id, &headers)?;
    if report.generation_seen > node.generation { return Err(Error::bad_request("observed generation is ahead of desired state")) }
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    let now = db::now();
    let mut removed = false;
    let capacity = if report.generation_seen == node.generation { &report.capacity } else { &node.capacity };
    db::touch_node(&tx, id, capacity)?;
    if node.quiet {
        db::set_quiet(&tx, id, false)?;
        db::record(&tx, db::NewEvent::new("node back", format!("{} is reporting again", node.hostname)).node(id))?;
    }
    if let Some(usage) = report.usage {
        state.metrics.record(Subject::Node(id), now, usage);
    }
    if (report.network_ready, report.tailnet_route) != (node.network_ready, node.tailnet_route) {
        db::set_network_reports(&tx, id, report.network_ready, report.tailnet_route)?;
    }
    let mesh_changed = match &report.mesh {
        Some(mesh) => {
            valid_mesh(mesh)?;
            db::set_mesh(&tx, &node, mesh)?
        }
        None => false,
    };
    for obs in &report.instances {
        let Some(row) = db::instance(&tx, obs.id)? else { continue };
        if row.node_id != id {
            tracing::warn!(instance = %obs.id, node = %id, "report for an instance assigned elsewhere; ignored");
            continue;
        }
        if let Some(usage) = obs.usage {
            state.metrics.record(Subject::Instance(obs.id), now, usage);
        }
        // Delete completes only once the node has confirmed the VM and disk are gone,
        // and only if that confirmation is for the generation that asked for it.
        if row.desired == DesiredState::Absent && obs.state == ObservedState::Absent && report.generation_seen >= node.generation {
            db::delete_instance(&tx, obs.id)?;
            db::bump_project_nodes(&tx, row.project_id)?;
            state.metrics.forget(Subject::Instance(obs.id));
            db::record(&tx, db::NewEvent::new("removed", format!("vm and disk gone from {}", node.hostname)).instance(row.project_id, row.id, &row.name).node(id))?;
            removed = true;
            tracing::info!(instance = %obs.id, "instance removed");
        } else {
            note_transition(&tx, &node, &row, obs, now)?;
            db::set_observed(&tx, obs)?;
        }
    }
    if removed { db::bump_generation(&tx, id)?; }
    tx.commit()?;
    if removed || mesh_changed { state.notify_changed(); }
    Ok(())
}

/// Kiln hands a node's key and endpoints to every other node, so it only
/// stores ones that look like a WireGuard key and IPv4 address:port pairs.
fn valid_mesh(mesh: &MeshReport) -> Result<()> {
    let key = mesh.public_key.len() == 44 && mesh.public_key.ends_with('=')
        && mesh.public_key[..43].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
    let endpoints = mesh.endpoints.len() <= 8 && mesh.endpoints.iter().all(|e| e.address.parse::<std::net::SocketAddrV4>().is_ok());
    if key && endpoints { Ok(()) } else { Err(Error::bad_request("mesh report needs a WireGuard public key and at most 8 IPv4 endpoints")) }
}

/// Log state changes the node reports, and remember when a run began.
fn note_transition(conn: &rusqlite::Connection, node: &db::NodeRow, row: &db::InstanceRow, obs: &ObservedInstance, now: i64) -> rusqlite::Result<()> {
    let before = row.observed.as_ref().map(|o| o.state);
    if before == Some(obs.state) {
        return Ok(());
    }
    let running = obs.state == ObservedState::Running;
    if running != row.running_since.is_some() {
        db::set_running_since(conn, row.id, running.then_some(now))?;
    }
    let event = match obs.state {
        ObservedState::Running => db::NewEvent::new("running", match obs.addresses.first() {
            Some(addr) => format!("on {} · {addr}", node.hostname),
            None => format!("on {}", node.hostname),
        }),
        ObservedState::Stopped if before.is_some_and(|b| b != ObservedState::Absent) => db::NewEvent::new("stopped", format!("on {}", node.hostname)),
        ObservedState::Failed => db::NewEvent::new("failed", obs.error.clone().unwrap_or_else(|| "no reason given".into())),
        _ => return Ok(()),
    };
    db::record(conn, event.instance(row.project_id, row.id, &row.name).node(node.id))
}

pub(crate) fn authenticate(conn: &rusqlite::Connection, id: NodeId, headers: &HeaderMap) -> Result<db::NodeRow> {
    let token_hash = crate::auth::hash(bearer(headers)?);
    let node = db::node(conn, id)?.ok_or_else(Error::unauthorized)?;
    if node.token_hash.as_deref() != Some(token_hash.as_str()) { return Err(Error::unauthorized()) }
    match node.status {
        NodeDbStatus::Retired => Err(Error(StatusCode::GONE, "node retired".into())),
        NodeDbStatus::Pending => Err(Error::forbidden()),
        NodeDbStatus::Approved => Ok(node),
    }
}
