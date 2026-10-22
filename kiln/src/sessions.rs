//! Sessions: reaching a VM through its node. The serial console in the
//! browser, and TCP to one of the VM's ports for `cu ssh` and `cu forward`.
//!
//! 1. The browser (a WebSocket) or cu (an HTTP upgrade) asks kiln for a
//!    session on a running VM.
//! 2. Kiln records it as pending; the VM's node learns of it through
//!    `GET /nodes/{id}/consoles`, a long-poll it keeps open.
//! 3. The node connects to its VM, serial console or port, and dials kiln
//!    back with an HTTP upgrade to carry the bytes. If it can't, it says why.
//!
//! Kiln never connects to a node or a VM, so this works the same wherever
//! the node is: on the home router or anywhere else on the tailnet.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use copper_api::*;
use futures_util::{SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

use crate::auth::Auth;
use crate::db;
use crate::error::{Error, Result};
use crate::{node_api, user_api, AppState};

/// How long a caller waits for the node to dial back.
const NODE_ANSWER: Duration = Duration::from_secs(10);
/// How long a node's long-poll waits before returning empty.
const LONG_POLL: Duration = Duration::from_secs(25);
/// The HTTP upgrade protocol nodes dial back with.
pub const NODE_PROTOCOL: &str = "copper-console";
/// The HTTP upgrade protocol cu asks for a TCP session with.
pub const CLIENT_PROTOCOL: &str = "copper-tcp";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/projects/{project}/instances/{name}/console", get(browser))
        .route("/v1/projects/{project}/instances/{name}/ssh", get(ssh))
        .route("/v1/projects/{project}/instances/{name}/ports/{port}", get(port))
        .route("/nodes/{id}/consoles", get(node_poll))
        .route("/nodes/{id}/consoles/{session}", get(node_attach))
        .route("/nodes/{id}/consoles/{session}/refuse", post(node_refuse))
}

// ---- the broker ----------------------------------------------------------------

/// What the node hands back: its end of the session, or why there's none.
type Answer = std::result::Result<Upgraded, String>;

struct Pending {
    node: NodeId,
    instance: InstanceId,
    /// `None` for the serial console.
    port: Option<u16>,
    /// Already handed to the node in a poll response.
    offered: bool,
    deliver: oneshot::Sender<Answer>,
}

/// Sessions waiting for their node to dial back.
pub struct Sessions {
    pending: Mutex<HashMap<Uuid, Pending>>,
    changed: watch::Sender<u64>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self { pending: Mutex::default(), changed: watch::channel(0).0 }
    }
}

impl Sessions {
    fn open(&self, node: NodeId, instance: InstanceId, port: Option<u16>) -> (Uuid, oneshot::Receiver<Answer>) {
        let (deliver, ready) = oneshot::channel();
        let session = Uuid::new_v4();
        self.pending.lock().unwrap().insert(session, Pending { node, instance, port, offered: false, deliver });
        self.changed.send_modify(|v| *v += 1);
        (session, ready)
    }

    fn cancel(&self, session: Uuid) {
        self.pending.lock().unwrap().remove(&session);
    }

    /// Sessions for this node it hasn't been told about yet.
    fn offers_for(&self, node: NodeId) -> Vec<ConsoleRequest> {
        let mut pending = self.pending.lock().unwrap();
        pending
            .iter_mut()
            .filter(|(_, p)| p.node == node && !p.offered)
            .map(|(session, p)| {
                p.offered = true;
                ConsoleRequest { session: *session, instance: p.instance, port: p.port }
            })
            .collect()
    }

    fn take(&self, node: NodeId, session: Uuid) -> Option<oneshot::Sender<Answer>> {
        let mut pending = self.pending.lock().unwrap();
        if pending.get(&session)?.node != node {
            return None;
        }
        pending.remove(&session).map(|p| p.deliver)
    }
}

/// The instance's node, if the instance is running and the caller may reach it.
fn running_on(state: &AppState, auth: &Auth, project: &str, name: &str) -> Result<(db::InstanceRow, NodeId)> {
    let conn = state.db.lock().unwrap();
    let row = user_api::load(&conn, auth, project, name)?;
    let online = db::node(&conn, row.node_id)?.is_some_and(|n| n.online());
    let current = InstanceState::derive(row.desired, row.observed.as_ref().map(|o| o.state), online);
    if current != InstanceState::Running {
        let shown = serde_json::to_string(&current)?;
        return Err(Error::conflict(format!("instance is {}, not running", shown.trim_matches('"'))));
    }
    let node = row.node_id;
    Ok((row, node))
}

// ---- the serial console, from the browser ---------------------------------------

async fn browser(
    State(state): State<AppState>,
    auth: Auth,
    Path((project, name)): Path<(String, String)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    same_origin(&headers)?;
    let (row, node) = running_on(&state, &auth, &project, &name)?;
    {
        let conn = state.db.lock().unwrap();
        db::record(&conn, db::NewEvent::new("console", "serial console opened").instance(row.project_id, row.id, &row.name).by(&auth.0.name))?;
    }
    let (session, ready) = state.sessions.open(node, row.id, None);
    let sessions = state.sessions.clone();
    Ok(ws.on_upgrade(move |mut socket| async move {
        let note = match tokio::time::timeout(NODE_ANSWER, ready).await {
            Ok(Ok(Ok(upgraded))) => return relay(socket, TokioIo::new(upgraded)).await,
            Ok(Ok(Err(reason))) => format!("\r\n[copper] {reason}\r\n"),
            _ => {
                sessions.cancel(session);
                "\r\n[copper] the node didn't open the console. It needs to be online and running a current ingot.\r\n".into()
            }
        };
        let _ = socket.send(Message::Text(note.into())).await;
        let _ = socket.close().await;
    }))
}

/// A page on another site can open a WebSocket to kiln and the browser will
/// send our cookie with it, so only accept pages served by kiln itself.
fn same_origin(headers: &HeaderMap) -> Result<()> {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else { return Ok(()) };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let origin_host = origin.split_once("://").map(|(_, rest)| rest).unwrap_or(origin);
    if origin_host == host { Ok(()) } else { Err(Error::forbidden()) }
}

/// Shuttle bytes between the browser and the node until either side ends.
async fn relay(socket: WebSocket, node: TokioIo<Upgraded>) {
    let (mut to_browser, mut from_browser) = socket.split();
    let (mut from_node, mut to_node) = tokio::io::split(node);
    let upstream = async {
        while let Some(Ok(msg)) = from_browser.next().await {
            let bytes = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Close(_) => break,
                _ => continue,
            };
            if to_node.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };
    let downstream = async {
        let mut buf = vec![0u8; 8192];
        loop {
            match from_node.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_browser.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to_browser.close().await;
    };
    tokio::select! {
        _ = upstream => {},
        _ = downstream => {},
    }
}

// ---- TCP to a VM's port, from cu -------------------------------------------------

/// `cu ssh` and `cu ssh-config`: port 22.
async fn ssh(state: State<AppState>, auth: Auth, Path((project, name)): Path<(String, String)>, req: axum::extract::Request) -> Result<Response> {
    tcp(state, auth, project, name, 22, req).await
}

/// `cu forward`: any port.
async fn port(state: State<AppState>, auth: Auth, Path((project, name, port)): Path<(String, String, u16)>, req: axum::extract::Request) -> Result<Response> {
    tcp(state, auth, project, name, port, req).await
}

/// Only the project's owner (or an admin) gets a session, and only to a
/// running instance. Kiln waits for the node before answering, so a VM that
/// isn't listening yet is a 503 that cu retries, not a dead connection.
async fn tcp(State(state): State<AppState>, auth: Auth, project: String, name: String, port: u16, mut req: axum::extract::Request) -> Result<Response> {
    if req.headers().get(header::UPGRADE).and_then(|v| v.to_str().ok()) != Some(CLIENT_PROTOCOL) {
        return Err(Error(StatusCode::UPGRADE_REQUIRED, format!("this endpoint needs Upgrade: {CLIENT_PROTOCOL}; use cu ssh or cu forward")));
    }
    if port == 0 {
        return Err(Error::bad_request("port must be 1-65535"));
    }
    let (row, node) = running_on(&state, &auth, &project, &name)?;
    let (session, ready) = state.sessions.open(node, row.id, Some(port));
    let from_node = match tokio::time::timeout(NODE_ANSWER, ready).await {
        Ok(Ok(Ok(upgraded))) => upgraded,
        Ok(Ok(Err(reason))) => return Err(Error(StatusCode::SERVICE_UNAVAILABLE, reason)),
        _ => {
            state.sessions.cancel(session);
            return Err(Error(StatusCode::GATEWAY_TIMEOUT, "the vm's node didn't answer; it needs to be online and running a current ingot".into()));
        }
    };
    let on_upgrade = hyper::upgrade::on(&mut req);
    let (user, id) = (auth.0.name.clone(), row.id);
    tracing::info!(%user, instance = %id, port, "tunnel opened");
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(client) => {
                let result = tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(from_node)).await;
                tracing::info!(%user, instance = %id, port, ?result, "tunnel closed");
            }
            Err(e) => tracing::warn!(%user, instance = %id, error = %e, "tunnel upgrade failed"),
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, CLIENT_PROTOCOL)
        .body(axum::body::Body::empty())
        .map_err(|e| Error::from(anyhow::Error::from(e)))
}

// ---- the node side -------------------------------------------------------------

async fn node_poll(State(state): State<AppState>, Path(id): Path<NodeId>, headers: HeaderMap) -> Result<Json<Vec<ConsoleRequest>>> {
    {
        let conn = state.db.lock().unwrap();
        node_api::authenticate(&conn, id, &headers)?;
    }
    let mut changed = state.sessions.changed.subscribe();
    let deadline = tokio::time::sleep(LONG_POLL);
    tokio::pin!(deadline);
    loop {
        let offers = state.sessions.offers_for(id);
        if !offers.is_empty() {
            return Ok(Json(offers));
        }
        tokio::select! {
            _ = changed.changed() => {},
            _ = &mut deadline => return Ok(Json(vec![])),
        }
    }
}

async fn node_attach(
    State(state): State<AppState>,
    Path((id, session)): Path<(NodeId, Uuid)>,
    headers: HeaderMap,
    mut req: axum::extract::Request,
) -> Result<Response> {
    {
        let conn = state.db.lock().unwrap();
        node_api::authenticate(&conn, id, &headers)?;
    }
    if headers.get(header::UPGRADE).and_then(|v| v.to_str().ok()) != Some(NODE_PROTOCOL) {
        return Err(Error(StatusCode::UPGRADE_REQUIRED, format!("needs Upgrade: {NODE_PROTOCOL}")));
    }
    let deliver = state.sessions.take(id, session).ok_or_else(|| Error::not_found("session"))?;
    let on_upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        if let Ok(upgraded) = on_upgrade.await {
            let _ = deliver.send(Ok(upgraded));
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, NODE_PROTOCOL)
        .body(axum::body::Body::empty())
        .map_err(|e| Error::from(anyhow::Error::from(e)))
}

/// The node couldn't reach its VM, e.g. nothing listens on the port yet.
async fn node_refuse(
    State(state): State<AppState>,
    Path((id, session)): Path<(NodeId, Uuid)>,
    headers: HeaderMap,
    Json(refused): Json<SessionRefused>,
) -> Result<StatusCode> {
    {
        let conn = state.db.lock().unwrap();
        node_api::authenticate(&conn, id, &headers)?;
    }
    let deliver = state.sessions.take(id, session).ok_or_else(|| Error::not_found("session"))?;
    let reason: String = refused.reason.chars().filter(|c| !c.is_control()).take(200).collect();
    let _ = deliver.send(Err(reason));
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_offered_once_and_only_to_their_node() {
        let s = Sessions::default();
        let (node, other) = (NodeId::new_v4(), NodeId::new_v4());
        let (session, _ready) = s.open(node, InstanceId::new_v4(), Some(22));
        assert!(s.offers_for(other).is_empty());
        let offers = s.offers_for(node);
        assert_eq!((offers[0].session, offers[0].port), (session, Some(22)));
        assert!(s.offers_for(node).is_empty(), "offered once");
        assert!(s.take(other, session).is_none(), "another node can't claim it");
        assert!(s.take(node, session).is_some());
        assert!(s.take(node, session).is_none(), "claimed once");
    }

    #[test]
    fn websockets_only_from_pages_kiln_served() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "kiln:8400".parse().unwrap());
        assert!(same_origin(&h).is_ok(), "no Origin: not a browser");
        h.insert(header::ORIGIN, "http://kiln:8400".parse().unwrap());
        assert!(same_origin(&h).is_ok());
        h.insert(header::ORIGIN, "http://evil.example".parse().unwrap());
        assert!(same_origin(&h).is_err());
    }
}
