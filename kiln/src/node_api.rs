//! Kiln <-> ingot protocol. See docs/design.md.
//!
//! No token auth here for v1: the tailnet is the trust boundary, and a node
//! must be approved by an admin before kiln hands it any desired state.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use copper_api::*;
use serde::Deserialize;

use crate::db::{self, NodeDbStatus};
use crate::error::{Error, Result};
use crate::AppState;

/// How long a desired-state long-poll waits before returning unchanged.
const LONG_POLL: Duration = Duration::from_secs(25);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/nodes/register", post(register))
        .route("/nodes/{id}/desired", get(desired))
        .route("/nodes/{id}/observed", post(observed))
}

async fn register(State(state): State<AppState>, Json(req): Json<RegisterRequest>) -> Result<Json<RegisterResponse>> {
    let conn = state.db.lock().unwrap();
    let status = match db::register_node(&conn, &req)? {
        NodeDbStatus::Pending => RegistrationStatus::Pending,
        NodeDbStatus::Approved => RegistrationStatus::Approved,
        NodeDbStatus::Retired => RegistrationStatus::Retired,
    };
    tracing::info!(node = %req.node_id, host = %req.hostname, ?status, "node registered");
    Ok(Json(RegisterResponse { status }))
}

#[derive(Deserialize)]
struct DesiredQuery {
    /// Long-poll: block until generation > after, or the timeout passes.
    after: Option<Generation>,
}

fn read_desired(state: &AppState, id: NodeId) -> Result<DesiredNodeState> {
    let conn = state.db.lock().unwrap();
    let node = db::node(&conn, id)?.ok_or_else(|| Error::not_found("node"))?;
    if node.status != NodeDbStatus::Approved {
        return Err(Error::forbidden());
    }
    let instances = db::list_instances_on_node(&conn, id)?
        .into_iter()
        .map(|r| DesiredInstance { id: r.id, name: r.name, state: r.desired, spec: r.spec })
        .collect();
    Ok(DesiredNodeState { generation: node.generation, instances })
}

async fn desired(
    State(state): State<AppState>,
    Path(id): Path<NodeId>,
    Query(q): Query<DesiredQuery>,
) -> Result<Json<DesiredNodeState>> {
    let mut rx = state.changed.subscribe();
    let deadline = tokio::time::sleep(LONG_POLL);
    tokio::pin!(deadline);
    loop {
        let d = read_desired(&state, id)?;
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
    Json(report): Json<ObservedNodeState>,
) -> Result<()> {
    let conn = state.db.lock().unwrap();
    let node = db::node(&conn, id)?.ok_or_else(|| Error::not_found("node"))?;
    if node.status != NodeDbStatus::Approved {
        return Err(Error::forbidden());
    }
    db::touch_node(&conn, id, &report.capacity)?;
    for obs in &report.instances {
        let Some(row) = db::instance(&conn, obs.id)? else { continue };
        if row.node_id != id {
            tracing::warn!(instance = %obs.id, node = %id, "report for an instance assigned elsewhere; ignored");
            continue;
        }
        // Delete completes only once the node has confirmed the VM and disk are gone,
        // and only if that confirmation is for the generation that asked for it.
        if row.desired == DesiredState::Absent && obs.state == ObservedState::Absent && report.generation_seen >= node.generation {
            db::delete_instance(&conn, obs.id)?;
            tracing::info!(instance = %obs.id, "instance removed");
        } else {
            db::set_observed(&conn, obs)?;
        }
    }
    Ok(())
}
