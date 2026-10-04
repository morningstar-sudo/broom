// api.rs — what the JSON handlers share: the error type and its helpers.
use axum::{http::StatusCode, Json};

pub type ApiError = (StatusCode, String);

/// 500 with a generic message; the real error (OS paths, SQL…) only goes to the server log.
pub fn ise(e: impl ToString) -> ApiError {
    tracing::error!("internal error: {}", e.to_string());
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
}

/// 400 with the message as given (meant for the admin).
pub fn bad(e: impl ToString) -> ApiError {
    (StatusCode::BAD_REQUEST, e.to_string())
}

pub fn ok() -> Json<serde_json::Value> {
    Json(serde_json::json!({"ok": true}))
}
