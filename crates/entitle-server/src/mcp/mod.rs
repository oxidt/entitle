//! MCP for AI clients (Claude, ChatGPT, MCP gateways): tools over the admin API at `/mcp`,
//! behind a minimal OAuth 2.1 authorization server whose consent step asks for the admin
//! token. Tools read; with the `mcp:write` scope they also grant and revoke entitlements.
//! Mounted only when `ENTITLE_PUBLIC_URL` is set.
//!
//! `/mcp` accepts an MCP access token (`mcp_…`) or the admin token itself, for clients
//! configured with a static `Authorization` header (which can write).

mod oauth;
mod tools;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::AppState;
use crate::auth::is_admin_token;

/// Every token has it: the read tools.
const READ_SCOPE: &str = "mcp";
/// Optional, chosen on the consent page: the write tools.
const WRITE_SCOPE: &str = "mcp:write";
/// Prefix of issued access tokens.
const TOKEN_PREFIX: &str = "mcp_";

pub fn router(state: &AppState) -> Router<AppState> {
    let Some(public_url) = state.public_url.clone() else {
        return Router::new();
    };
    let mcp = tools::router(state.clone(), &public_url)
        .layer(from_fn_with_state(state.clone(), require_token));
    oauth::router(state.trust_proxy).merge(mcp)
}

/// Keyed with the admin token, so rotating it revokes every MCP token and pending code.
fn token_hash(state: &AppState, token: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(state.admin_token.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(b"entitle mcp token v1\0");
    mac.update(token.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// What the request's token may do; in the request extensions for the tools.
#[derive(Clone, Copy)]
struct Access {
    write: bool,
}

/// Without a valid token, `401` with the challenge that starts OAuth discovery.
async fn require_token(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let access = match token {
        Some(t) if is_admin_token(&state, t) => Some(Access { write: true }),
        Some(t) if t.starts_with(TOKEN_PREFIX) => {
            let found: sqlx::Result<Option<(String,)>> =
                sqlx::query_as("SELECT scope FROM mcp_tokens WHERE token_hash = $1")
                    .bind(token_hash(&state, t))
                    .fetch_optional(&state.db)
                    .await;
            match found {
                Ok(row) => row.map(|(scope,)| Access {
                    write: scope.split(' ').any(|s| s == WRITE_SCOPE),
                }),
                Err(e) => return crate::error::ApiError::from(e).into_response(),
            }
        }
        _ => None,
    };
    if let Some(access) = access {
        request.extensions_mut().insert(access);
        return next.run(request).await;
    }
    let public_url = state.public_url.as_deref().unwrap_or_default();
    let challenge = oxidt_mcp_oauth::challenge::www_authenticate(
        &format!("{public_url}/.well-known/oauth-protected-resource/mcp"),
        Some(READ_SCOPE),
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
    )
        .into_response()
}
