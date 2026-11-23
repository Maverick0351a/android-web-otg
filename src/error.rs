//! Typed application errors mapped cleanly to HTTP JSON responses.

use std::fmt;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Application-wide error type for USB, ADB, authentication, and HTTP handlers.
#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    Unauthorized(String),
    ServiceUnavailable(String),
    Usb(String),
    Io(std::io::Error),
    Internal(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            Self::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            Self::ServiceUnavailable(msg) => write!(f, "Service unavailable: {msg}"),
            Self::Usb(msg) => write!(f, "USB error: {msg}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Internal(msg) => write!(f, "Internal error: {msg}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(msg) => {
                (StatusCode::BAD_REQUEST, Json(json!({ "ok": false, "error": msg })))
                    .into_response()
            }
            Self::Unauthorized(msg) => (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "ok": false, "auth_required": true, "error": msg })),
            )
                .into_response(),
            Self::ServiceUnavailable(msg) => {
                (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "ok": false, "error": msg })))
                    .into_response()
            }
            Self::Usb(msg) => {
                (StatusCode::OK, Json(json!({ "ok": false, "error": msg }))).into_response()
            }
            Self::Io(err) => {
                (StatusCode::BAD_GATEWAY, Json(json!({ "ok": false, "error": err.to_string() })))
                    .into_response()
            }
            Self::Internal(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "ok": false, "error": msg })))
                    .into_response()
            }
        }
    }
}

/// Shorthand Result type using [`AppError`].
pub type Result<T> = std::result::Result<T, AppError>;
