//! Serial consoles, for when SSH won't.
//!
//! 1. The browser opens a WebSocket to kiln for a running instance.
//! 2. Kiln records a pending session; the instance's node learns of it
//!    through `GET /nodes/{id}/consoles`, a long-poll it keeps open.
//! 3. The node dials back with an HTTP upgrade and carries the VM's serial
//!    console bytes over it. Kiln relays them to the WebSocket.
//!
//! Kiln never connects to a node, same as everywhere else in copper.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
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

/// How long the browser waits for the node to dial back.
const NODE_ANSWER: Duration = Duration::from_secs(10);
/// How long a node's console long-poll waits before returning empty.
const LONG_POLL: Duration = Duration::from_secs(25);
/// The HTTP upgrade protocol nodes dial back with.
pub const NODE_PROTOCOL: &str = "copper-console";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/projects/{project}/instances/{name}/console", get(browser))
        .route("/nodes/{id}/consoles", get(node_poll))
        .route("/nodes/{id}/consoles/{session}", get(node_attach))
}

// ---- the broker ----------------------------------------------------------------

struct Pending {
    node: NodeId,
    instance: InstanceId,
    /// Already handed to the node in a poll response.
    offered: bool,
    deliver: oneshot::Sender<Upgraded>,
}

/// Sessions waiting for their node to dial back.
pub struct Consoles {
    pending: Mutex<HashMap<Uuid, Pending>>,
    changed: watch::Sender<u64>,
}

impl Default for Consoles {
    fn default() -> Self {
        Self { pending: Mutex::default(), changed: watch::channel(0).0 }
    }
}

impl Consoles {
    fn open(&self, node: NodeId, instance: InstanceId) -> (Uuid, oneshot::Receiver<Upgraded>) {
        let (deliver, ready) = oneshot::channel();
        let session = Uuid::new_v4();
        self.pending.lock().unwrap().insert(session, Pending { node, instance, offered: false, deliver });
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
                ConsoleRequest { session: *session, instance: p.instance }
            })
            .collect()
    }

    fn take(&self, node: NodeId, session: Uuid) -> Option<oneshot::Sender<Upgraded>> {
        let mut pending = self.pending.lock().unwrap();
        if pending.get(&session)?.node != node {
            return None;
        }
        pending.remove(&session).map(|p| p.deliver)
    }
}

// ---- the browser side ----------------------------------------------------------

async fn browser(
    State(state): State<AppState>,
    auth: Auth,
    Path((project, name)): Path<(String, String)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    same_origin(&headers)?;
    let (node, instance) = {
        let conn = state.db.lock().unwrap();
        let row = user_api::load(&conn, &auth, &project, &name)?;
        let online = db::node(&conn, row.node_id)?.is_some_and(|n| n.online());
        let current = InstanceState::derive(row.desired, row.observed.as_ref().map(|o| o.state), online);
        if current != InstanceState::Running {
            return Err(Error::conflict("the console needs a running instance"));
        }
        db::record(&conn, db::NewEvent::new("console", "serial console opened").instance(row.project_id, row.id, &row.name).by(&auth.0.name))?;
        (row.node_id, row.id)
    };
    let (session, ready) = state.consoles.open(node, instance);
    let consoles = state.consoles.clone();
    Ok(ws.on_upgrade(move |mut socket| async move {
        match tokio::time::timeout(NODE_ANSWER, ready).await {
            Ok(Ok(upgraded)) => relay(socket, TokioIo::new(upgraded)).await,
            _ => {
                consoles.cancel(session);
                let note = "\r\n[copper] the node didn't open the console. Serial consoles need a Linux node running a current ingot.\r\n";
                let _ = socket.send(Message::Text(note.into())).await;
                let _ = socket.close().await;
            }
        }
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

// ---- the node side -------------------------------------------------------------

async fn node_poll(State(state): State<AppState>, Path(id): Path<NodeId>, headers: HeaderMap) -> Result<Json<Vec<ConsoleRequest>>> {
    {
        let conn = state.db.lock().unwrap();
        node_api::authenticate(&conn, id, &headers)?;
    }
    let mut changed = state.consoles.changed.subscribe();
    let deadline = tokio::time::sleep(LONG_POLL);
    tokio::pin!(deadline);
    loop {
        let offers = state.consoles.offers_for(id);
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
    let deliver = state.consoles.take(id, session).ok_or_else(|| Error::not_found("console session"))?;
    let on_upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        if let Ok(upgraded) = on_upgrade.await {
            let _ = deliver.send(upgraded);
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, NODE_PROTOCOL)
        .body(axum::body::Body::empty())
        .map_err(|e| Error::from(anyhow::Error::from(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_offered_once_and_only_to_their_node() {
        let c = Consoles::default();
        let (node, other) = (NodeId::new_v4(), NodeId::new_v4());
        let (session, _ready) = c.open(node, InstanceId::new_v4());
        assert!(c.offers_for(other).is_empty());
        assert_eq!(c.offers_for(node)[0].session, session);
        assert!(c.offers_for(node).is_empty(), "offered once");
        assert!(c.take(other, session).is_none(), "another node can't claim it");
        assert!(c.take(node, session).is_some());
        assert!(c.take(node, session).is_none(), "claimed once");
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
