//! The OAuth 2.1 authorization server in front of `/mcp` (rules from `oxidt-mcp-oauth`):
//!
//! - `GET  /.well-known/oauth-protected-resource[/mcp]` (RFC 9728)
//! - `GET  /.well-known/oauth-authorization-server` (RFC 8414)
//! - `POST /oauth/register`: dynamic client registration (RFC 7591)
//! - `GET  /oauth/authorize`: the consent page; `POST` approves with the admin token
//! - `POST /oauth/token`: authorization code + PKCE for an access token
//!
//! `/oauth/*` is rate limited per client IP.
//!
//! Consent is stateless: the form carries the authorize parameters, which are validated
//! again on submit. Clients ask for `mcp`; whether to add `mcp:write` is decided on the
//! consent page (preselected when the client asked for it). Only someone holding the admin token can approve, so there is nothing
//! for a cross-site request to forge. Access tokens do not expire and there are no
//! refresh tokens; rotating the admin token revokes them all.

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use chrono::{DateTime, Duration, Utc};
use oxidt_mcp_oauth::authorize::{self, AuthorizeError};
use oxidt_mcp_oauth::metadata::{self, Paths, ServerMetadata};
use oxidt_mcp_oauth::token::{self, Grant, StoredCode};
use oxidt_mcp_oauth::{Allowlist, OAuthError, Scopes, register};
use serde::Deserialize;
use serde_json::json;

use super::{READ_SCOPE, TOKEN_PREFIX, WRITE_SCOPE, token_hash};
use crate::AppState;
use crate::auth::is_admin_token;
use crate::crypto::random_token;
use crate::rate_limit::{self, RateLimit};

const CODE_TTL_SECONDS: i64 = 60;
/// Registration is unauthenticated, so what it stores is bounded: per client, and by
/// dropping clients that got no token within a day and capping the rest.
const MAX_REDIRECT_URIS: usize = 5;
const MAX_REDIRECT_URI_LEN: usize = 512;
const MAX_PENDING_CLIENTS: i64 = 1000;

/// Requests per minute and client IP to `/oauth/*`; a connection takes four.
const OAUTH_PER_MINUTE: u32 = 30;

pub fn router(trust_proxy: bool) -> Router<AppState> {
    let oauth = Router::new()
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(consent).post(decide))
        .route("/oauth/token", post(exchange))
        .layer(from_fn_with_state(
            RateLimit::per_minute(OAUTH_PER_MINUTE, trust_proxy),
            rate_limit::limit,
        ));
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .merge(oauth)
}

/// Callbacks a client may register: Claude, ChatGPT, the MCPI gateway, and loopback
/// (Claude Code and other local clients).
fn allowlist() -> Allowlist {
    Allowlist::claude()
        .with_chatgpt()
        .with_exact("https://mcpi.app/gateway/oauth/callback")
}

fn scopes() -> Scopes {
    Scopes {
        required: vec![READ_SCOPE.into()],
        optional: vec![WRITE_SCOPE.into()],
    }
}

fn public_url(state: &AppState) -> &str {
    state.public_url.as_deref().unwrap_or_default()
}

fn oauth_error(error: &OAuthError) -> Response {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::BAD_REQUEST);
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(error.json()),
    )
        .into_response()
}

fn server_error(e: sqlx::Error) -> Response {
    tracing::error!(error = %e, "mcp oauth: database error");
    oauth_error(&OAuthError::server_error(""))
}

// ---- discovery ----

async fn protected_resource(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(metadata::protected_resource(
        public_url(&state),
        "/mcp",
        &[READ_SCOPE.into(), WRITE_SCOPE.into()],
        Some("entitle"),
    ))
}

async fn authorization_server(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(metadata::authorization_server(
        public_url(&state),
        &ServerMetadata {
            scopes: vec![READ_SCOPE.into(), WRITE_SCOPE.into()],
            refresh_tokens: false,
            revocation: false,
            iss_parameter: false,
            paths: Paths::default(),
        },
    ))
}

// ---- registration ----

async fn register(State(state): State<AppState>, Json(req): Json<register::Request>) -> Response {
    let reg = match register::validate(req, &allowlist()) {
        Ok(reg) => reg,
        Err(e) => {
            tracing::warn!(error = %e, "mcp oauth: rejected client registration");
            return oauth_error(&e);
        }
    };
    if reg.redirect_uris.len() > MAX_REDIRECT_URIS
        || reg
            .redirect_uris
            .iter()
            .any(|u| u.len() > MAX_REDIRECT_URI_LEN)
    {
        return oauth_error(&OAuthError::invalid_redirect_uri(
            "too many or too long redirect_uris",
        ));
    }
    let pending: Result<(i64,), _> = sqlx::query_as(
        "WITH stale AS (
            DELETE FROM mcp_clients c
            WHERE c.created_at < now() - interval '1 day'
              AND NOT EXISTS (SELECT 1 FROM mcp_tokens t WHERE t.client_id = c.client_id)
         )
         SELECT count(*) FROM mcp_clients c
         WHERE c.created_at >= now() - interval '1 day'
           AND NOT EXISTS (SELECT 1 FROM mcp_tokens t WHERE t.client_id = c.client_id)",
    )
    .fetch_one(&state.db)
    .await;
    match pending {
        Ok((n,)) if n >= MAX_PENDING_CLIENTS => {
            return oauth_error(&OAuthError::temporarily_unavailable(
                "too many pending registrations, try again later",
            ));
        }
        Ok(_) => {}
        Err(e) => return server_error(e),
    }
    // Registrant-supplied and shown on the consent page.
    let name = reg
        .client_name
        .as_deref()
        .map(|n| n.chars().take(64).collect::<String>());
    let client_id = random_token("client_");
    let inserted = sqlx::query(
        "INSERT INTO mcp_clients (client_id, client_name, redirect_uris) VALUES ($1, $2, $3)",
    )
    .bind(&client_id)
    .bind(name)
    .bind(&reg.redirect_uris)
    .execute(&state.db)
    .await;
    if let Err(e) = inserted {
        return server_error(e);
    }
    (
        StatusCode::CREATED,
        Json(register::response(&client_id, &reg, &scopes(), false)),
    )
        .into_response()
}

// ---- consent ----

/// The client, and the request validated against its registration.
#[allow(clippy::result_large_err)] // the error is the response to send
async fn validate(
    state: &AppState,
    req: &authorize::Request,
) -> Result<(Option<String>, authorize::Validated), Response> {
    let client: Option<(Option<String>, Vec<String>)> =
        sqlx::query_as("SELECT client_name, redirect_uris FROM mcp_clients WHERE client_id = $1")
            .bind(&req.client_id)
            .fetch_optional(&state.db)
            .await
            .map_err(server_error)?;
    let Some((name, redirect_uris)) = client else {
        return Err(error_page(
            "Unknown client. Start the connection again from your client.",
        ));
    };
    let resource = format!("{}/mcp", public_url(state));
    match authorize::validate(
        req,
        &redirect_uris,
        &allowlist(),
        &scopes(),
        Some(&resource),
    ) {
        Ok(validated) => Ok((name, validated)),
        // Not a registered callback: never redirect there.
        Err(AuthorizeError::InvalidRedirectUri) => Err(error_page(
            "The redirect URL is not registered for this client.",
        )),
        Err(AuthorizeError::Redirect(e)) => Err(redirect(
            &req.redirect_uri,
            &[("error", e.error)],
            req.state.as_deref(),
        )),
    }
}

async fn consent(State(state): State<AppState>, Query(req): Query<authorize::Request>) -> Response {
    match validate(&state, &req).await {
        Ok((name, _)) => {
            let asked_write = req
                .scope
                .as_deref()
                .is_some_and(|s| s.split(' ').any(|s| s == WRITE_SCOPE));
            consent_page(&req, name.as_deref(), asked_write, None)
        }
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct Decision {
    #[serde(flatten)]
    request: authorize::Request,
    #[serde(default)]
    admin_token: String,
    /// The "allow changes" checkbox; absent when unchecked.
    write: Option<String>,
    decision: String,
}

async fn decide(State(state): State<AppState>, Form(form): Form<Decision>) -> Response {
    let (name, pending) = match validate(&state, &form.request).await {
        Ok(v) => v,
        Err(response) => return response,
    };
    if form.decision != "approve" {
        return redirect(
            &pending.redirect_uri,
            &[("error", "access_denied")],
            pending.state.as_deref(),
        );
    }
    if !is_admin_token(&state, form.admin_token.trim()) {
        // Keep the checkbox as submitted, not as the client asked.
        return consent_page(
            &form.request,
            name.as_deref(),
            form.write.is_some(),
            Some("That token was rejected."),
        );
    }
    let scope = if form.write.is_some() {
        format!("{READ_SCOPE} {WRITE_SCOPE}")
    } else {
        READ_SCOPE.to_owned()
    };
    let code = random_token("");
    let stored = sqlx::query(
        "INSERT INTO mcp_codes
            (code_hash, client_id, redirect_uri, code_challenge, scope, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(token_hash(&state, &code))
    .bind(&pending.client_id)
    .bind(&pending.redirect_uri)
    .bind(&pending.code_challenge)
    .bind(&scope)
    .bind(Utc::now() + Duration::seconds(CODE_TTL_SECONDS))
    .execute(&state.db)
    .await;
    if let Err(e) = stored {
        return server_error(e);
    }
    redirect(
        &pending.redirect_uri,
        &[("code", &code)],
        pending.state.as_deref(),
    )
}

fn redirect(redirect_uri: &str, params: &[(&str, &str)], state: Option<&str>) -> Response {
    let mut params = params.to_vec();
    if let Some(state) = state {
        params.push(("state", state));
    }
    match authorize::redirect_url(redirect_uri, &params) {
        Ok(url) => Redirect::to(url.as_str()).into_response(),
        Err(_) => error_page("Invalid redirect URL."),
    }
}

// ---- token ----

async fn exchange(State(state): State<AppState>, Form(form): Form<token::Request>) -> Response {
    let grant = match token::parse(form) {
        Ok(Grant::AuthorizationCode(grant)) => grant,
        Ok(Grant::RefreshToken(_)) => {
            return oauth_error(&OAuthError::unsupported_grant_type(
                "refresh tokens are not issued",
            ));
        }
        Err(e) => return oauth_error(&e),
    };
    // Single use: deleting it is the claim.
    let code: Option<(String, String, String, String, DateTime<Utc>)> = match sqlx::query_as(
        "DELETE FROM mcp_codes WHERE code_hash = $1
         RETURNING client_id, redirect_uri, code_challenge, scope, expires_at",
    )
    .bind(token_hash(&state, &grant.code))
    .fetch_optional(&state.db)
    .await
    {
        Ok(code) => code,
        Err(e) => return server_error(e),
    };
    let Some((client_id, redirect_uri, code_challenge, scope, expires_at)) = code else {
        return oauth_error(&OAuthError::invalid_grant("unknown or used code"));
    };
    let stored = StoredCode {
        client_id,
        redirect_uri,
        code_challenge,
        resource: Some(format!("{}/mcp", public_url(&state))),
        expires_at: expires_at.timestamp(),
    };
    if let Err(e) = token::verify_code_exchange(&grant, &stored, Utc::now().timestamp()) {
        return oauth_error(&e);
    }
    let access_token = random_token(TOKEN_PREFIX);
    let inserted =
        sqlx::query("INSERT INTO mcp_tokens (token_hash, client_id, scope) VALUES ($1, $2, $3)")
            .bind(token_hash(&state, &access_token))
            .bind(&stored.client_id)
            .bind(&scope)
            .execute(&state.db)
            .await;
    if let Err(e) = inserted {
        return server_error(e);
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "access_token": access_token,
            "token_type": "Bearer",
            "scope": scope,
        })),
    )
        .into_response()
}

// ---- pages ----

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · entitle</title>
<style>
  :root {{ color-scheme: light dark; --line: #8884; --accent: #4f46e5; }}
  body {{ font: 15px/1.5 system-ui, sans-serif; max-width: 26rem; margin: 4rem auto; padding: 0 1rem; }}
  .card {{ border: 1px solid var(--line); border-radius: 12px; padding: 1.5rem; }}
  h1 {{ font-size: 1.2rem; margin: 0 0 .5rem; }}
  label {{ display: block; margin: 1rem 0 .3rem; font-size: .9rem; }}
  input {{ box-sizing: border-box; width: 100%; padding: .55rem .7rem; border: 1px solid var(--line); border-radius: 8px; font: inherit; background: transparent; color: inherit; }}
  .row {{ display: flex; gap: .75rem; margin-top: 1.25rem; }}
  button {{ flex: 1; padding: .6rem; border-radius: 8px; border: 1px solid var(--line); font: inherit; cursor: pointer; background: transparent; color: inherit; }}
  button.primary {{ background: var(--accent); border-color: var(--accent); color: #fff; }}
  .muted {{ opacity: .7; font-size: .9rem; }}
  label.check {{ display: flex; gap: .5rem; align-items: baseline; }}
  label.check input {{ width: auto; }}
  .error {{ color: #dc2626; }}
</style>
</head>
<body><div class="card">{body}</div></body>
</html>"#,
        title = escape(title),
    );
    (
        status,
        [
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Html(html),
    )
        .into_response()
}

fn error_page(message: &str) -> Response {
    page(
        StatusCode::BAD_REQUEST,
        "Authorization error",
        &format!("<h1>Authorization error</h1><p>{}</p>", escape(message)),
    )
}

fn consent_page(
    req: &authorize::Request,
    name: Option<&str>,
    write: bool,
    error: Option<&str>,
) -> Response {
    let hidden = [
        ("response_type", Some(&req.response_type)),
        ("client_id", Some(&req.client_id)),
        ("redirect_uri", Some(&req.redirect_uri)),
        ("code_challenge", Some(&req.code_challenge)),
        ("code_challenge_method", Some(&req.code_challenge_method)),
        ("scope", req.scope.as_ref()),
        ("state", req.state.as_ref()),
        ("resource", req.resource.as_ref()),
    ]
    .into_iter()
    .filter_map(|(k, v)| {
        v.map(|v| format!(r#"<input type="hidden" name="{k}" value="{}">"#, escape(v)))
    })
    .collect::<String>();
    let client = escape(name.unwrap_or("An MCP client"));
    let checked = if write { " checked" } else { "" };
    let error = error
        .map(|e| format!(r#"<p class="error" role="alert">{}</p>"#, escape(e)))
        .unwrap_or_default();
    let body = format!(
        r#"<h1>Connect {client} to entitle</h1>
<p class="muted">It will be able to read every project's customers, entitlements, products,
purchase activity and revenue metrics.</p>
<form method="post" action="/oauth/authorize">
  {hidden}
  <label for="admin_token">Admin token</label>
  <input id="admin_token" name="admin_token" type="password" autocomplete="current-password"
    placeholder="ENTITLE_ADMIN_TOKEN" autofocus>
  <label class="check"><input type="checkbox" name="write" value="1"{checked}>
    Allow changes: grant and revoke customers' entitlements</label>
  {error}
  <div class="row">
    <button type="submit" name="decision" value="deny" formnovalidate>Deny</button>
    <button type="submit" name="decision" value="approve" class="primary">Allow</button>
  </div>
</form>"#
    );
    page(StatusCode::OK, "Connect", &body)
}
