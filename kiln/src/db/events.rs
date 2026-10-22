//! The event log: what happened, to what, and who asked.

use copper_api::{Event, InstanceId, NodeId};
use rusqlite::{params, Connection};

use super::{now, Uuid};

/// Something worth telling a person about. Build with `NewEvent::new` and
/// the `instance`/`node`/`by` helpers.
#[derive(Default)]
pub struct NewEvent {
    pub kind: &'static str,
    pub detail: String,
    pub project: Option<Uuid>,
    pub instance: Option<(InstanceId, String)>,
    pub node: Option<NodeId>,
    pub actor: Option<String>,
}

impl NewEvent {
    pub fn new(kind: &'static str, detail: impl Into<String>) -> Self {
        Self { kind, detail: detail.into(), ..Self::default() }
    }
    pub fn instance(mut self, project: Uuid, id: InstanceId, name: &str) -> Self {
        self.project = Some(project);
        self.instance = Some((id, name.to_string()));
        self
    }
    pub fn node(mut self, node: NodeId) -> Self {
        self.node = Some(node);
        self
    }
    pub fn by(mut self, actor: &str) -> Self {
        self.actor = Some(actor.to_string());
        self
    }
}

pub fn record(conn: &Connection, e: NewEvent) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO events (at, kind, detail, project_id, instance_id, instance_name, node_id, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            now(),
            e.kind,
            e.detail,
            e.project.map(|p| p.to_string()),
            e.instance.as_ref().map(|(id, _)| id.to_string()),
            e.instance.as_ref().map(|(_, name)| name.clone()),
            e.node.map(|n| n.to_string()),
            e.actor,
        ],
    )?;
    Ok(())
}

/// Which events to list. Every filter is optional and they combine.
#[derive(Default)]
pub struct EventFilter {
    pub instance: Option<InstanceId>,
    pub project: Option<Uuid>,
    /// Only events in projects this user owns. Ordinary users always get this.
    pub owner: Option<Uuid>,
    pub limit: u32,
}

pub fn list_events(conn: &Connection, f: &EventFilter) -> rusqlite::Result<Vec<Event>> {
    conn.prepare(
        "SELECT e.id, e.at, e.kind, e.detail, p.name, e.instance_name, n.hostname, e.actor
         FROM events e
         LEFT JOIN projects p ON p.id = e.project_id
         LEFT JOIN nodes n ON n.id = e.node_id
         WHERE (?1 IS NULL OR e.instance_id = ?1)
           AND (?2 IS NULL OR e.project_id = ?2)
           AND (?3 IS NULL OR p.owner_id = ?3)
         ORDER BY e.id DESC LIMIT ?4",
    )?
    .query_map(
        params![f.instance.map(|i| i.to_string()), f.project.map(|p| p.to_string()), f.owner.map(|o| o.to_string()), f.limit.clamp(1, 500)],
        |r| {
            Ok(Event {
                id: r.get(0)?,
                at: r.get(1)?,
                kind: r.get(2)?,
                detail: r.get(3)?,
                project: r.get(4)?,
                instance: r.get(5)?,
                node: r.get(6)?,
                actor: r.get(7)?,
            })
        },
    )?
    .collect()
}

/// Keep the log from growing without bound.
pub fn prune_events(conn: &Connection, older_than: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM events WHERE at < ?1", [older_than])
}
