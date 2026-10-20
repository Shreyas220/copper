//! SQLite schema and migrations. Applied on every start; idempotent.

use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;

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
         PRAGMA foreign_keys = ON;",
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
