//! Bearer tokens, stored as SHA-256 hashes. Admins issue them with `kiln token`.

use axum::extract::FromRequestParts;
use axum::http::{HeaderMap, request::Parts};
use copper_api::Role;
use rand::RngCore;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::db::{self, User};
use crate::error::Error;
use crate::AppState;

pub fn hash(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    format!("{:x}", h.finalize())
}

pub fn issue_token(conn: &Connection, user: &str, role: Option<Role>) -> anyhow::Result<String> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let user = db::ensure_user(&tx, user, role)?;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = format!("cu_{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    db::insert_token(&tx, &hash(&token), user.id)?;
    tx.commit()?;
    Ok(token)
}

/// Extractor: the authenticated user. Fails with 401 when the token is missing or unknown.
pub struct Auth(pub User);

impl Auth {
    pub fn require_admin(&self) -> crate::error::Result<()> {
        if self.0.role == Role::Admin { Ok(()) } else { Err(Error::forbidden()) }
    }
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Error> {
        let token = bearer(&parts.headers)?;
        let conn = state.db.lock().unwrap();
        db::user_by_token_hash(&conn, &hash(token))?.map(Auth).ok_or_else(Error::unauthorized)
    }
}

pub fn bearer(headers: &HeaderMap) -> crate::error::Result<&str> {
    headers.get("authorization").and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer ")).filter(|token| !token.is_empty())
        .ok_or_else(Error::unauthorized)
}
