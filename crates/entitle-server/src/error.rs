use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use entitle_core::AdapterError;
use entitle_core::identity::IdentityError;
use serde_json::json;

/// Error returned by every handler. Body: `{"error": {"code": "...", "message": "..."}}`.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("missing or invalid credentials")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("{0}")]
    PayloadTooLarge(String),
    /// Over a request budget; answered with `Retry-After`.
    #[error("too many requests, retry in {retry_after} s")]
    TooManyRequests { retry_after: u64 },
    #[error(transparent)]
    TransferRefused(#[from] crate::purchases::TransferRefused),
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn not_found(what: impl Into<String>) -> Self {
        ApiError::NotFound(what.into())
    }

    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ApiError::MethodNotAllowed => (StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed"),
            ApiError::PayloadTooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
            ApiError::TooManyRequests { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            ApiError::TransferRefused(_) => (StatusCode::CONFLICT, "transfer_refused"),
            ApiError::Adapter(e) => match e {
                AdapterError::InvalidSignature(_) => {
                    (StatusCode::UNAUTHORIZED, "invalid_signature")
                }
                AdapterError::Malformed(_) => (StatusCode::BAD_REQUEST, "malformed_payload"),
                AdapterError::NotFound => (StatusCode::NOT_FOUND, "store_not_found"),
                AdapterError::Credentials(_) => {
                    (StatusCode::UNPROCESSABLE_ENTITY, "store_credentials")
                }
                AdapterError::Upstream(_) => (StatusCode::BAD_GATEWAY, "store_unavailable"),
                AdapterError::NotImplemented => (StatusCode::NOT_IMPLEMENTED, "not_implemented"),
            },
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

impl From<IdentityError> for ApiError {
    fn from(e: IdentityError) -> Self {
        ApiError::BadRequest(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let message = match &self {
            ApiError::Internal(e) => {
                tracing::error!(error = ?e, "internal error");
                "internal server error".to_string()
            }
            ApiError::Adapter(e) => {
                tracing::warn!(error = %e, "store adapter error");
                e.to_string()
            }
            other => other.to_string(),
        };
        let mut response = (
            status,
            Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response();
        if let ApiError::TooManyRequests { retry_after } = self {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, retry_after.into());
        }
        response
    }
}

/// True for a Postgres unique-constraint violation.
pub fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}
