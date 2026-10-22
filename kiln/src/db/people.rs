//! Users, browser sessions, invites and API tokens.

use copper_api::Role;
use rusqlite::{params, Connection, OptionalExtension};

use super::{enum_to_text, text_to_enum, user_from_row, Uuid, User};

// ---- passwords and sessions ----------------------------------------------------

pub fn set_password_hash(conn: &Connection, user: Uuid, hash: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE users SET password_hash = ?1 WHERE id = ?2", params![hash, user.to_string()])?;
    Ok(())
}

/// The user and their stored password hash, if they have one.
pub fn password_hash_of(conn: &Connection, name: &str) -> rusqlite::Result<Option<(User, Option<String>)>> {
    conn.query_row("SELECT id, name, role, password_hash FROM users WHERE name = ?1", [name], |r| Ok((user_from_row(r)?, r.get(3)?)))
        .optional()
}

pub fn insert_session(conn: &Connection, hash: &str, user: Uuid, now: i64, expires_at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sessions (hash, user_id, created_at, expires_at, last_seen) VALUES (?1, ?2, ?3, ?4, ?3)",
        params![hash, user.to_string(), now, expires_at],
    )?;
    Ok(())
}

/// The user behind an unexpired session, and when the session was last used.
pub fn session_user(conn: &Connection, hash: &str, now: i64) -> rusqlite::Result<Option<(User, i64)>> {
    conn.query_row(
        "SELECT u.id, u.name, u.role, s.last_seen FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.hash = ?1 AND s.expires_at > ?2",
        params![hash, now],
        |r| Ok((user_from_row(r)?, r.get(3)?)),
    )
    .optional()
}

/// Sliding expiry: a session lives as long as it keeps being used.
pub fn touch_session(conn: &Connection, hash: &str, now: i64, expires_at: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE sessions SET last_seen = ?1, expires_at = ?2 WHERE hash = ?3", params![now, expires_at, hash])?;
    Ok(())
}

pub fn delete_session(conn: &Connection, hash: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM sessions WHERE hash = ?1", [hash])?;
    Ok(())
}

pub fn delete_expired_sessions(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now])
}

// ---- invites -------------------------------------------------------------------

pub struct InviteRow {
    pub name: Option<String>,
    pub role: Role,
    pub project: Option<String>,
    pub expires_at: i64,
}

pub fn insert_invite(conn: &Connection, hash: &str, invite: &InviteRow, created_by: Option<Uuid>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO invites (hash, name, role, project, created_by, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![hash, invite.name, enum_to_text(&invite.role), invite.project, created_by.map(|u| u.to_string()), invite.expires_at],
    )?;
    Ok(())
}

pub fn invite(conn: &Connection, hash: &str, now: i64) -> rusqlite::Result<Option<InviteRow>> {
    conn.query_row(
        "SELECT name, role, project, expires_at FROM invites WHERE hash = ?1 AND expires_at > ?2",
        params![hash, now],
        |r| Ok(InviteRow { name: r.get(0)?, role: text_to_enum(&r.get::<_, String>(1)?)?, project: r.get(2)?, expires_at: r.get(3)? }),
    )
    .optional()
}

pub fn delete_invite(conn: &Connection, hash: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM invites WHERE hash = ?1", [hash])?;
    Ok(())
}

// ---- tokens --------------------------------------------------------------------

/// The user behind a token, and when the token was last used.
pub fn token_user(conn: &Connection, hash: &str) -> rusqlite::Result<Option<(User, Option<i64>)>> {
    conn.query_row(
        "SELECT u.id, u.name, u.role, t.last_used_at FROM tokens t JOIN users u ON u.id = t.user_id WHERE t.hash = ?1",
        [hash],
        |r| Ok((user_from_row(r)?, r.get(3)?)),
    )
    .optional()
}

pub fn touch_token(conn: &Connection, hash: &str, now: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE tokens SET last_used_at = ?1 WHERE hash = ?2", params![now, hash])?;
    Ok(())
}

pub struct TokenRow {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub created_at: i64,
    pub last_used: Option<i64>,
}

/// Every token, or only one user's.
pub fn list_tokens(conn: &Connection, owner: Option<Uuid>) -> rusqlite::Result<Vec<TokenRow>> {
    conn.prepare(
        "SELECT t.id, t.name, u.name, t.created_at, t.last_used_at FROM tokens t JOIN users u ON u.id = t.user_id
         WHERE ?1 IS NULL OR t.user_id = ?1 ORDER BY u.name, t.created_at",
    )?
    .query_map([owner.map(|o| o.to_string())], |r| {
        Ok(TokenRow { id: r.get(0)?, name: r.get(1)?, owner: r.get(2)?, created_at: r.get(3)?, last_used: r.get(4)? })
    })?
    .collect()
}

/// Revoke a token, restricted to one owner unless `owner` is `None`.
pub fn delete_token(conn: &Connection, id: &str, owner: Option<Uuid>) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM tokens WHERE id = ?1 AND (?2 IS NULL OR user_id = ?2)", params![id, owner.map(|o| o.to_string())])? == 1)
}

// ---- users ---------------------------------------------------------------------

pub struct UserRow {
    pub user: User,
    pub has_password: bool,
    pub projects: Vec<String>,
    pub last_seen: Option<i64>,
}

pub fn list_users(conn: &Connection) -> rusqlite::Result<Vec<UserRow>> {
    let mut users: Vec<UserRow> = conn
        .prepare(
            "SELECT u.id, u.name, u.role, u.password_hash IS NOT NULL,
                    MAX(COALESCE((SELECT MAX(last_used_at) FROM tokens WHERE user_id = u.id), 0),
                        COALESCE((SELECT MAX(last_seen) FROM sessions WHERE user_id = u.id), 0))
             FROM users u ORDER BY u.name",
        )?
        .query_map([], |r| {
            let seen: i64 = r.get(4)?;
            Ok(UserRow { user: user_from_row(r)?, has_password: r.get(3)?, projects: vec![], last_seen: (seen > 0).then_some(seen) })
        })?
        .collect::<rusqlite::Result<_>>()?;
    for row in &mut users {
        row.projects = conn
            .prepare("SELECT name FROM projects WHERE owner_id = ?1 ORDER BY name")?
            .query_map([row.user.id.to_string()], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
    }
    Ok(users)
}

pub fn set_role(conn: &Connection, user: Uuid, role: Role) -> rusqlite::Result<()> {
    conn.execute("UPDATE users SET role = ?1 WHERE id = ?2", params![enum_to_text(&role), user.to_string()])?;
    Ok(())
}

pub fn admin_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM users WHERE role = 'admin'", [], |r| r.get(0))
}

/// Remove a user and everything that only exists for them. The caller must
/// have checked they hold no instances; their empty projects go too.
pub fn delete_user(conn: &Connection, user: Uuid) -> rusqlite::Result<()> {
    let id = user.to_string();
    conn.execute("DELETE FROM sessions WHERE user_id = ?1", [&id])?;
    conn.execute("DELETE FROM tokens WHERE user_id = ?1", [&id])?;
    conn.execute("DELETE FROM ssh_keys WHERE user_id = ?1", [&id])?;
    conn.execute("DELETE FROM invites WHERE created_by = ?1", [&id])?;
    conn.execute("DELETE FROM projects WHERE owner_id = ?1", [&id])?;
    conn.execute("DELETE FROM users WHERE id = ?1", [&id])?;
    Ok(())
}

pub fn instance_count_of(conn: &Connection, user: Uuid) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM instances WHERE project_id IN (SELECT id FROM projects WHERE owner_id = ?1)",
        [user.to_string()],
        |r| r.get(0),
    )
}
