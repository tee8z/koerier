use axum::Json;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// A public LNURL error. Static reasons keep backend details private.
#[derive(Debug, Serialize)]
pub(crate) struct LnurlError {
    status: &'static str,
    reason: &'static str,
}

impl LnurlError {
    pub(crate) fn new(reason: &'static str) -> Self {
        Self {
            status: "ERROR",
            reason,
        }
    }
}

impl IntoResponse for LnurlError {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}
