//! SSH public keys people save, so making a VM doesn't mean pasting one.
//!
//! Keys are personal: everyone, admins included, sees and manages only their
//! own. A create that names no keys gets the caller's saved ones. Private keys
//! never come here; `cu key new` and the console both make the key pair where
//! the person is and send only the public half.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use copper_api::*;
use sha2::{Digest, Sha256};

use crate::auth::Auth;
use crate::db::{self, KeyRow, NewEvent};
use crate::error::{Error, Result};
use crate::AppState;

/// Key types sshd on our images accepts in authorized_keys.
const KINDS: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/keys", get(list_keys).post(add_key))
        .route("/v1/keys/{id}", delete(remove_key))
}

/// A public key line taken apart.
pub struct Parsed<'a> {
    pub comment: Option<&'a str>,
    pub fingerprint: String,
}

/// One line as `ssh-keygen` writes it to `~/.ssh/id_*.pub`: a known type,
/// then base64 whose first field names the same type, then an optional
/// comment. Anything else, a private key above all, is refused.
pub fn parse(line: &str) -> Option<Parsed<'_>> {
    if line.contains(['\n', '\r']) {
        return None;
    }
    let mut parts = line.trim().splitn(3, char::is_whitespace);
    let (kind, blob) = (parts.next()?, parts.next()?);
    let comment = parts.next().map(str::trim).filter(|c| !c.is_empty());
    if !KINDS.contains(&kind) {
        return None;
    }
    let bytes = STANDARD.decode(blob).ok()?;
    let named = bytes.get(..4).map(|n| u32::from_be_bytes(n.try_into().unwrap()) as usize)?;
    if bytes.get(4..4 + named)? != kind.as_bytes() {
        return None;
    }
    Some(Parsed { comment, fingerprint: format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&bytes))) })
}

pub fn validate(keys: &[String]) -> Result<()> {
    match keys.iter().any(|k| parse(k).is_none()) {
        true => Err(Error::bad_request("ssh keys must be OpenSSH public keys, one line each, like ~/.ssh/id_ed25519.pub")),
        false => Ok(()),
    }
}

fn to_api(k: KeyRow) -> SshKey {
    SshKey { id: k.id, name: k.name, public_key: k.public_key, fingerprint: k.fingerprint, created_at: k.created_at }
}

async fn list_keys(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<SshKey>>> {
    let conn = state.db.lock().unwrap();
    Ok(Json(db::list_keys(&conn, auth.0.id)?.into_iter().map(to_api).collect()))
}

async fn add_key(State(state): State<AppState>, auth: Auth, Json(req): Json<AddSshKey>) -> Result<(StatusCode, Json<SshKey>)> {
    let line = req.public_key.trim();
    let parsed = parse(line).ok_or_else(|| Error::bad_request("that isn't an OpenSSH public key; paste one line, like ~/.ssh/id_ed25519.pub"))?;
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).or(parsed.comment).unwrap_or("key");
    if name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(Error::bad_request("give the key a name of 1-64 characters"));
    }
    let conn = state.db.lock().unwrap();
    if let Some(have) = db::key_by_fingerprint(&conn, auth.0.id, &parsed.fingerprint)? {
        return Err(Error::conflict(format!("you already saved this key, as {}", have.name)));
    }
    let key = KeyRow { id: db::random_hex(8), name: name.to_string(), public_key: line.to_string(), fingerprint: parsed.fingerprint, created_at: db::now() };
    db::insert_key(&conn, auth.0.id, &key)?;
    db::record(&conn, NewEvent::new("ssh key", format!("saved {} · {}", key.name, key.fingerprint)).by(&auth.0.name))?;
    Ok((StatusCode::CREATED, Json(to_api(key))))
}

/// VMs that already have the key keep it: keys are only written at first boot.
async fn remove_key(State(state): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<StatusCode> {
    let conn = state.db.lock().unwrap();
    if !db::delete_key(&conn, auth.0.id, &id)? {
        return Err(Error::not_found("ssh key"));
    }
    db::record(&conn, NewEvent::new("ssh key", format!("removed {id}")).by(&auth.0.name))?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFj/NbTgjOM0L/Sk0uGgwW9NGFVuPaS91tElolgH/nmC";

    #[test]
    fn fingerprints_match_ssh_keygen() {
        // `ssh-keygen -lf` prints this fingerprint for KEY.
        let line = format!("{KEY} alice@laptop");
        let parsed = parse(&line).unwrap();
        assert_eq!(parsed.comment, Some("alice@laptop"));
        assert_eq!(parsed.fingerprint, "SHA256:ARQPlI2pp175E3pLrJpmwPKuVA/HFV/dmIQTvtceRa8");
    }

    #[test]
    fn refuses_what_isnt_a_public_key() {
        assert!(parse(KEY).is_some(), "the comment is optional");
        assert!(parse("-----BEGIN OPENSSH PRIVATE KEY-----").is_none());
        assert!(parse(&format!("{KEY}\nruncmd: [reboot]")).is_none());
        assert!(parse(&KEY.replace("ssh-ed25519", "ssh-rsa")).is_none(), "type must match the blob");
        assert!(parse("ssh-ed25519 not-base64!").is_none());
        assert!(parse("ssh-dss AAAAB3NzaC1kc3M=").is_none());
    }
}
