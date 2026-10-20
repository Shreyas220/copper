//! Controller <-> agent protocol. See docs/design.md.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use copper_api::*;
use serde::Deserialize;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/nodes/register", post(register))
        .route("/nodes/{id}/desired", get(desired))
        .route("/nodes/{id}/observed", post(observed))
}

async fn register(
    State(_state): State<AppState>,
    Json(_req): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, StatusCode> {
    // TODO: insert as pending if unknown; return current status otherwise.
    Err(StatusCode::NOT_IMPLEMENTED)
}

#[derive(Deserialize)]
struct DesiredQuery {
    /// Long-poll: block until generation > after, or a timeout.
    #[allow(dead_code)]
    after: Option<Generation>,
}

async fn desired(
    State(_state): State<AppState>,
    Path(_id): Path<NodeId>,
    Query(_q): Query<DesiredQuery>,
) -> Result<Json<DesiredNodeState>, StatusCode> {
    // TODO: read instances for this node; long-poll on generation.
    Err(StatusCode::NOT_IMPLEMENTED)
}

async fn observed(
    State(_state): State<AppState>,
    Path(_id): Path<NodeId>,
    Json(_report): Json<ObservedNodeState>,
) -> StatusCode {
    // TODO: update last_seen, capacity, observed_* per instance;
    // drop rows where desired = absent and observed = absent.
    StatusCode::NOT_IMPLEMENTED
}
