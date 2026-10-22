//! People: signing in, invites, users and API tokens.
//!
//! Nobody signs up. An admin makes an invite link; opening it lets the person
//! pick a password (and, if the invite allows, their name). The same kind of
//! link resets a forgotten password. The first admin's link comes from
//! `kiln invite --role admin` on the kiln machine.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header::SET_COOKIE, StatusCode};
use axum::response::{AppendHeaders, IntoResponse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use copper_api::*;

use crate::auth::{self, Auth};
use crate::db::{self, InviteRow, NewEvent};
use crate::error::{Error, Result};
use crate::user_api::validate_name;
use crate::AppState;

const INVITE_LIFETIME_SECS: i64 = 24 * 3600;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/me", get(me))
        .route("/v1/session", post(sign_in).delete(sign_out))
        .route("/v1/invites", post(create_invite))
        .route("/v1/invites/{code}", get(show_invite))
        .route("/v1/invites/{code}/accept", post(accept_invite))
        .route("/v1/users", get(list_users))
        .route("/v1/users/{name}", delete(remove_user))
        .route("/v1/users/{name}/role", post(set_role))
        .route("/v1/tokens", get(list_tokens).post(create_token))
        .route("/v1/tokens/{id}", delete(revoke_token))
}

async fn me(Auth(user): Auth) -> Json<Me> {
    Json(Me { name: user.name, role: user.role })
}

// ---- sessions ------------------------------------------------------------------

async fn sign_in(State(state): State<AppState>, Json(req): Json<SignIn>) -> Result<impl IntoResponse> {
    let stored = db::password_hash_of(&state.db.lock().unwrap(), &req.name)?;
    let password = req.password;
    let verified = tokio::task::spawn_blocking(move || auth::check_password(stored, &password)).await.map_err(anyhow::Error::from)??;
    let Some(user) = verified else {
        // Slow down guessing; the hash already cost a few tens of ms.
        tokio::time::sleep(Duration::from_secs(1)).await;
        return Err(Error(StatusCode::UNAUTHORIZED, "wrong name or password".into()));
    };
    let conn = state.db.lock().unwrap();
    let cookie = auth::start_session(&conn, user.id)?;
    tracing::info!(user = %user.name, "signed in");
    Ok((AppendHeaders([(SET_COOKIE, cookie)]), Json(Me { name: user.name, role: user.role })))
}

async fn sign_out(State(state): State<AppState>, _auth: Auth, headers: axum::http::HeaderMap) -> Result<impl IntoResponse> {
    if let Some(secret) = auth::session_cookie(&headers) {
        db::delete_session(&state.db.lock().unwrap(), &auth::hash(secret))?;
    }
    Ok((AppendHeaders([(SET_COOKIE, auth::clear_session_cookie())]), StatusCode::NO_CONTENT))
}

// ---- invites -------------------------------------------------------------------

/// Make a one-time link. With `name` naming an existing user, the link resets
/// their password instead of adding someone.
/// `by` is `None` for invites made on the kiln machine with `kiln invite`.
pub fn make_invite(conn: &rusqlite::Connection, req: &CreateInvite, by: Option<&db::User>) -> Result<Invite> {
    if let Some(name) = &req.name {
        validate_name(name)?;
    }
    if let Some(project) = &req.project {
        validate_name(project)?;
    }
    let existing = match &req.name {
        Some(name) => db::user_by_name(conn, name)?,
        None => None,
    };
    let role = req.role.or(existing.as_ref().map(|u| u.role)).unwrap_or(Role::User);
    let code = db::random_hex(16);
    let row = InviteRow { name: req.name.clone(), role, project: req.project.clone(), expires_at: db::now() + INVITE_LIFETIME_SECS };
    db::insert_invite(conn, &auth::hash(&code), &row, by.map(|u| u.id))?;
    let detail = match (&req.name, existing.is_some()) {
        (Some(name), true) => format!("password reset link for {name}"),
        (Some(name), false) => format!("invite for {name} as {}", role_name(role)),
        (None, _) => format!("invite as {}", role_name(role)),
    };
    db::record(conn, NewEvent::new("invited", detail).by(by.map_or("kiln", |u| u.name.as_str())))?;
    Ok(Invite { code, expires_at: row.expires_at, name: row.name, role, project: row.project, reset: existing.is_some() })
}

async fn create_invite(State(state): State<AppState>, auth: Auth, Json(req): Json<CreateInvite>) -> Result<(StatusCode, Json<Invite>)> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    Ok((StatusCode::CREATED, Json(make_invite(&conn, &req, Some(&auth.0))?)))
}

/// What the invite page shows before anyone types a password.
async fn show_invite(State(state): State<AppState>, Path(code): Path<String>) -> Result<Json<Invite>> {
    let conn = state.db.lock().unwrap();
    let row = db::invite(&conn, &auth::hash(&code), db::now())?.ok_or_else(|| Error::not_found("invite"))?;
    let reset = match &row.name {
        Some(name) => db::user_by_name(&conn, name)?.is_some(),
        None => false,
    };
    Ok(Json(Invite { code, expires_at: row.expires_at, name: row.name, role: row.role, project: row.project, reset }))
}

async fn accept_invite(State(state): State<AppState>, Path(code): Path<String>, Json(req): Json<AcceptInvite>) -> Result<impl IntoResponse> {
    if req.password.chars().count() < MIN_PASSWORD_LEN {
        return Err(Error::bad_request(format!("use at least {MIN_PASSWORD_LEN} characters")));
    }
    let password_hash = auth::hash_password(&req.password)?;
    let conn = state.db.lock().unwrap();
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    let invite_hash = auth::hash(&code);
    let invite = db::invite(&tx, &invite_hash, db::now())?.ok_or_else(|| Error::not_found("invite"))?;
    let name = invite.name.clone().unwrap_or_else(|| req.name.trim().to_string());
    validate_name(&name)?;
    let user = match db::user_by_name(&tx, &name)? {
        Some(existing) if invite.name.as_deref() == Some(name.as_str()) => existing,
        Some(_) => return Err(Error::conflict("that name is taken")),
        None => db::ensure_user(&tx, &name, Some(invite.role))?,
    };
    db::set_password_hash(&tx, user.id, &password_hash)?;
    if let Some(project) = &invite.project {
        if db::project_by_name(&tx, user.id, project)?.is_none() {
            db::create_project(&tx, user.id, project)?;
        }
    }
    db::delete_invite(&tx, &invite_hash)?;
    db::record(&tx, NewEvent::new("joined", format!("{} set a password", user.name)).by(&user.name))?;
    let cookie = auth::start_session(&tx, user.id)?;
    tx.commit()?;
    Ok((AppendHeaders([(SET_COOKIE, cookie)]), Json(Me { name: user.name, role: user.role })))
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        Role::User => "user",
    }
}

// ---- users ---------------------------------------------------------------------

async fn list_users(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<User>>> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    Ok(Json(
        db::list_users(&conn)?
            .into_iter()
            .map(|u| User { name: u.user.name, role: u.user.role, projects: u.projects, last_seen: u.last_seen, has_password: u.has_password })
            .collect(),
    ))
}

async fn set_role(State(state): State<AppState>, auth: Auth, Path(name): Path<String>, Json(req): Json<SetRole>) -> Result<StatusCode> {
    auth.require_admin()?;
    let conn = state.db.lock().unwrap();
    let user = db::user_by_name(&conn, &name)?.ok_or_else(|| Error::not_found("user"))?;
    if user.role == Role::Admin && req.role != Role::Admin && db::admin_count(&conn)? <= 1 {
        return Err(Error::conflict("copper needs at least one admin"));
    }
    db::set_role(&conn, user.id, req.role)?;
    db::record(&conn, NewEvent::new("role", format!("{name} is now {}", role_name(req.role))).by(&auth.0.name))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removing someone revokes their tokens and sessions and drops their empty
/// projects. Their VMs must be deleted first: delete is never implied.
async fn remove_user(State(state): State<AppState>, auth: Auth, Path(name): Path<String>) -> Result<StatusCode> {
    auth.require_admin()?;
    if name == auth.0.name {
        return Err(Error::conflict("you can't remove yourself"));
    }
    let conn = state.db.lock().unwrap();
    let user = db::user_by_name(&conn, &name)?.ok_or_else(|| Error::not_found("user"))?;
    if db::instance_count_of(&conn, user.id)? > 0 {
        return Err(Error::conflict(format!("{name} still has vms; delete them first")));
    }
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    db::delete_user(&tx, user.id)?;
    db::record(&tx, NewEvent::new("removed user", name.clone()).by(&auth.0.name))?;
    tx.commit()?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- tokens --------------------------------------------------------------------

fn token_api(t: db::TokenRow) -> Token {
    Token { id: t.id, name: t.name, owner: t.owner, created_at: t.created_at, last_used: t.last_used }
}

/// Your own tokens; admins see everyone's.
async fn list_tokens(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Token>>> {
    let conn = state.db.lock().unwrap();
    let owner = (!auth.is_admin()).then_some(auth.0.id);
    Ok(Json(db::list_tokens(&conn, owner)?.into_iter().map(token_api).collect()))
}

async fn create_token(State(state): State<AppState>, auth: Auth, Json(req): Json<CreateToken>) -> Result<(StatusCode, Json<NewToken>)> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(Error::bad_request("give the token a name of 1-64 characters"));
    }
    let conn = state.db.lock().unwrap();
    let (id, secret) = auth::new_token(&conn, auth.0.id, name)?;
    db::record(&conn, NewEvent::new("token", format!("made {name}")).by(&auth.0.name))?;
    let token = Token { id, name: name.to_string(), owner: auth.0.name.clone(), created_at: db::now(), last_used: None };
    Ok((StatusCode::CREATED, Json(NewToken { token, secret })))
}

async fn revoke_token(State(state): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<StatusCode> {
    let conn = state.db.lock().unwrap();
    let owner = (!auth.is_admin()).then_some(auth.0.id);
    if !db::delete_token(&conn, &id, owner)? {
        return Err(Error::not_found("token"));
    }
    db::record(&conn, NewEvent::new("token", format!("revoked {id}")).by(&auth.0.name))?;
    Ok(StatusCode::NO_CONTENT)
}
