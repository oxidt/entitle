//! Request authentication: admin token and project API keys.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use entitle_core::ProjectId;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::AppState;
use crate::crypto::hash_key;
use crate::error::ApiError;

fn bearer(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

/// Requires `Authorization: Bearer <ENTITLE_ADMIN_TOKEN>`.
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = bearer(parts).ok_or(ApiError::Unauthorized)?;
        if is_admin_token(state, token) {
            Ok(Admin)
        } else {
            Err(ApiError::Unauthorized)
        }
    }
}

/// Constant-time comparison with `ENTITLE_ADMIN_TOKEN`.
pub fn is_admin_token(state: &AppState, token: &str) -> bool {
    bool::from(token.as_bytes().ct_eq(state.admin_token.as_bytes()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// `pk_…`: safe to ship in apps and web frontends.
    Public,
    /// `sk_…`: developer backends only.
    Secret,
}

impl KeyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyKind::Public => "public",
            KeyKind::Secret => "secret",
        }
    }

    pub fn prefix(self) -> &'static str {
        match self {
            KeyKind::Public => "pk_",
            KeyKind::Secret => "sk_",
        }
    }
}

/// Any valid, unrevoked project API key (public or secret).
#[derive(Debug, Clone, Copy)]
pub struct ProjectKey {
    pub project_id: ProjectId,
    pub kind: KeyKind,
}

impl FromRequestParts<AppState> for ProjectKey {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let key = bearer(parts).ok_or(ApiError::Unauthorized)?;
        let kind = if key.starts_with("pk_") {
            KeyKind::Public
        } else if key.starts_with("sk_") {
            KeyKind::Secret
        } else {
            return Err(ApiError::Unauthorized);
        };
        let row: Option<(Uuid, String)> = sqlx::query_as(
            "SELECT project_id, kind FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL",
        )
        .bind(hash_key(key))
        .fetch_optional(&state.db)
        .await?;
        match row {
            Some((project_id, stored_kind)) if stored_kind == kind.as_str() => Ok(ProjectKey {
                project_id: ProjectId(project_id),
                kind,
            }),
            _ => Err(ApiError::Unauthorized),
        }
    }
}

/// A secret (`sk_…`) project key.
#[derive(Debug, Clone, Copy)]
pub struct SecretKey {
    pub project_id: ProjectId,
}

impl FromRequestParts<AppState> for SecretKey {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let key = ProjectKey::from_request_parts(parts, state).await?;
        match key.kind {
            KeyKind::Secret => Ok(SecretKey {
                project_id: key.project_id,
            }),
            KeyKind::Public => Err(ApiError::Forbidden(
                "this endpoint requires a secret (sk_) key".into(),
            )),
        }
    }
}
