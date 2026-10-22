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
    "ALTER TABLE nodes ADD COLUMN token_hash TEXT;
     UPDATE nodes SET status = 'pending', last_seen = NULL WHERE status = 'approved';",
    // v3: image catalogue, per-node overcommit, spread groups.
    r#"
    CREATE TABLE images (
        name       TEXT NOT NULL,
        arch       TEXT NOT NULL,
        sha256     TEXT NOT NULL,
        size_bytes INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (name, arch)
    );
    ALTER TABLE nodes ADD COLUMN cpu_ratio REAL NOT NULL DEFAULT 1.0;
    ALTER TABLE nodes ADD COLUMN memory_ratio REAL NOT NULL DEFAULT 1.0;
    ALTER TABLE nodes ADD COLUMN disk_ratio REAL NOT NULL DEFAULT 1.0;
    ALTER TABLE instances ADD COLUMN spread TEXT;
    "#,
    // v4: the console. Browser sign-in, invites, named tokens, quotas, the
    // event log, and remembering which nodes have gone quiet.
    r#"
    ALTER TABLE users ADD COLUMN password_hash TEXT;
    CREATE TABLE sessions (
        hash       TEXT PRIMARY KEY,
        user_id    TEXT NOT NULL REFERENCES users(id),
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        last_seen  INTEGER NOT NULL
    );
    CREATE TABLE invites (
        hash       TEXT PRIMARY KEY,
        name       TEXT,                      -- fixed name, or NULL to let them choose
        role       TEXT NOT NULL,
        project    TEXT,                      -- project to create for them
        created_by TEXT REFERENCES users(id),    -- NULL when made with `kiln invite`
        expires_at INTEGER NOT NULL
    );
    ALTER TABLE tokens ADD COLUMN id TEXT;
    ALTER TABLE tokens ADD COLUMN name TEXT NOT NULL DEFAULT 'cli';
    ALTER TABLE tokens ADD COLUMN last_used_at INTEGER;
    UPDATE tokens SET id = lower(hex(randomblob(8)));
    CREATE UNIQUE INDEX tokens_by_id ON tokens(id);
    ALTER TABLE projects ADD COLUMN quota_vcpus INTEGER;
    ALTER TABLE projects ADD COLUMN quota_memory_mb INTEGER;
    ALTER TABLE projects ADD COLUMN quota_disk_gb INTEGER;
    ALTER TABLE instances ADD COLUMN running_since INTEGER;
    ALTER TABLE nodes ADD COLUMN quiet INTEGER NOT NULL DEFAULT 0;
    CREATE TABLE events (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        at            INTEGER NOT NULL,
        kind          TEXT NOT NULL,
        detail        TEXT NOT NULL,
        project_id    TEXT,
        instance_id   TEXT,
        instance_name TEXT,                   -- kept after the instance is gone
        node_id       TEXT,
        actor         TEXT
    );
    CREATE INDEX events_by_instance ON events(instance_id, id);
    CREATE INDEX events_by_project ON events(project_id, id);
    "#,
    // v5: saved SSH public keys, used when a create names none.
    r#"
    CREATE TABLE ssh_keys (
        id          TEXT PRIMARY KEY,
        user_id     TEXT NOT NULL REFERENCES users(id),
        name        TEXT NOT NULL,
        public_key  TEXT NOT NULL,
        fingerprint TEXT NOT NULL,
        created_at  INTEGER NOT NULL,
        UNIQUE (user_id, fingerprint)
    );
    "#,
    // v6: copper networks. A node's subnet is N in 10.42.N.0/24, given when
    // it's approved and never reused; nodes approved before now are
    // numbered in the order they joined.
    r#"
    ALTER TABLE nodes ADD COLUMN subnet INTEGER;
    ALTER TABLE nodes ADD COLUMN network_ready INTEGER;
    ALTER TABLE nodes ADD COLUMN tailnet_route INTEGER;
    UPDATE nodes SET subnet = (SELECT COUNT(*) FROM nodes AS n WHERE n.status != 'pending' AND n.rowid <= nodes.rowid)
        WHERE status != 'pending';
    CREATE UNIQUE INDEX nodes_by_subnet ON nodes(subnet);
    "#,
];

mod events;
mod keys;
mod people;
pub use events::*;
pub use keys::*;
pub use people::*;

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
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let current: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    anyhow::ensure!(current >= 0 && current as usize <= MIGRATIONS.len(), "unsupported database schema version {current}");
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = i as i64 + 1;
        tracing::info!(version, "applying migration");
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
    }
    tx.commit()?;
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

pub fn ensure_user(conn: &Connection, name: &str, role: Option<Role>) -> rusqlite::Result<User> {
    if let Some(u) = user_by_name(conn, name)? {
        let role = role.unwrap_or(u.role);
        if u.role != role {
            conn.execute("UPDATE users SET role = ?1 WHERE id = ?2", params![enum_to_text(&role), u.id.to_string()])?;
        }
        return Ok(User { role, ..u });
    }
    let role = role.unwrap_or(Role::User);
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

/// Store a token's hash under a short random id the owner can revoke it by.
pub fn insert_token(conn: &Connection, hash: &str, user: Uuid, name: &str) -> rusqlite::Result<String> {
    let id = random_hex(8);
    conn.execute(
        "INSERT INTO tokens (hash, user_id, created_at, id, name) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![hash, user.to_string(), now(), id, name],
    )?;
    Ok(id)
}

pub fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---- projects ----------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    #[allow(dead_code)] // part of the row; ownership checks go through names
    pub owner_id: Uuid,
    pub quota: QuotaCaps,
}

const PROJECT_COLS: &str = "p.id, p.name, p.owner_id, p.quota_vcpus, p.quota_memory_mb, p.quota_disk_gb";

fn project_from_row(r: &Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: uuid_col(r, 0)?,
        name: r.get(1)?,
        owner_id: uuid_col(r, 2)?,
        quota: QuotaCaps { vcpus: r.get(3)?, memory_mb: r.get::<_, Option<i64>>(4)?.map(|v| v as u64), disk_gb: r.get::<_, Option<i64>>(5)?.map(|v| v as u64) },
    })
}

pub fn create_project(conn: &Connection, owner: Uuid, name: &str) -> rusqlite::Result<Project> {
    let id = Uuid::new_v4();
    conn.execute(
        "INSERT INTO projects (id, name, owner_id) VALUES (?1, ?2, ?3)",
        params![id.to_string(), name, owner.to_string()],
    )?;
    Ok(Project { id, name: name.to_string(), owner_id: owner, quota: QuotaCaps::default() })
}

pub fn project_by_name(conn: &Connection, owner: Uuid, name: &str) -> rusqlite::Result<Option<Project>> {
    conn.query_row(
        &format!("SELECT {PROJECT_COLS} FROM projects p WHERE p.owner_id = ?1 AND p.name = ?2"),
        params![owner.to_string(), name],
        project_from_row,
    )
    .optional()
}

pub fn project_by_id(conn: &Connection, id: Uuid) -> rusqlite::Result<Option<Project>> {
    conn.query_row(&format!("SELECT {PROJECT_COLS} FROM projects p WHERE p.id = ?1"), [id.to_string()], project_from_row).optional()
}

pub fn list_projects(conn: &Connection, owner: Uuid) -> rusqlite::Result<Vec<Project>> {
    conn.prepare(&format!("SELECT {PROJECT_COLS} FROM projects p WHERE p.owner_id = ?1 ORDER BY p.name"))?
        .query_map([owner.to_string()], project_from_row)?
        .collect()
}

pub fn set_quota(conn: &Connection, project: Uuid, caps: &QuotaCaps) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE projects SET quota_vcpus = ?1, quota_memory_mb = ?2, quota_disk_gb = ?3 WHERE id = ?4",
        params![caps.vcpus, caps.memory_mb.map(|v| v as i64), caps.disk_gb.map(|v| v as i64), project.to_string()],
    )?;
    Ok(())
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
    pub token_hash: Option<String>,
    pub overcommit: Overcommit,
    /// The sweeper logged that this node stopped reporting.
    pub quiet: bool,
    /// N in its copper subnet, `10.42.N.0/24`. `None` until approved.
    pub subnet: Option<u8>,
    /// As the node last reported them; see `ObservedNodeState`.
    pub network_ready: Option<bool>,
    pub tailnet_route: Option<bool>,
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
    pub fn to_api(&self, allocated: Capacity, instances: Vec<String>, usage: Option<Usage>, subnet: Option<String>) -> Node {
        Node {
            subnet,
            network_ready: self.network_ready,
            tailnet_route: self.tailnet_route,
            id: self.id,
            hostname: self.hostname.clone(),
            arch: self.arch,
            status: self.public_status(),
            capacity: Some(self.capacity.clone()),
            allocated: Some(allocated),
            overcommit: self.overcommit,
            fingerprint: self.token_hash.as_deref().map(fingerprint),
            last_seen: self.last_seen,
            generation: self.generation,
            instances,
            usage,
        }
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
        token_hash: r.get(7)?,
        overcommit: Overcommit { cpu: r.get(8)?, memory: r.get(9)?, disk: r.get(10)? },
        quiet: r.get(11)?,
        subnet: r.get(12)?,
        network_ready: r.get(13)?,
        tailnet_route: r.get(14)?,
    })
}

const NODE_COLS: &str = "id, hostname, arch, status, generation, last_seen, capacity_json, token_hash, cpu_ratio, memory_ratio, disk_ratio, quiet, \
                         subnet, network_ready, tailnet_route";

/// The node's subnet number, giving it the next one if it has none yet.
/// Numbers are never reused, so a retired node's addresses never come back.
pub fn ensure_subnet(conn: &Connection, id: NodeId) -> anyhow::Result<u8> {
    if let Some(n) = node(conn, id)?.and_then(|n| n.subnet) {
        return Ok(n);
    }
    let next: i64 = conn.query_row("SELECT COALESCE(MAX(subnet), 0) + 1 FROM nodes", [], |r| r.get(0))?;
    anyhow::ensure!(next <= 254, "copper's network has room for 254 nodes, and all have been used");
    conn.execute("UPDATE nodes SET subnet = ?1 WHERE id = ?2", params![next, id.to_string()])?;
    Ok(next as u8)
}

pub fn set_network_reports(conn: &Connection, id: NodeId, network_ready: Option<bool>, tailnet_route: Option<bool>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE nodes SET network_ready = ?1, tailnet_route = ?2 WHERE id = ?3",
        params![network_ready, tailnet_route, id.to_string()],
    )?;
    Ok(())
}

pub fn set_quiet(conn: &Connection, id: NodeId, quiet: bool) -> rusqlite::Result<()> {
    conn.execute("UPDATE nodes SET quiet = ?1 WHERE id = ?2", params![quiet, id.to_string()])?;
    Ok(())
}

pub fn node(conn: &Connection, id: NodeId) -> rusqlite::Result<Option<NodeRow>> {
    conn.query_row(&format!("SELECT {NODE_COLS} FROM nodes WHERE id = ?1"), [id.to_string()], node_from_row).optional()
}

pub fn list_nodes(conn: &Connection) -> rusqlite::Result<Vec<NodeRow>> {
    conn.prepare(&format!("SELECT {NODE_COLS} FROM nodes ORDER BY hostname"))?
        .query_map([], node_from_row)?
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegisterNodeOutcome {
    Registered(NodeDbStatus),
    CredentialMismatch,
}

/// Pin the credential before approval. Known-node capacity is refreshed only
/// by an observation for the current generation, never by registration.
pub fn register_node(conn: &Connection, req: &RegisterRequest, token_hash: &str) -> anyhow::Result<RegisterNodeOutcome> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let existing = node(&tx, req.node_id)?;
    if existing.as_ref().and_then(|node| node.token_hash.as_deref()).is_some_and(|hash| hash != token_hash) {
        return Ok(RegisterNodeOutcome::CredentialMismatch);
    }
    let status = existing.map_or(NodeDbStatus::Pending, |node| node.status);
    tx.execute(
        "INSERT INTO nodes (id, hostname, arch, status, capacity_json, token_hash)
         VALUES (?1, ?2, ?3, 'pending', ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET hostname = excluded.hostname,
             token_hash = COALESCE(nodes.token_hash, excluded.token_hash)",
        params![req.node_id.to_string(), req.hostname, enum_to_text(&req.arch), serde_json::to_string(&req.capacity)?, token_hash],
    )?;
    tx.commit()?;
    Ok(RegisterNodeOutcome::Registered(status))
}

pub fn set_node_status(conn: &Connection, id: NodeId, status: NodeDbStatus) -> rusqlite::Result<bool> {
    Ok(conn.execute("UPDATE nodes SET status = ?1 WHERE id = ?2", params![enum_to_text(&status), id.to_string()])? == 1)
}

pub fn set_overcommit(conn: &Connection, id: NodeId, oc: &Overcommit) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE nodes SET cpu_ratio = ?1, memory_ratio = ?2, disk_ratio = ?3 WHERE id = ?4",
        params![oc.cpu, oc.memory, oc.disk, id.to_string()],
    )?;
    Ok(())
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
    /// Spread group within the project, used only by placement.
    pub spread: Option<String>,
    pub created_at: i64,
    /// When the node first reported it running after anything else.
    pub running_since: Option<i64>,
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
        spread: r.get(7)?,
        created_at: r.get(8)?,
        running_since: r.get(9)?,
    })
}

const INSTANCE_COLS: &str = "id, project_id, name, node_id, desired_state, spec_json, observed_json, spread, created_at, running_since";

pub fn insert_instance(conn: &Connection, row: &InstanceRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO instances (id, project_id, name, node_id, desired_state, spec_json, spread, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            row.id.to_string(),
            row.project_id.to_string(),
            row.name,
            row.node_id.to_string(),
            enum_to_text(&row.desired),
            serde_json::to_string(&row.spec).unwrap(),
            row.spread,
            row.created_at,
        ],
    )?;
    Ok(())
}

pub fn set_running_since(conn: &Connection, id: InstanceId, since: Option<i64>) -> rusqlite::Result<()> {
    conn.execute("UPDATE instances SET running_since = ?1 WHERE id = ?2", params![since, id.to_string()])?;
    Ok(())
}


pub fn set_spec(conn: &Connection, id: InstanceId, spec: &InstanceSpec) -> rusqlite::Result<()> {
    conn.execute("UPDATE instances SET spec_json = ?1 WHERE id = ?2", params![serde_json::to_string(spec).unwrap(), id.to_string()])?;
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
        let obs = ObservedInstance { id: row.id, state: ObservedState::Failed, error: Some(reason.to_string()), addresses: vec![], usage: None };
        set_observed(conn, &obs)?;
    }
    Ok(())
}

// ---- image catalogue -----------------------------------------------------------

fn image_from_row(r: &Row) -> rusqlite::Result<Image> {
    Ok(Image { name: r.get(0)?, arch: text_to_enum(&r.get::<_, String>(1)?)?, sha256: r.get(2)?, size_bytes: r.get::<_, i64>(3)? as u64 })
}

/// Point a name and arch at a blob. Instances keep the hash they were created
/// with, so re-importing a name never changes existing disks.
pub fn upsert_image(conn: &Connection, image: &Image) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO images (name, arch, sha256, size_bytes, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(name, arch) DO UPDATE SET sha256 = excluded.sha256, size_bytes = excluded.size_bytes, created_at = excluded.created_at",
        params![image.name, enum_to_text(&image.arch), image.sha256, image.size_bytes as i64, now()],
    )?;
    Ok(())
}

pub fn image(conn: &Connection, name: &str, arch: Arch) -> rusqlite::Result<Option<Image>> {
    conn.query_row("SELECT name, arch, sha256, size_bytes FROM images WHERE name = ?1 AND arch = ?2",
        params![name, enum_to_text(&arch)], image_from_row).optional()
}

pub fn list_images(conn: &Connection) -> rusqlite::Result<Vec<Image>> {
    conn.prepare("SELECT name, arch, sha256, size_bytes FROM images ORDER BY name, arch")?
        .query_map([], image_from_row)?.collect()
}

/// A blob may be served if any catalogue entry or any instance refers to it.
pub fn blob_referenced(conn: &Connection, sha256: &str) -> rusqlite::Result<bool> {
    if conn.query_row("SELECT EXISTS(SELECT 1 FROM images WHERE sha256 = ?1)", [sha256], |r| r.get(0))? {
        return Ok(true);
    }
    conn.query_row("SELECT EXISTS(SELECT 1 FROM instances WHERE json_extract(spec_json, '$.boot_disk.sha256') = ?1)", [sha256], |r| r.get(0))
}

pub fn list_all_projects(conn: &Connection) -> rusqlite::Result<Vec<(Project, String)>> {
    conn.prepare(&format!("SELECT {PROJECT_COLS}, u.name FROM projects p JOIN users u ON p.owner_id = u.id ORDER BY u.name, p.name"))?
        .query_map([], |r| Ok((project_from_row(r)?, r.get(6)?)))?.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_failure_rolls_back_schema_and_version() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE tokens (already_here TEXT)").unwrap();
        assert!(migrate(&conn).is_err());
        assert!(conn.is_autocommit());
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 0);
        let nodes: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'nodes')", [], |r| r.get(0)).unwrap();
        assert!(!nodes);
    }

    #[test]
    fn legacy_nodes_require_credential_enrollment_and_reapproval() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        let id = NodeId::new_v4();
        conn.execute("INSERT INTO nodes (id, hostname, arch, status, capacity_json) VALUES (?1, 'legacy', 'x86_64', 'approved', ?2)",
            params![id.to_string(), r#"{"vcpus":8,"memory_mb":16384,"disk_gb":100}"#]).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(node(&conn, id).unwrap().unwrap().status, NodeDbStatus::Pending);
        assert!(node(&conn, id).unwrap().unwrap().token_hash.is_none());
        let req = RegisterRequest { node_id: id, hostname: "legacy".into(), arch: Arch::X86_64,
            capacity: Capacity { vcpus: 8, memory_mb: 16384, disk_gb: 100 } };
        assert_eq!(register_node(&conn, &req, "pinned-hash").unwrap(), RegisterNodeOutcome::Registered(NodeDbStatus::Pending));
        assert_eq!(node(&conn, id).unwrap().unwrap().token_hash.as_deref(), Some("pinned-hash"));
        assert_eq!(register_node(&conn, &req, "different-hash").unwrap(), RegisterNodeOutcome::CredentialMismatch);
    }
}
