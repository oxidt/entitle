//! Signed identity for public-key callers.
//!
//! The public key ships in apps, so on its own it lets anyone act as any app user
//! id. A developer's backend can instead mint a short-lived HS256 JWT for its
//! signed-in user (`sub` = app user id, `aud` = `entitle:<project id>`, `exp`)
//! with the project's identity secret. Tokens are verified whenever sent and
//! required once the project sets `require_signed_identity`. Anonymous ids need
//! no proof: they are random and never returned to public keys.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use entitle_core::ProjectId;
use entitle_core::identity::is_anonymous;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::Deserialize;

use crate::AppState;
use crate::auth::{KeyKind, ProjectKey};
use crate::error::{ApiError, ApiResult};

/// Request header carrying the token for the app user id in the path.
pub const HEADER: &str = "x-entitle-identity";

pub fn secret_aad(project: ProjectId) -> Vec<u8> {
    format!("identity_secret:{}", project.0).into_bytes()
}

pub fn audience(project: ProjectId) -> String {
    format!("entitle:{}", project.0)
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
}

/// The token's `sub` if it is a valid, unexpired HS256 token for `project`.
pub fn verify(secret: &str, project: ProjectId, token: &str) -> Option<String> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_audience(&[audience(project)]);
    validation.set_required_spec_claims(&["exp", "aud", "sub"]);
    validation.validate_nbf = true;
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .ok()
    .map(|data| data.claims.sub)
}

/// The `X-Entitle-Identity` header, if sent.
pub struct IdentityToken(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for IdentityToken {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        parts
            .headers
            .get(HEADER)
            .map(|v| {
                v.to_str()
                    .map(str::to_owned)
                    .map_err(|_| ApiError::BadRequest(format!("invalid {HEADER} header")))
            })
            .transpose()
            .map(IdentityToken)
    }
}

/// Checks that the caller may act as `app_user_id`.
pub async fn authorize(
    state: &AppState,
    key: &ProjectKey,
    app_user_id: &str,
    token: Option<&str>,
) -> ApiResult<()> {
    if key.kind == KeyKind::Secret || is_anonymous(app_user_id) {
        return Ok(());
    }
    let (required, sealed): (bool, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT require_signed_identity, sealed_identity_secret FROM projects WHERE id = $1",
    )
    .bind(key.project_id.0)
    .fetch_one(&state.db)
    .await?;
    let Some(token) = token else {
        return if required {
            Err(ApiError::Forbidden(format!(
                "a signed identity token is required for `{app_user_id}`"
            )))
        } else {
            Ok(())
        };
    };
    let Some(sealed) = sealed else {
        return Err(ApiError::BadRequest(
            "identity token sent, but the project has no identity secret".into(),
        ));
    };
    let secret = state.cipher.open(&sealed, &secret_aad(key.project_id))?;
    let secret = String::from_utf8(secret).map_err(anyhow::Error::from)?;
    match verify(&secret, key.project_id, token) {
        Some(sub) if sub == app_user_id => Ok(()),
        _ => Err(ApiError::Forbidden(format!(
            "invalid identity token for `{app_user_id}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;
    use uuid::Uuid;

    fn token(secret: &str, alg: Algorithm, claims: serde_json::Value) -> String {
        encode(
            &Header::new(alg),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    }

    #[test]
    fn verifies_sub_audience_expiry_and_algorithm() {
        let project = ProjectId(Uuid::new_v4());
        let exp = chrono::Utc::now().timestamp() + 600;
        let ok = json!({ "sub": "alice", "aud": audience(project), "exp": exp });
        let t = token("s", Algorithm::HS256, ok.clone());
        assert_eq!(verify("s", project, &t).as_deref(), Some("alice"));

        assert_eq!(verify("other", project, &t), None);
        assert_eq!(verify("s", ProjectId(Uuid::new_v4()), &t), None);
        let expired = json!({ "sub": "alice", "aud": audience(project), "exp": exp - 3600 });
        assert_eq!(
            verify("s", project, &token("s", Algorithm::HS256, expired)),
            None
        );
        let no_exp = json!({ "sub": "alice", "aud": audience(project) });
        assert_eq!(
            verify("s", project, &token("s", Algorithm::HS256, no_exp)),
            None
        );
        assert_eq!(
            verify("s", project, &token("s", Algorithm::HS512, ok)),
            None
        );
        let not_yet = json!({ "sub": "alice", "aud": audience(project), "exp": exp + 86400, "nbf": exp + 3600 });
        assert_eq!(
            verify("s", project, &token("s", Algorithm::HS256, not_yet)),
            None
        );
    }
}
