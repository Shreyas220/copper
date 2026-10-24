//! Admin-only node management: approve, retire, overcommit, live usage.

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use copper_api::*;

use crate::auth::Auth;
use crate::db::{self, NewEvent, NodeDbStatus};
use crate::error::{Error, Result};
use crate::metrics::Subject;
use crate::{placement, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/nodes/{id}/approve", post(approve_node))
        .route("/v1/nodes/{id}/retire", post(retire_node))
        .route("/v1/nodes/{id}/policy", post(set_node_policy))
        .route("/v1/nodes/{id}/metrics", get(node_metrics))
}

pub fn node_api(conn: &rusqlite::Connection, state: &AppState, node: &db::NodeRow) -> Result<Node> {
    let instances = db::list_instances_on_node(conn, node.id)?;
    let mut names = vec![];
    for inst in &instances {
        let project = db::project_by_id(conn, inst.project_id)?.map(|p| p.name).unwrap_or_default();
        names.push(format!("{project}/{}", inst.name));
    }
    let usage = state.metrics.latest(Subject::Node(node.id)).filter(|_| node.online());
    let subnet = node.subnet.map(|n| state.vm_net.node(n).subnet);
    // Only peers still in the mesh; a report may lag a retirement.
    let mut peers = vec![];
    for p in node.mesh.iter().flat_map(|m| &m.peers) {
        if let Some(other) = db::node(conn, p.node)?.filter(|o| o.status == db::NodeDbStatus::Approved) {
            peers.push(NodePeer { hostname: other.hostname, via: p.via, handshake_age_s: p.handshake_age_s });
        }
    }
    Ok(node.to_api(placement::allocated(instances.iter()), names, usage, subnet, peers))
}

async fn list_nodes(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Node>>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    db::list_nodes(&conn)?.iter().map(|n| node_api(&conn, &state, n)).collect::<Result<Vec<_>>>().map(Json)
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
    let detail = format!("{} · cpu {}× · memory {}× · disk {}×", node.hostname, oc.cpu, oc.memory, oc.disk);
    db::record(&conn, NewEvent::new("overcommit", detail).node(id).by(&auth.0.name))?;
    tracing::info!(node = %id, cpu = oc.cpu, memory = oc.memory, disk = oc.disk, "overcommit changed");
    node_api(&conn, &state, &db::node(&conn, id)?.unwrap()).map(Json)
}

async fn approve_node(State(state): State<AppState>, auth: Auth, Path(id): Path<NodeId>) -> Result<Json<Node>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let node = db::node(&conn, id)?.ok_or_else(|| Error::not_found("node"))?;
    if node.status == NodeDbStatus::Retired {
        return Err(Error::conflict("node is retired"));
    }
    if node.token_hash.is_none() {
        return Err(Error::conflict("node must register its credential before approval"));
    }
    db::set_node_status(&conn, id, NodeDbStatus::Approved)?;
    let subnet = state.vm_net.node(db::ensure_subnet(&conn, id).map_err(|e| Error::conflict(e.to_string()))?).subnet;
    // Its peers in the mesh change, once it has reported a key.
    db::bump_approved_nodes(&conn)?;
    db::record(&conn, NewEvent::new("node approved", format!("{} can take work · its vms get {subnet}", node.hostname)).node(id).by(&auth.0.name))?;
    state.notify_changed();
    tracing::info!(node = %id, host = %node.hostname, "node approved");
    node_api(&conn, &state, &db::node(&conn, id)?.unwrap()).map(Json)
}

/// Retiring a node marks every instance on it failed. Nothing is re-created.
async fn retire_node(State(state): State<AppState>, auth: Auth, Path(id): Path<NodeId>) -> Result<Json<Node>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let node = db::node(&conn, id)?.ok_or_else(|| Error::not_found("node"))?;
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    db::set_node_status(&tx, id, NodeDbStatus::Retired)?;
    db::fail_instances_on_node(&tx, id, "node retired")?;
    // It leaves every other node's mesh.
    db::bump_approved_nodes(&tx)?;
    db::record(&tx, NewEvent::new("node retired", format!("{}; its vms are marked failed, disks stay on the box", node.hostname)).node(id).by(&auth.0.name))?;
    tx.commit()?;
    state.notify_changed();
    tracing::info!(node = %id, host = %node.hostname, "node retired");
    node_api(&conn, &state, &db::node(&conn, id)?.unwrap()).map(Json)
}

async fn node_metrics(State(state): State<AppState>, auth: Auth, Path(id): Path<NodeId>) -> Result<Json<Vec<Sample>>> {
    auth.require_admin()?;
    Ok(Json(state.metrics.hour(Subject::Node(id))))
}
