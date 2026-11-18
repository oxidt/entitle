//! entitle server: HTTP API, persistence, outgoing webhooks and adapter wiring.

pub mod adapters;
pub mod auth;
pub mod cli;
pub mod config;
pub mod crypto;
pub mod customers;
pub mod dashboard;
pub mod db;
pub mod error;
pub mod extract;
pub mod fx;
pub mod identity_token;
pub mod mcp;
pub mod metrics;
pub mod outbox;
pub mod purchases;
pub mod rate_limit;
pub mod routes;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{Method, header};
use axum::routing::get;
use sqlx::PgPool;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

pub use adapters::Adapters;
pub use crypto::Cipher;

/// Embedded migrations, run on startup.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub cipher: Arc<Cipher>,
    pub admin_token: Arc<str>,
    pub adapters: Adapters,
    /// See [`config::Config::webhook_allow_private`].
    pub webhook_allow_private: bool,
    /// See [`config::Config::trust_proxy`].
    pub trust_proxy: bool,
    /// See [`config::Config::public_url`]; MCP is off without it.
    pub public_url: Option<Arc<str>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .nest("/admin/v1", routes::admin::router())
        // The 405 fallback is set before the CORS layer so preflights reach it.
        .nest(
            "/v1",
            routes::sdk::router()
                .method_not_allowed_fallback(method_not_allowed)
                .layer(sdk_cors()),
        )
        .nest("/webhooks", routes::webhooks::router())
        .merge(dashboard::router(state.clone()))
        .merge(mcp::router(&state))
        .fallback(|| async { error::ApiError::not_found("no such route") })
        .method_not_allowed_fallback(method_not_allowed)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn method_not_allowed() -> error::ApiError {
    error::ApiError::MethodNotAllowed
}

/// Browsers and webviews call `/v1` directly with a public key in the `Authorization` header;
/// no cookies are involved, so any origin is allowed without credentials.
fn sdk_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::HeaderName::from_static(identity_token::HEADER),
        ])
        .max_age(Duration::from_secs(24 * 60 * 60))
}

/// Which build is running: the image build passes the source commit as `ENTITLE_GIT_SHA`.
async fn version() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "commit": option_env!("ENTITLE_GIT_SHA"),
    }))
}

async fn healthz(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<&'static str, error::ApiError> {
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok("ok")
}
