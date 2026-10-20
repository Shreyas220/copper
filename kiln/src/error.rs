use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use copper_api::ApiError;

#[derive(Debug)]
pub struct Error(pub StatusCode, pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn not_found(what: &str) -> Self {
        Self(StatusCode::NOT_FOUND, format!("{what} not found"))
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, msg.into())
    }
    pub fn forbidden() -> Self {
        Self(StatusCode::FORBIDDEN, "forbidden".into())
    }
    pub fn unauthorized() -> Self {
        Self(StatusCode::UNAUTHORIZED, "missing or invalid token".into())
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { message: self.1 })).into_response()
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %e, "internal error");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}
