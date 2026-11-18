//! The setup pages' writes go through the admin API in-process: the same
//! routes, validation and errors as `entitle admin` and HTTP clients, with the
//! admin token added here on the server.

use axum::body::Body;
use axum::http::{Method, Request, header};
use http_body_util::BodyExt;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tower::ServiceExt;

use crate::AppState;

/// The API's error message, or a transport failure.
#[derive(Debug)]
pub struct ApiFailure(pub String);

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ApiFailure {}

/// Calls `/admin/v1{path}`; an empty response deserializes from `null`.
pub async fn call<T: DeserializeOwned>(
    state: &AppState,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<T, ApiFailure> {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/admin/v1{path}"))
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", state.admin_token),
        );
    let req = match body {
        Some(body) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => {
            req = req.header(header::CONTENT_LENGTH, 0);
            req.body(Body::empty())
        }
    }
    .map_err(|e| ApiFailure(e.to_string()))?;
    let router = axum::Router::new()
        .nest("/admin/v1", crate::routes::admin::router())
        .with_state(state.clone());
    // The router's error type is `Infallible`.
    let Ok(response) = router.oneshot(req).await;
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| ApiFailure(e.to_string()))?
        .to_bytes();
    let json: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).map_err(|e| ApiFailure(e.to_string()))?
    };
    if !status.is_success() {
        let message = json["error"]["message"]
            .as_str()
            .map_or_else(|| format!("HTTP {status}"), str::to_owned);
        return Err(ApiFailure(message));
    }
    serde_json::from_value(json).map_err(|e| ApiFailure(e.to_string()))
}
