//! Drop-in replacements for axum's `Json`, `Path` and `Query` whose rejections use the
//! `{"error": …}` envelope ([`ApiError`]) instead of plain text.

use axum::extract::rejection::{BytesRejection, JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, OptionalFromRequest, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::ApiError;

/// A request rejected by axum before the handler ran: 413 stays 413, everything
/// the client can fix is a 400, the rest (missing route params) is a bug.
fn rejected(status: StatusCode, message: String) -> ApiError {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::PayloadTooLarge(message)
    } else if status.is_client_error() {
        ApiError::BadRequest(message)
    } else {
        ApiError::Internal(anyhow::anyhow!(message))
    }
}

impl From<JsonRejection> for ApiError {
    fn from(r: JsonRejection) -> Self {
        rejected(r.status(), r.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(r: PathRejection) -> Self {
        rejected(r.status(), r.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(r: QueryRejection) -> Self {
        rejected(r.status(), r.body_text())
    }
}

impl From<BytesRejection> for ApiError {
    fn from(r: BytesRejection) -> Self {
        rejected(r.status(), r.body_text())
    }
}

/// JSON request body / response.
#[derive(Debug, Default)]
pub struct Json<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Json<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let axum::Json(value) = <axum::Json<T> as FromRequest<S>>::from_request(req, state).await?;
        Ok(Json(value))
    }
}

/// Optional body: absent when the request has no `Content-Type`.
impl<T: DeserializeOwned, S: Send + Sync> OptionalFromRequest<S> for Json<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Option<Self>, ApiError> {
        let value = <axum::Json<T> as OptionalFromRequest<S>>::from_request(req, state).await?;
        Ok(value.map(|axum::Json(v)| Json(v)))
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// Path parameters.
#[derive(Debug)]
pub struct Path<T>(pub T);

impl<T: DeserializeOwned + Send, S: Send + Sync> FromRequestParts<S> for Path<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let axum::extract::Path(value) =
            axum::extract::Path::from_request_parts(parts, state).await?;
        Ok(Path(value))
    }
}

/// Query string parameters.
#[derive(Debug)]
pub struct Query<T>(pub T);

impl<T: DeserializeOwned + Send, S: Send + Sync> FromRequestParts<S> for Query<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let axum::extract::Query(value) =
            axum::extract::Query::from_request_parts(parts, state).await?;
        Ok(Query(value))
    }
}
