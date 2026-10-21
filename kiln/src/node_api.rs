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
    let status = match db::register_node(&conn, &req, &crate::auth::hash(token))? {
        RegisterNodeOutcome::Registered(NodeDbStatus::Pending) => RegistrationStatus::Pending,
        RegisterNodeOutcome::Registered(NodeDbStatus::Approved) => RegistrationStatus::Approved,
        RegisterNodeOutcome::Registered(NodeDbStatus::Retired) => RegistrationStatus::Retired,
        RegisterNodeOutcome::CredentialMismatch => return Err(Error::unauthorized()),
    };
    tracing::info!(node = %req.node_id, host = %req.hostname, ?status, "node registered");
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
    let mut removed = false;
    let capacity = if report.generation_seen == node.generation { &report.capacity } else { &node.capacity };
    db::touch_node(&tx, id, capacity)?;
    for obs in &report.instances {
        let Some(row) = db::instance(&tx, obs.id)? else { continue };
        if row.node_id != id {
            tracing::warn!(instance = %obs.id, node = %id, "report for an instance assigned elsewhere; ignored");
            continue;
        }
        // Delete completes only once the node has confirmed the VM and disk are gone,
        // and only if that confirmation is for the generation that asked for it.
        if row.desired == DesiredState::Absent && obs.state == ObservedState::Absent && report.generation_seen >= node.generation {
            db::delete_instance(&tx, obs.id)?;
            removed = true;
            tracing::info!(instance = %obs.id, "instance removed");
        } else {
            db::set_observed(&tx, obs)?;
        }
    }
    if removed { db::bump_generation(&tx, id)?; }
    tx.commit()?;
    if removed { state.notify_changed(); }
    Ok(())
}

fn authenticate(conn: &rusqlite::Connection, id: NodeId, headers: &HeaderMap) -> Result<db::NodeRow> {
    let token_hash = crate::auth::hash(bearer(headers)?);
    let node = db::node(conn, id)?.ok_or_else(Error::unauthorized)?;
    if node.token_hash.as_deref() != Some(token_hash.as_str()) { return Err(Error::unauthorized()) }
    match node.status {
        NodeDbStatus::Retired => Err(Error(StatusCode::GONE, "node retired".into())),
        NodeDbStatus::Pending => Err(Error::forbidden()),
        NodeDbStatus::Approved => Ok(node),
    }
}
