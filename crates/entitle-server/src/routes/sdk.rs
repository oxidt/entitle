//! `/v1`: the SDK API (public or secret key) and secret-only customer operations.

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use entitle_core::{
    CheckoutRequest, CheckoutSession, CustomerId, GoogleCredentials, ProjectId, Store,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::adapters::load_credentials;
use crate::auth::{ProjectKey, SecretKey};
use crate::customers::{self, CustomerInfo};
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, Path};
use crate::identity_token::{self, IdentityToken};
use crate::purchases::{Source, TransferRefused, apply_snapshot};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/customers/{app_user_id}",
            get(get_customer).delete(delete_customer),
        )
        .route("/customers/{app_user_id}/login", post(log_in))
        .route("/customers/{app_user_id}/sync", post(sync))
        .route("/customers/{app_user_id}/checkout", post(checkout))
        .route("/customers/{app_user_id}/portal", post(portal))
        .route("/customers/{app_user_id}/export", get(export))
        .route(
            "/customers/{app_user_id}/entitlements/{identifier}/grant",
            post(grant),
        )
        .route(
            "/customers/{app_user_id}/entitlements/{identifier}/revoke",
            post(revoke),
        )
}

pub(crate) async fn info(
    state: &AppState,
    project: ProjectId,
    customer: CustomerId,
    app_user_id: &str,
) -> ApiResult<Json<CustomerInfo>> {
    // Play's management link names the app package, which lives in the credentials.
    let play_package = match load_credentials::<GoogleCredentials>(
        &state.db,
        &state.cipher,
        project,
        Store::PlayStore,
    )
    .await
    {
        Ok(c) => Some(c.package_name),
        Err(ApiError::NotFound(_)) => None,
        Err(e) => return Err(e),
    };
    let mut conn = state.db.acquire().await?;
    Ok(Json(
        customers::info(
            &mut conn,
            project,
            customer,
            app_user_id,
            play_package.as_deref(),
            Utc::now(),
        )
        .await?,
    ))
}

pub(crate) async fn existing(
    state: &AppState,
    project: ProjectId,
    app_user_id: &str,
) -> ApiResult<CustomerId> {
    let mut conn = state.db.acquire().await?;
    customers::find(&mut conn, project, app_user_id)
        .await?
        .ok_or_else(|| ApiError::not_found("customer not found"))
}

async fn get_customer(
    key: ProjectKey,
    IdentityToken(token): IdentityToken,
    State(state): State<AppState>,
    Path(app_user_id): Path<String>,
) -> ApiResult<Json<CustomerInfo>> {
    identity_token::authorize(&state, &key, &app_user_id, token.as_deref()).await?;
    let customer = customers::get_or_create(&state.db, key.project_id, &app_user_id).await?;
    let Json(info) = info(&state, key.project_id, customer, &app_user_id).await?;
    Ok(Json(info.for_key(key.kind)))
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "LogInRequest"))]
struct LogIn {
    new_app_user_id: String,
    /// Signed identity token for `new_app_user_id` (see `X-Entitle-Identity`).
    #[cfg_attr(test, ts(optional))]
    identity_token: Option<String>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
struct LogInResponse {
    /// `true` if `new_app_user_id` had never been seen before.
    created: bool,
    customer_info: CustomerInfo,
}

async fn log_in(
    key: ProjectKey,
    IdentityToken(token): IdentityToken,
    State(state): State<AppState>,
    Path(app_user_id): Path<String>,
    Json(body): Json<LogIn>,
) -> ApiResult<Json<LogInResponse>> {
    // Both sides of a merge must be the caller's.
    identity_token::authorize(&state, &key, &app_user_id, token.as_deref()).await?;
    identity_token::authorize(
        &state,
        &key,
        &body.new_app_user_id,
        body.identity_token.as_deref(),
    )
    .await?;
    let (customer, created) = customers::log_in(
        &state.db,
        key.project_id,
        &app_user_id,
        &body.new_app_user_id,
    )
    .await?;
    let Json(customer_info) = info(&state, key.project_id, customer, &body.new_app_user_id).await?;
    Ok(Json(LogInResponse {
        created,
        customer_info: customer_info.for_key(key.kind),
    }))
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "SyncRequest"))]
struct Sync {
    store: Store,
    /// Apple transaction id, Google purchase token, Creem subscription or checkout id.
    id: String,
}

async fn sync(
    key: ProjectKey,
    IdentityToken(token): IdentityToken,
    State(state): State<AppState>,
    Path(app_user_id): Path<String>,
    Json(body): Json<Sync>,
) -> ApiResult<Json<CustomerInfo>> {
    identity_token::authorize(&state, &key, &app_user_id, token.as_deref()).await?;
    let project = key.project_id;
    let customer = customers::get_or_create(&state.db, project, &app_user_id).await?;
    let snapshots = state
        .adapters
        .fetch_purchases(&state.db, &state.cipher, project, body.store, &body.id)
        .await?;
    let now = Utc::now();
    let mut tx = state.db.begin().await?;
    for s in &snapshots {
        apply_snapshot(&mut tx, project, s, Source::Restore(customer), now)
            .await
            .map_err(|e| match e.downcast::<TransferRefused>() {
                Ok(refused) => refused.into(),
                Err(e) => ApiError::Internal(e),
            })?;
    }
    tx.commit().await?;
    let Json(info) = info(&state, project, customer, &app_user_id).await?;
    Ok(Json(info.for_key(key.kind)))
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "CheckoutRequest"))]
struct Checkout {
    /// Creem product id (`prod_…`), configured as a `creem` product in the project.
    product_id: String,
    #[cfg_attr(test, ts(optional))]
    success_url: Option<String>,
    #[serde(default)]
    #[cfg_attr(test, ts(optional, as = "Option<_>"))]
    metadata: BTreeMap<String, String>,
}

async fn checkout(
    key: ProjectKey,
    IdentityToken(token): IdentityToken,
    State(state): State<AppState>,
    Path(app_user_id): Path<String>,
    Json(body): Json<Checkout>,
) -> ApiResult<Json<CheckoutSession>> {
    identity_token::authorize(&state, &key, &app_user_id, token.as_deref()).await?;
    let project = key.project_id;
    let known: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM products WHERE project_id = $1 AND store = 'creem' AND store_product_id = $2",
    )
    .bind(project.0)
    .bind(&body.product_id)
    .fetch_optional(&state.db)
    .await?;
    if known.is_none() {
        return Err(ApiError::not_found("unknown creem product"));
    }
    if let Some(success_url) = &body.success_url {
        check_redirect_origin(&state, project, success_url).await?;
    }
    let customer = customers::get_or_create(&state.db, project, &app_user_id).await?;
    let request = CheckoutRequest {
        product_id: body.product_id,
        customer,
        success_url: body.success_url,
        metadata: body.metadata,
    };
    Ok(Json(
        state
            .adapters
            .create_checkout(&state.db, &state.cipher, project, &request)
            .await?,
    ))
}

/// `success_url` must point to one of the project's `checkout_redirect_origins`.
async fn check_redirect_origin(
    state: &AppState,
    project: ProjectId,
    success_url: &str,
) -> ApiResult<()> {
    let url = reqwest::Url::parse(success_url)
        .map_err(|e| ApiError::BadRequest(format!("invalid success_url: {e}")))?;
    let (allowed,): (Vec<String>,) =
        sqlx::query_as("SELECT checkout_redirect_origins FROM projects WHERE id = $1")
            .bind(project.0)
            .fetch_one(&state.db)
            .await?;
    if allowed.contains(&url.origin().ascii_serialization()) {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "success_url origin is not in the project's checkout_redirect_origins".into(),
        ))
    }
}

#[derive(Serialize)]
struct PortalResponse {
    url: String,
}

/// Secret key only: the portal link gives access to the customer's billing details.
async fn portal(
    key: SecretKey,
    State(state): State<AppState>,
    Path(app_user_id): Path<String>,
) -> ApiResult<Json<PortalResponse>> {
    let project = key.project_id;
    let customer = existing(&state, project, &app_user_id).await?;
    let store_customer: Option<(String,)> = sqlx::query_as(
        "SELECT store_customer_id FROM purchases
         WHERE customer_id = $1 AND store = 'creem' AND store_customer_id IS NOT NULL
         ORDER BY purchased_at DESC LIMIT 1",
    )
    .bind(customer.0)
    .fetch_optional(&state.db)
    .await?;
    let Some((store_customer_id,)) = store_customer else {
        return Err(ApiError::not_found("customer has no creem purchase"));
    };
    let url = state
        .adapters
        .portal_url(&state.db, &state.cipher, project, &store_customer_id)
        .await?;
    Ok(Json(PortalResponse { url }))
}

// ---- secret key only ----

#[derive(Deserialize, Default)]
pub(crate) struct Grant {
    /// `null` / omitted = lifetime.
    pub expires_at: Option<DateTime<Utc>>,
}

async fn entitlement_id(state: &AppState, project: ProjectId, identifier: &str) -> ApiResult<Uuid> {
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM entitlements WHERE project_id = $1 AND identifier = $2")
            .bind(project.0)
            .bind(identifier)
            .fetch_optional(&state.db)
            .await?;
    row.map(|(id,)| id)
        .ok_or_else(|| ApiError::not_found("entitlement not found"))
}

async fn grant(
    key: SecretKey,
    State(state): State<AppState>,
    Path((app_user_id, identifier)): Path<(String, String)>,
    body: Option<Json<Grant>>,
) -> ApiResult<Json<CustomerInfo>> {
    let Json(body) = body.unwrap_or_default();
    grant_entitlement(&state, key.project_id, &app_user_id, &identifier, body).await
}

/// Grants `identifier` to the customer, creating the customer on first sight.
pub(crate) async fn grant_entitlement(
    state: &AppState,
    project: ProjectId,
    app_user_id: &str,
    identifier: &str,
    body: Grant,
) -> ApiResult<Json<CustomerInfo>> {
    if body.expires_at.is_some_and(|e| e <= Utc::now()) {
        return Err(ApiError::BadRequest(
            "expires_at must be in the future".into(),
        ));
    }
    let entitlement = entitlement_id(state, project, identifier).await?;
    let customer = customers::get_or_create(&state.db, project, app_user_id).await?;
    sqlx::query(
        "INSERT INTO promotional_grants (id, customer_id, entitlement_id, expires_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(customer.0)
    .bind(entitlement)
    .bind(body.expires_at)
    .execute(&state.db)
    .await?;
    info(state, project, customer, app_user_id).await
}

async fn revoke(
    key: SecretKey,
    State(state): State<AppState>,
    Path((app_user_id, identifier)): Path<(String, String)>,
) -> ApiResult<Json<CustomerInfo>> {
    revoke_entitlement(&state, key.project_id, &app_user_id, &identifier).await
}

/// Revokes the customer's active promotional grants of `identifier`.
pub(crate) async fn revoke_entitlement(
    state: &AppState,
    project: ProjectId,
    app_user_id: &str,
    identifier: &str,
) -> ApiResult<Json<CustomerInfo>> {
    let entitlement = entitlement_id(state, project, identifier).await?;
    let customer = existing(state, project, app_user_id).await?;
    sqlx::query(
        "UPDATE promotional_grants SET revoked_at = now()
         WHERE customer_id = $1 AND entitlement_id = $2 AND revoked_at IS NULL",
    )
    .bind(customer.0)
    .bind(entitlement)
    .execute(&state.db)
    .await?;
    info(state, project, customer, app_user_id).await
}

async fn export(
    key: SecretKey,
    State(state): State<AppState>,
    Path(app_user_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let customer = existing(&state, key.project_id, &app_user_id).await?;
    let mut conn = state.db.acquire().await?;
    Ok(Json(
        customers::export(&mut conn, key.project_id, customer, Utc::now()).await?,
    ))
}

async fn delete_customer(
    key: SecretKey,
    State(state): State<AppState>,
    Path(app_user_id): Path<String>,
) -> ApiResult<StatusCode> {
    let customer = existing(&state, key.project_id, &app_user_id).await?;
    customers::delete(&state.db, customer).await?;
    tracing::info!(project = %key.project_id, %customer, "customer erased (GDPR)");
    Ok(StatusCode::NO_CONTENT)
}

/// TypeScript types for the JS SDK (`sdk/js/src/generated`). Regenerate with
/// `cargo test -p entitle-server --lib export_bindings`.
#[cfg(test)]
mod bindings {
    use ts_rs::{Config, TS};

    use super::*;

    #[test]
    fn export_bindings() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../sdk/js/src/generated");
        // Start from scratch so types removed on the Rust side disappear too.
        let _ = std::fs::remove_dir_all(dir);
        // JSON.parse yields `number` for i64 (prices in minor units fit easily).
        let cfg = Config::new()
            .with_out_dir(dir)
            .with_import_extension(Some("js"))
            .with_large_int("number");
        LogIn::export_all(&cfg).unwrap();
        LogInResponse::export_all(&cfg).unwrap();
        Sync::export_all(&cfg).unwrap();
        Checkout::export_all(&cfg).unwrap();
        CheckoutSession::export_all(&cfg).unwrap();
    }
}
