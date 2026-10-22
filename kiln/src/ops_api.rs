//! What the console watches: images, the event log, metrics and quotas.

use axum::extract::{Path, Query, State};
use axum::routing::{get, put};
use axum::{Json, Router};
use copper_api::*;
use serde::Deserialize;

use crate::auth::Auth;
use crate::db::{self, EventFilter, NewEvent};
use crate::error::{Error, Result};
use crate::metrics::Subject;
use crate::user_api::{load, project};
use crate::{placement, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/info", get(info))
        .route("/v1/images", get(list_images))
        .route("/v1/events", get(list_events))
        .route("/v1/projects/{project}/instances/{name}/metrics", get(instance_metrics))
        .route("/v1/quotas", get(list_quotas))
        .route("/v1/projects/{project}/quota", put(set_quota))
}

async fn info(_auth: Auth) -> Json<KilnInfo> {
    Json(KilnInfo { hostname: hostname(), version: env!("CARGO_PKG_VERSION").to_string() })
}

/// The short hostname, as ingot reports it for its node.
fn hostname() -> String {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).split('.').next().unwrap_or_default().to_string()
}

async fn list_images(State(state): State<AppState>, _auth: Auth) -> Result<Json<Vec<Image>>> {
    let conn = state.db.lock().unwrap();
    Ok(Json(db::list_images(&conn)?))
}

#[derive(Deserialize)]
struct EventQuery {
    /// `name` or `owner:name`, as in instance URLs.
    project: Option<String>,
    /// An instance name within `project`.
    instance: Option<String>,
    limit: Option<u32>,
}

/// Newest first. Ordinary users only ever see their own projects; node-only
/// events such as "node went quiet" are for admins.
async fn list_events(State(state): State<AppState>, auth: Auth, Query(q): Query<EventQuery>) -> Result<Json<Vec<Event>>> {
    let conn = state.db.lock().unwrap();
    let mut filter = EventFilter { limit: q.limit.unwrap_or(100), ..EventFilter::default() };
    match (&q.project, &q.instance) {
        (Some(p), Some(name)) => filter.instance = Some(load(&conn, &auth, p, name)?.id),
        (Some(p), None) => filter.project = Some(project(&conn, &auth, p)?.project.id),
        (None, Some(_)) => return Err(Error::bad_request("instance needs project")),
        (None, None) => {}
    }
    if !auth.is_admin() {
        filter.owner = Some(auth.0.id);
    }
    let mut events = db::list_events(&conn, &filter)?;
    if !auth.is_admin() {
        for e in &mut events {
            e.node = None; // only admins see placement
        }
    }
    Ok(Json(events))
}

async fn instance_metrics(State(state): State<AppState>, auth: Auth, Path((project, name)): Path<(String, String)>) -> Result<Json<Vec<Sample>>> {
    let id = {
        let conn = state.db.lock().unwrap();
        load(&conn, &auth, &project, &name)?.id
    };
    Ok(Json(state.metrics.hour(Subject::Instance(id))))
}

/// Admins see every project; users see their own.
async fn list_quotas(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Quota>>> {
    let conn = state.db.lock().unwrap();
    let projects = if auth.is_admin() {
        db::list_all_projects(&conn)?
    } else {
        db::list_projects(&conn, auth.0.id)?.into_iter().map(|p| (p, auth.0.name.clone())).collect()
    };
    projects
        .into_iter()
        .map(|(p, owner)| Ok(Quota { used: placement::project_usage(&conn, p.id)?, project: p.name, owner, caps: p.quota }))
        .collect::<Result<Vec<_>>>()
        .map(Json)
}

/// Caps apply to new creates and resizes. Lowering one below current use
/// evicts nothing; the project just can't grow until it's under again.
async fn set_quota(State(state): State<AppState>, auth: Auth, Path(path): Path<String>, Json(caps): Json<QuotaCaps>) -> Result<Json<Quota>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let p = project(&conn, &auth, &path)?;
    db::set_quota(&conn, p.project.id, &caps)?;
    let show = |v: Option<u64>, unit: &str| v.map_or("no cap".to_string(), |v| format!("{v}{unit}"));
    let detail = format!(
        "vcpu {} · memory {} · disk {}",
        show(caps.vcpus.map(u64::from), ""),
        show(caps.memory_mb, " MB"),
        show(caps.disk_gb, " GB")
    );
    let mut event = NewEvent::new("quota", detail).by(&auth.0.name);
    event.project = Some(p.project.id);
    db::record(&conn, event)?;
    Ok(Json(Quota { used: placement::project_usage(&conn, p.project.id)?, project: p.project.name, owner: p.owner, caps }))
}
