//! SQLite schema, migrations and the queries kiln needs. Everything here is
//! synchronous rusqlite; callers hold the connection mutex briefly.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use copper_api::*;
use rusqlite::{params, Connection, OptionalExtension, Row};

/// A node is offline once it has gone this long without reporting.
pub const OFFLINE_AFTER_SECS: i64 = 30;

const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE nodes (
        id            TEXT PRIMARY KEY,
        hostname      TEXT NOT NULL,
        arch          TEXT NOT NULL,
        status        TEXT NOT NULL,          -- pending | approved | retired
        generation    INTEGER NOT NULL DEFAULT 0,
        last_seen     INTEGER,                -- unix seconds; NULL until first report
        capacity_json TEXT NOT NULL
    );

    CREATE TABLE users (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL UNIQUE,
        role       TEXT NOT NULL               -- admin | user
    );

    CREATE TABLE tokens (
        hash       TEXT PRIMARY KEY,
        user_id    TEXT NOT NULL REFERENCES users(id),
        created_at INTEGER NOT NULL
    );

    CREATE TABLE projects (
        id       TEXT PRIMARY KEY,
        name     TEXT NOT NULL,
        owner_id TEXT NOT NULL REFERENCES users(id),
        UNIQUE (owner_id, name)
    );

    CREATE TABLE instances (
        id             TEXT PRIMARY KEY,
        project_id     TEXT NOT NULL REFERENCES projects(id),
        name           TEXT NOT NULL,
        node_id        TEXT NOT NULL REFERENCES nodes(id),
        desired_state  TEXT NOT NULL,          -- running | stopped | absent
        spec_json      TEXT NOT NULL,
        observed_state TEXT,                   -- NULL until the node reports
        observed_json  TEXT,                   -- full ObservedInstance
        created_at     INTEGER NOT NULL,
        UNIQUE (project_id, name)
    );
    "#,
];

pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = i as i64 + 1;
        tracing::info!(version, "applying migration");
        conn.execute_batch(sql)?;
        conn.pragma_update(None, "user_version", version)?;
    }
    Ok(())
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

// ---- small enum <-> text helpers -------------------------------------------

fn enum_to_text<T: serde::Serialize>(v: &T) -> String {
    // serde gives us the snake_case name as a JSON string; strip the quotes.
    serde_json::to_string(v).unwrap().trim_matches('"').to_string()
}

fn text_to_enum<T: serde::de::DeserializeOwned>(s: &str) -> rusqlite::Result<T> {
    serde_json::from_str(&format!("\"{s}\""))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn uuid_col(row: &Row, idx: usize) -> rusqlite::Result<Uuid> {
    let s: String = row.get(idx)?;
    s.parse().map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e)))
}

pub use uuid::Uuid;

// ---- users and tokens ------------------------------------------------------

#[derive(Debug, Clone)]
pub struct User {
    pub id: Uuid,
    pub name: String,
    pub role: Role,
}

pub fn ensure_user(conn: &Connection, name: &str, role: Role) -> rusqlite::Result<User> {
    if let Some(u) = user_by_name(conn, name)? {
        if u.role != role {
            conn.execute("UPDATE users SET role = ?1 WHERE id = ?2", params![enum_to_text(&role), u.id.to_string()])?;
        }
        return Ok(User { role, ..u });
    }
    let id = Uuid::new_v4();
    conn.execute(
        "INSERT INTO users (id, name, role) VALUES (?1, ?2, ?3)",
        params![id.to_string(), name, enum_to_text(&role)],
    )?;
    Ok(User { id, name: name.to_string(), role })
}

fn user_from_row(row: &Row) -> rusqlite::Result<User> {
    Ok(User { id: uuid_col(row, 0)?, name: row.get(1)?, role: text_to_enum(&row.get::<_, String>(2)?)? })
}

pub fn user_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<User>> {
    conn.query_row("SELECT id, name, role FROM users WHERE name = ?1", [name], user_from_row).optional()
}

pub fn insert_token(conn: &Connection, hash: &str, user: Uuid) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO tokens (hash, user_id, created_at) VALUES (?1, ?2, ?3)",
        params![hash, user.to_string(), now()],
    )?;
    Ok(())
}

pub fn user_by_token_hash(conn: &Connection, hash: &str) -> rusqlite::Result<Option<User>> {
    conn.query_row(
        "SELECT u.id, u.name, u.role FROM tokens t JOIN users u ON u.id = t.user_id WHERE t.hash = ?1",
        [hash],
        user_from_row,
    )
    .optional()
}

// ---- projects ----------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub owner_id: Uuid,
}

pub fn create_project(conn: &Connection, owner: Uuid, name: &str) -> rusqlite::Result<Project> {
    let id = Uuid::new_v4();
    conn.execute(
        "INSERT INTO projects (id, name, owner_id) VALUES (?1, ?2, ?3)",
        params![id.to_string(), name, owner.to_string()],
    )?;
    Ok(Project { id, name: name.to_string(), owner_id: owner })
}

pub fn project_by_name(conn: &Connection, owner: Uuid, name: &str) -> rusqlite::Result<Option<Project>> {
    conn.query_row(
        "SELECT id, name, owner_id FROM projects WHERE owner_id = ?1 AND name = ?2",
        params![owner.to_string(), name],
        |r| Ok(Project { id: uuid_col(r, 0)?, name: r.get(1)?, owner_id: uuid_col(r, 2)? }),
    )
    .optional()
}

pub fn list_projects(conn: &Connection, owner: Uuid) -> rusqlite::Result<Vec<Project>> {
    conn.prepare("SELECT id, name, owner_id FROM projects WHERE owner_id = ?1 ORDER BY name")?
        .query_map([owner.to_string()], |r| Ok(Project { id: uuid_col(r, 0)?, name: r.get(1)?, owner_id: uuid_col(r, 2)? }))?
        .collect()
}

// ---- nodes ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeDbStatus {
    Pending,
    Approved,
    Retired,
}

#[derive(Debug, Clone)]
pub struct NodeRow {
    pub id: NodeId,
    pub hostname: String,
    pub arch: Arch,
    pub status: NodeDbStatus,
    pub generation: Generation,
    pub last_seen: Option<i64>,
    pub capacity: Capacity,
}

impl NodeRow {
    pub fn online(&self) -> bool {
        self.status == NodeDbStatus::Approved
            && self.last_seen.is_some_and(|t| now() - t <= OFFLINE_AFTER_SECS)
    }
    pub fn public_status(&self) -> NodeStatus {
        match self.status {
            NodeDbStatus::Pending => NodeStatus::Pending,
            NodeDbStatus::Retired => NodeStatus::Retired,
            NodeDbStatus::Approved if self.online() => NodeStatus::Online,
            NodeDbStatus::Approved => NodeStatus::Offline,
        }
    }
    pub fn to_api(&self) -> Node {
        Node { id: self.id, hostname: self.hostname.clone(), arch: self.arch, status: self.public_status() }
    }
}

fn node_from_row(r: &Row) -> rusqlite::Result<NodeRow> {
    Ok(NodeRow {
        id: uuid_col(r, 0)?,
        hostname: r.get(1)?,
        arch: text_to_enum(&r.get::<_, String>(2)?)?,
        status: text_to_enum(&r.get::<_, String>(3)?)?,
        generation: r.get::<_, i64>(4)? as u64,
        last_seen: r.get(5)?,
        capacity: serde_json::from_str(&r.get::<_, String>(6)?)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e)))?,
    })
}

const NODE_COLS: &str = "id, hostname, arch, status, generation, last_seen, capacity_json";

pub fn node(conn: &Connection, id: NodeId) -> rusqlite::Result<Option<NodeRow>> {
    conn.query_row(&format!("SELECT {NODE_COLS} FROM nodes WHERE id = ?1"), [id.to_string()], node_from_row).optional()
}

pub fn list_nodes(conn: &Connection) -> rusqlite::Result<Vec<NodeRow>> {
    conn.prepare(&format!("SELECT {NODE_COLS} FROM nodes ORDER BY hostname"))?
        .query_map([], node_from_row)?
        .collect()
}

/// Insert as pending if unknown. Hostname and capacity are refreshed on every call.
pub fn register_node(conn: &Connection, req: &RegisterRequest) -> rusqlite::Result<NodeDbStatus> {
    let cap = serde_json::to_string(&req.capacity).unwrap();
    conn.execute(
        "INSERT INTO nodes (id, hostname, arch, status, capacity_json)
         VALUES (?1, ?2, ?3, 'pending', ?4)
         ON CONFLICT(id) DO UPDATE SET hostname = excluded.hostname, capacity_json = excluded.capacity_json",
        params![req.node_id.to_string(), req.hostname, enum_to_text(&req.arch), cap],
    )?;
    Ok(node(conn, req.node_id)?.map(|n| n.status).unwrap_or(NodeDbStatus::Pending))
}

pub fn set_node_status(conn: &Connection, id: NodeId, status: NodeDbStatus) -> rusqlite::Result<bool> {
    Ok(conn.execute("UPDATE nodes SET status = ?1 WHERE id = ?2", params![enum_to_text(&status), id.to_string()])? == 1)
}

pub fn bump_generation(conn: &Connection, id: NodeId) -> rusqlite::Result<()> {
    conn.execute("UPDATE nodes SET generation = generation + 1 WHERE id = ?1", [id.to_string()])?;
    Ok(())
}

pub fn touch_node(conn: &Connection, id: NodeId, capacity: &Capacity) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE nodes SET last_seen = ?1, capacity_json = ?2 WHERE id = ?3",
        params![now(), serde_json::to_string(capacity).unwrap(), id.to_string()],
    )?;
    Ok(())
}

// ---- instances ---------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct InstanceRow {
    pub id: InstanceId,
    pub project_id: Uuid,
    pub name: String,
    pub node_id: NodeId,
    pub desired: DesiredState,
    pub spec: InstanceSpec,
    pub observed: Option<ObservedInstance>,
}

fn instance_from_row(r: &Row) -> rusqlite::Result<InstanceRow> {
    let conv = |i, e: serde_json::Error| rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e));
    Ok(InstanceRow {
        id: uuid_col(r, 0)?,
        project_id: uuid_col(r, 1)?,
        name: r.get(2)?,
        node_id: uuid_col(r, 3)?,
        desired: text_to_enum(&r.get::<_, String>(4)?)?,
        spec: serde_json::from_str(&r.get::<_, String>(5)?).map_err(|e| conv(5, e))?,
        observed: match r.get::<_, Option<String>>(6)? {
            Some(s) => Some(serde_json::from_str(&s).map_err(|e| conv(6, e))?),
            None => None,
        },
    })
}

const INSTANCE_COLS: &str = "id, project_id, name, node_id, desired_state, spec_json, observed_json";

pub fn insert_instance(conn: &Connection, row: &InstanceRow) -> rusqlite::Result<()> {
    conn.execute(
        &format!("INSERT INTO instances ({INSTANCE_COLS}, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)"),
        params![
            row.id.to_string(),
            row.project_id.to_string(),
            row.name,
            row.node_id.to_string(),
            enum_to_text(&row.desired),
            serde_json::to_string(&row.spec).unwrap(),
            now()
        ],
    )?;
    Ok(())
}

pub fn instance(conn: &Connection, id: InstanceId) -> rusqlite::Result<Option<InstanceRow>> {
    conn.query_row(&format!("SELECT {INSTANCE_COLS} FROM instances WHERE id = ?1"), [id.to_string()], instance_from_row)
        .optional()
}

pub fn instance_by_name(conn: &Connection, project: Uuid, name: &str) -> rusqlite::Result<Option<InstanceRow>> {
    conn.query_row(
        &format!("SELECT {INSTANCE_COLS} FROM instances WHERE project_id = ?1 AND name = ?2"),
        params![project.to_string(), name],
        instance_from_row,
    )
    .optional()
}

pub fn list_instances_in_project(conn: &Connection, project: Uuid) -> rusqlite::Result<Vec<InstanceRow>> {
    conn.prepare(&format!("SELECT {INSTANCE_COLS} FROM instances WHERE project_id = ?1 ORDER BY name"))?
        .query_map([project.to_string()], instance_from_row)?
        .collect()
}

pub fn list_instances_on_node(conn: &Connection, node: NodeId) -> rusqlite::Result<Vec<InstanceRow>> {
    conn.prepare(&format!("SELECT {INSTANCE_COLS} FROM instances WHERE node_id = ?1 ORDER BY created_at"))?
        .query_map([node.to_string()], instance_from_row)?
        .collect()
}

pub fn set_desired(conn: &Connection, id: InstanceId, desired: DesiredState) -> rusqlite::Result<()> {
    conn.execute("UPDATE instances SET desired_state = ?1 WHERE id = ?2", params![enum_to_text(&desired), id.to_string()])?;
    Ok(())
}

pub fn set_observed(conn: &Connection, obs: &ObservedInstance) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE instances SET observed_state = ?1, observed_json = ?2 WHERE id = ?3",
        params![enum_to_text(&obs.state), serde_json::to_string(obs).unwrap(), obs.id.to_string()],
    )?;
    Ok(())
}

pub fn delete_instance(conn: &Connection, id: InstanceId) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM instances WHERE id = ?1", [id.to_string()])?;
    Ok(())
}

/// Mark every instance on a node as failed with the given reason. Used when a node is retired.
pub fn fail_instances_on_node(conn: &Connection, node: NodeId, reason: &str) -> rusqlite::Result<()> {
    for row in list_instances_on_node(conn, node)? {
        let obs = ObservedInstance { id: row.id, state: ObservedState::Failed, error: Some(reason.to_string()), addresses: vec![] };
        set_observed(conn, &obs)?;
    }
    Ok(())
}
