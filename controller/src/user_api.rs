//! User-facing API: projects, instances, and admin-only node management.

use axum::routing::get;
use axum::{Json, Router};
use homecloud_api::*;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/instances", get(list_instances))
        .route("/v1/nodes", get(list_nodes))
}

async fn list_instances() -> Json<Vec<Instance>> {
    // TODO: auth, project scoping, strip `node` for non-admins.
    Json(vec![])
}

async fn list_nodes() -> Json<Vec<Node>> {
    // TODO: admin only; status derived from nodes.status + last_seen.
    Json(vec![])
}
