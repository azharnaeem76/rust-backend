//! Pieces shared by the items and stats modules.

use std::fmt::Display;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use axum::{
    extract::rejection::{JsonRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// An API error, rendered as `{ "error": "<message>" }` with a matching status code.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    MethodNotAllowed,
    Internal(String),
}

impl ApiError {
    pub fn internal(context: &str, err: impl Display) -> Self {
        Self::Internal(format!("{context}: {err}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, message),
            Self::MethodNotAllowed => (
                StatusCode::METHOD_NOT_ALLOWED,
                "method not allowed".to_owned(),
            ),
            Self::Internal(detail) => {
                // Log the detail, but don't leak file paths or parser errors to clients.
                eprintln!("internal error: {detail}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_owned(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        Self::BadRequest(rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::BadRequest(rejection.body_text())
    }
}

/// Last-modified time of the data file, used as the cache key.
pub fn modified_time(path: &Path) -> Result<SystemTime, ApiError> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(|e| ApiError::internal("stat data file", e))
}

#[cfg(test)]
pub mod test_support {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Writes `contents` to `items.json` in a fresh temp directory and returns its path.
    pub fn temp_data_file(contents: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rust-assessment-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("items.json");
        std::fs::write(&path, contents).expect("write temp data file");
        path
    }
}
