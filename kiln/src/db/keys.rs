//! SSH public keys people have saved for their VMs.

use rusqlite::{params, Connection, OptionalExtension};

use super::Uuid;

pub struct KeyRow {
    pub id: String,
    pub name: String,
    pub public_key: String,
    pub fingerprint: String,
    pub created_at: i64,
}

fn key_from_row(r: &rusqlite::Row) -> rusqlite::Result<KeyRow> {
    Ok(KeyRow { id: r.get(0)?, name: r.get(1)?, public_key: r.get(2)?, fingerprint: r.get(3)?, created_at: r.get(4)? })
}

pub fn insert_key(conn: &Connection, user: Uuid, key: &KeyRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO ssh_keys (id, user_id, name, public_key, fingerprint, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![key.id, user.to_string(), key.name, key.public_key, key.fingerprint, key.created_at],
    )?;
    Ok(())
}

/// Oldest first, so the key someone saved first stays first.
pub fn list_keys(conn: &Connection, user: Uuid) -> rusqlite::Result<Vec<KeyRow>> {
    conn.prepare("SELECT id, name, public_key, fingerprint, created_at FROM ssh_keys WHERE user_id = ?1 ORDER BY created_at, id")?
        .query_map([user.to_string()], key_from_row)?
        .collect()
}

pub fn key_by_fingerprint(conn: &Connection, user: Uuid, fingerprint: &str) -> rusqlite::Result<Option<KeyRow>> {
    conn.query_row(
        "SELECT id, name, public_key, fingerprint, created_at FROM ssh_keys WHERE user_id = ?1 AND fingerprint = ?2",
        params![user.to_string(), fingerprint],
        key_from_row,
    )
    .optional()
}

/// Forget one of a user's keys. VMs that already have it keep it.
pub fn delete_key(conn: &Connection, user: Uuid, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM ssh_keys WHERE id = ?1 AND user_id = ?2", params![id, user.to_string()])? == 1)
}
