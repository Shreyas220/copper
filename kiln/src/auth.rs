//! Who is asking. Two ways in, one result:
//!
//! - `Authorization: Bearer cu_...`: API tokens, used by `cu` and scripts.
//! - The `copper_session` cookie: a browser signed in with a password.
//!
//! Secrets are only ever stored as SHA-256 hashes; passwords as Argon2id.
//! Cookie requests that change state must also send the `x-copper` header,
//! which a cross-site page cannot add without a CORS preflight kiln never
//! grants.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::{request::Parts, HeaderMap, Method};
use copper_api::{Role, CSRF_HEADER, SESSION_COOKIE};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::db::{self, now, User};
use crate::error::Error;
use crate::AppState;

/// Sessions slide: each use pushes expiry this far out.
pub const SESSION_LIFETIME_SECS: i64 = 30 * 24 * 3600;
/// Last-used timestamps are only rewritten this often, to keep reads cheap.
const TOUCH_EVERY_SECS: i64 = 60;

pub fn hash(secret: &str) -> String {
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    format!("{:x}", h.finalize())
}

/// Create a user if needed and mint a named API token for them.
pub fn issue_token(conn: &Connection, user: &str, role: Option<Role>, name: &str) -> anyhow::Result<String> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let user = db::ensure_user(&tx, user, role)?;
    let token = new_token(&tx, user.id, name)?.1;
    tx.commit()?;
    Ok(token)
}

/// Returns the token's id and its secret, which is never stored.
pub fn new_token(conn: &Connection, user: db::Uuid, name: &str) -> rusqlite::Result<(String, String)> {
    let secret = format!("cu_{}", db::random_hex(32));
    let id = db::insert_token(conn, &hash(&secret), user, name)?;
    Ok((id, secret))
}

// ---- passwords -------------------------------------------------------------

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::encode_b64(db::random_hex(16).as_bytes()).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    Ok(Argon2::default().hash_password(password.as_bytes(), &salt).map_err(|e| anyhow::anyhow!("hashing password: {e}"))?.to_string())
}

/// Check a password against a user's stored hash, as read with
/// `db::password_hash_of`. Takes no lock: hashing is slow on purpose.
/// Unknown users and users without a password still cost one hash, so
/// timing doesn't reveal names.
pub fn check_password(found: Option<(User, Option<String>)>, password: &str) -> anyhow::Result<Option<User>> {
    const DUMMY: &str = "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let stored = found.as_ref().and_then(|(_, h)| h.clone()).unwrap_or_else(|| DUMMY.to_string());
    let parsed = PasswordHash::new(&stored).map_err(|e| anyhow::anyhow!("stored password hash: {e}"))?;
    let ok = Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok();
    Ok(match found {
        Some((user, Some(_))) if ok => Some(user),
        _ => None,
    })
}

// ---- browser sessions --------------------------------------------------------

/// Start a session and return the `Set-Cookie` value carrying it.
pub fn start_session(conn: &Connection, user: db::Uuid) -> rusqlite::Result<String> {
    let secret = format!("cs_{}", db::random_hex(32));
    let now = now();
    db::insert_session(conn, &hash(&secret), user, now, now + SESSION_LIFETIME_SECS)?;
    Ok(format!("{SESSION_COOKIE}={secret}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_LIFETIME_SECS}"))
}

/// A `Set-Cookie` value that removes the session cookie.
pub fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

pub fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers.get_all("cookie").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k == SESSION_COOKIE && !v.is_empty()).then_some(v)
    })
}

// ---- the extractor -----------------------------------------------------------

/// The authenticated user. Fails with 401 when nothing valid was presented.
pub struct Auth(pub User);

impl Auth {
    pub fn require_admin(&self) -> crate::error::Result<()> {
        if self.is_admin() { Ok(()) } else { Err(Error::forbidden()) }
    }
    pub fn is_admin(&self) -> bool {
        self.0.role == Role::Admin
    }
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Error> {
        let conn = state.db.lock().unwrap();
        let now = now();
        if let Ok(token) = bearer(&parts.headers) {
            let token_hash = hash(token);
            let (user, last_used) = db::token_user(&conn, &token_hash)?.ok_or_else(Error::unauthorized)?;
            if last_used.is_none_or(|t| now - t >= TOUCH_EVERY_SECS) {
                db::touch_token(&conn, &token_hash, now)?;
            }
            return Ok(Auth(user));
        }
        let cookie = session_cookie(&parts.headers).ok_or_else(Error::unauthorized)?;
        let changes_state = !matches!(parts.method, Method::GET | Method::HEAD);
        if changes_state && parts.headers.get(CSRF_HEADER).is_none() {
            return Err(Error::forbidden());
        }
        let session_hash = hash(cookie);
        let (user, last_seen) = db::session_user(&conn, &session_hash, now)?.ok_or_else(Error::unauthorized)?;
        if now - last_seen >= TOUCH_EVERY_SECS {
            db::touch_session(&conn, &session_hash, now, now + SESSION_LIFETIME_SECS)?;
        }
        Ok(Auth(user))
    }
}

pub fn bearer(headers: &HeaderMap) -> crate::error::Result<&str> {
    headers.get("authorization").and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer ")).filter(|token| !token.is_empty())
        .ok_or_else(Error::unauthorized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_verify_and_unknown_users_fail_alike() {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let user = db::ensure_user(&conn, "sam", None).unwrap();
        let check = |name: &str, password: &str| check_password(db::password_hash_of(&conn, name).unwrap(), password).unwrap();
        assert!(check("sam", "whatever-long").is_none(), "no password set yet");
        db::set_password_hash(&conn, user.id, &hash_password("correct horse").unwrap()).unwrap();
        assert_eq!(check("sam", "correct horse").unwrap().name, "sam");
        assert!(check("sam", "wrong horse").is_none());
        assert!(check("nobody", "correct horse").is_none());
    }

    #[test]
    fn session_cookie_is_found_among_others() {
        let mut h = HeaderMap::new();
        h.insert("cookie", "theme=ink; copper_session=cs_abc; other=1".parse().unwrap());
        assert_eq!(session_cookie(&h), Some("cs_abc"));
        h.insert("cookie", "copper_session=".parse().unwrap());
        assert_eq!(session_cookie(&h), None);
    }
}
