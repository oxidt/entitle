//! `/admin/v1`: projects, API keys, store credentials, products, entitlements,
//! webhook endpoints, revenue metrics, purchase activity, customer support.
//! Requires the bootstrap admin token.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post, put};
use chrono::{DateTime, NaiveDate, Utc};
use entitle_core::metrics::{Interval, Metrics, Range};
use entitle_core::{BillingPeriod, Environment, Money, ProjectId, Store};
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::AppState;
use crate::adapters::save_credentials;
use crate::auth::{Admin, KeyKind};
use crate::crypto::{hash_key, random_token};
use crate::customers::CustomerInfo;
use crate::error::{ApiError, ApiResult, is_unique_violation};
use crate::extract::{Json, Path, Query};
use crate::identity_token;
use crate::outbox::{check_endpoint_url, endpoint_secret_aad};
use crate::routes::sdk::{Grant, existing, grant_entitlement, info, revoke_entitlement};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/projects", post(create_project).get(list_projects))
        .route(
            "/projects/{project_id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route(
            "/projects/{project_id}/identity-secret",
            post(rotate_identity_secret),
        )
        .route("/projects/{project_id}/metrics", get(metrics))
        .route("/projects/{project_id}/activity", get(activity))
        .route("/projects/{project_id}/customers", get(list_customers))
        .route(
            "/projects/{project_id}/customers/{app_user_id}",
            get(get_customer),
        )
        .route(
            "/projects/{project_id}/customers/{app_user_id}/entitlements/{identifier}/grant",
            post(grant),
        )
        .route(
            "/projects/{project_id}/customers/{app_user_id}/entitlements/{identifier}/revoke",
            post(revoke),
        )
        .route(
            "/projects/{project_id}/api-keys",
            post(create_key).get(list_keys),
        )
        .route(
            "/projects/{project_id}/api-keys/{key_id}",
            delete(revoke_key),
        )
        .route("/projects/{project_id}/credentials", get(list_credentials))
        .route(
            "/projects/{project_id}/credentials/{store}",
            put(put_credentials).delete(delete_credentials),
        )
        .route(
            "/projects/{project_id}/credentials/{store}/test",
            post(test_credentials),
        )
        .route(
            "/projects/{project_id}/products",
            post(create_product).get(list_products),
        )
        .route(
            "/projects/{project_id}/products/{product_id}",
            patch(update_product).delete(delete_product),
        )
        .route(
            "/projects/{project_id}/entitlements",
            post(create_entitlement).get(list_entitlements),
        )
        .route(
            "/projects/{project_id}/entitlements/{entitlement_id}",
            delete(delete_entitlement),
        )
        .route(
            "/projects/{project_id}/entitlements/{entitlement_id}/products/{product_id}",
            put(attach_product).delete(detach_product),
        )
        .route(
            "/projects/{project_id}/webhooks",
            post(create_webhook).get(list_webhooks),
        )
        .route(
            "/projects/{project_id}/webhooks/{webhook_id}",
            delete(delete_webhook),
        )
        .route(
            "/projects/{project_id}/webhooks/deliveries",
            get(list_deliveries),
        )
        .route(
            "/projects/{project_id}/webhooks/deliveries/{delivery_id}/retry",
            post(retry_delivery),
        )
}

fn parse_store(s: &str) -> ApiResult<Store> {
    Store::parse(s).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "unknown store `{s}` (creem, app_store, play_store)"
        ))
    })
}

async fn ensure_project(state: &AppState, id: Uuid) -> ApiResult<ProjectId> {
    let found: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM projects WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    found
        .map(|_| ProjectId(id))
        .ok_or_else(|| ApiError::not_found("project not found"))
}

fn deleted(rows: u64, what: &str) -> ApiResult<StatusCode> {
    if rows == 0 {
        Err(ApiError::not_found(format!("{what} not found")))
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

// ---- projects ----

const PROJECT_COLUMNS: &str = "id, name, checkout_redirect_origins, reporting_currency,
    require_signed_identity, sealed_identity_secret IS NOT NULL AS has_identity_secret,
    sandbox_grants_entitlements, created_at";

#[derive(Serialize, FromRow)]
pub(crate) struct Project {
    pub id: Uuid,
    pub name: String,
    pub checkout_redirect_origins: Vec<String>,
    pub reporting_currency: String,
    pub require_signed_identity: bool,
    pub has_identity_secret: bool,
    pub sandbox_grants_entitlements: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct CreateProject {
    name: String,
}

async fn create_project(
    _: Admin,
    State(state): State<AppState>,
    Json(body): Json<CreateProject>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    if body.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name must not be empty".into()));
    }
    let project: Project = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO projects (id, name) VALUES ($1, $2) RETURNING {PROJECT_COLUMNS}"
    )))
    .bind(Uuid::new_v4())
    .bind(body.name.trim())
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

async fn list_projects(_: Admin, State(state): State<AppState>) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(all_projects(&state.db).await?))
}

/// Every project, oldest first.
pub(crate) async fn all_projects(db: &sqlx::PgPool) -> sqlx::Result<Vec<Project>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PROJECT_COLUMNS} FROM projects ORDER BY created_at"
    )))
    .fetch_all(db)
    .await
}

async fn get_project(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> ApiResult<Json<Project>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PROJECT_COLUMNS} FROM projects WHERE id = $1"
    )))
    .bind(project_id)
    .fetch_optional(&state.db)
    .await?
    .map(Json)
    .ok_or_else(|| ApiError::not_found("project not found"))
}

#[derive(Deserialize)]
struct UpdateProject {
    checkout_redirect_origins: Option<Vec<String>>,
    reporting_currency: Option<String>,
    require_signed_identity: Option<bool>,
    sandbox_grants_entitlements: Option<bool>,
}

/// `https://example.com` (scheme, host, optional port; nothing else) → its serialized origin.
fn redirect_origin(s: &str) -> ApiResult<String> {
    let bad = |why: &str| ApiError::BadRequest(format!("invalid origin `{s}`: {why}"));
    let url = reqwest::Url::parse(s).map_err(|e| bad(&e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(bad("must be http(s)://host[:port]"));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(bad("must not have a path, query or fragment"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("must not contain credentials"));
    }
    Ok(url.origin().ascii_serialization())
}

async fn update_project(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(body): Json<UpdateProject>,
) -> ApiResult<Json<Project>> {
    let origins = body
        .checkout_redirect_origins
        .map(|origins| {
            let mut origins = origins
                .iter()
                .map(|o| redirect_origin(o))
                .collect::<ApiResult<Vec<_>>>()?;
            origins.sort();
            origins.dedup();
            ApiResult::Ok(origins)
        })
        .transpose()?;
    let currency = body
        .reporting_currency
        .map(|c| {
            if c.len() == 3 && c.bytes().all(|b| b.is_ascii_alphabetic()) {
                Ok(c.to_ascii_uppercase())
            } else {
                Err(ApiError::BadRequest(format!(
                    "invalid reporting_currency `{c}`: must be a 3-letter ISO 4217 code"
                )))
            }
        })
        .transpose()?;
    if body.require_signed_identity == Some(true) {
        let has_secret: Option<(bool,)> =
            sqlx::query_as("SELECT sealed_identity_secret IS NOT NULL FROM projects WHERE id = $1")
                .bind(project_id)
                .fetch_optional(&state.db)
                .await?;
        if has_secret == Some((false,)) {
            return Err(ApiError::BadRequest(
                "create an identity secret before requiring signed identity".into(),
            ));
        }
    }
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE projects SET checkout_redirect_origins = COALESCE($2, checkout_redirect_origins),
            reporting_currency = COALESCE($3, reporting_currency),
            require_signed_identity = COALESCE($4, require_signed_identity),
            sandbox_grants_entitlements = COALESCE($5, sandbox_grants_entitlements)
         WHERE id = $1 RETURNING {PROJECT_COLUMNS}"
    )))
    .bind(project_id)
    .bind(origins)
    .bind(currency)
    .bind(body.require_signed_identity)
    .bind(body.sandbox_grants_entitlements)
    .fetch_optional(&state.db)
    .await?
    .map(Json)
    .ok_or_else(|| ApiError::not_found("project not found"))
}

#[derive(Serialize)]
struct IdentitySecret {
    identity_secret: String,
}

/// Creates or replaces the project's identity secret; shown once. Replacing it
/// invalidates every token signed with the old one.
async fn rotate_identity_secret(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> ApiResult<(StatusCode, Json<IdentitySecret>)> {
    let secret = random_token("ids_");
    let sealed = state.cipher.seal(
        secret.as_bytes(),
        &identity_token::secret_aad(ProjectId(project_id)),
    );
    let r = sqlx::query("UPDATE projects SET sealed_identity_secret = $2 WHERE id = $1")
        .bind(project_id)
        .bind(sealed)
        .execute(&state.db)
        .await?;
    if r.rows_affected() == 0 {
        return Err(ApiError::not_found("project not found"));
    }
    Ok((
        StatusCode::CREATED,
        Json(IdentitySecret {
            identity_secret: secret,
        }),
    ))
}

async fn delete_project(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let r = sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(project_id)
        .execute(&state.db)
        .await?;
    deleted(r.rows_affected(), "project")
}

// ---- metrics ----

#[derive(Deserialize)]
struct MetricsParams {
    from: NaiveDate,
    to: NaiveDate,
    interval: Interval,
}

async fn metrics(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Query(params): Query<MetricsParams>,
) -> ApiResult<Json<Metrics>> {
    let range = Range::new(params.from, params.to, params.interval)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let currency: Option<(String,)> =
        sqlx::query_as("SELECT reporting_currency FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_optional(&state.db)
            .await?;
    let (currency,) = currency.ok_or_else(|| ApiError::not_found("project not found"))?;
    let metrics =
        crate::metrics::load_and_compute(&state.db, ProjectId(project_id), &currency, &range)
            .await?;
    Ok(Json(metrics))
}

// ---- customer list ----

const CUSTOMERS_DEFAULT_LIMIT: i64 = 50;
const CUSTOMERS_MAX_LIMIT: i64 = 200;

/// A customer's state, from store purchases only (promotional grants do not count).
#[derive(Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CustomerStatus {
    /// A paid (non-trial) purchase grants access now.
    Active,
    /// Access only through trial purchases.
    Trial,
    /// Has purchases, none of which grants access now.
    Churned,
    /// No purchases.
    None,
}

#[derive(Deserialize)]
struct CustomerListParams {
    status: Option<CustomerStatus>,
    store: Option<Store>,
    environment: Option<Environment>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(FromRow, Serialize)]
pub(crate) struct CustomerListItem {
    pub customer_id: Uuid,
    pub app_user_id: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    pub status: String,
    pub stores: Vec<String>,
}

/// Customers, most recently active first. With `store` or `environment`, only customers
/// with a matching purchase are listed, and their status counts only those purchases.
async fn list_customers(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Query(params): Query<CustomerListParams>,
) -> ApiResult<Json<Vec<CustomerListItem>>> {
    let limit = params.limit.unwrap_or(CUSTOMERS_DEFAULT_LIMIT);
    if !(1..=CUSTOMERS_MAX_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!(
            "limit must be between 1 and {CUSTOMERS_MAX_LIMIT}"
        )));
    }
    let offset = params.offset.unwrap_or(0);
    if offset < 0 {
        return Err(ApiError::BadRequest("offset must not be negative".into()));
    }
    let project = ensure_project(&state, project_id).await?;
    let rows = customer_list(
        &state.db,
        project,
        params.status,
        params.store,
        params.environment,
        limit,
        offset,
    )
    .await?;
    Ok(Json(rows))
}

/// Customers, most recently active first (see `list_customers`).
pub(crate) async fn customer_list(
    db: &sqlx::PgPool,
    project: ProjectId,
    status: Option<CustomerStatus>,
    store: Option<Store>,
    environment: Option<Environment>,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<CustomerListItem>> {
    let status = status.map(|s| match s {
        CustomerStatus::Active => "active",
        CustomerStatus::Trial => "trial",
        CustomerStatus::Churned => "churned",
        CustomerStatus::None => "none",
    });
    // `grants` mirrors `access_until` in entitle-core's entitlements.rs; keep them in step.
    sqlx::query_as(
        "WITH per AS (
            SELECT c.id, c.created_at,
                max(p.updated_at) AS last_purchase_at,
                count(p.id) AS purchases,
                bool_or(g.grants AND p.period_type <> 'trial') AS paid,
                bool_or(g.grants AND p.period_type = 'trial') AS trial,
                array_agg(DISTINCT p.store ORDER BY p.store) FILTER (WHERE p.id IS NOT NULL) AS stores
            FROM customers c
            LEFT JOIN purchases p ON p.customer_id = c.id
                AND ($2::text IS NULL OR p.store = $2)
                AND ($3::text IS NULL OR p.environment = $3)
            LEFT JOIN LATERAL (SELECT
                (p.status IN ('active', 'cancelled')
                    AND (p.expires_at > now() OR (p.kind = 'one_time' AND p.expires_at IS NULL)))
                OR (p.status = 'grace_period'
                    AND coalesce(p.grace_period_expires_at, p.expires_at) > now()) AS grants
            ) g ON true
            WHERE c.project_id = $1
            GROUP BY c.id
         ), classified AS (
            SELECT id, created_at, coalesce(stores, '{}') AS stores,
                greatest(created_at, last_purchase_at) AS last_activity_at,
                CASE WHEN paid THEN 'active' WHEN trial THEN 'trial'
                     WHEN purchases > 0 THEN 'churned' ELSE 'none' END AS status
            FROM per
            WHERE ($2::text IS NULL AND $3::text IS NULL) OR purchases > 0
         )
         SELECT k.id AS customer_id,
            (SELECT a.app_user_id FROM customer_aliases a
             WHERE a.customer_id = k.id
             ORDER BY a.created_at DESC, a.app_user_id LIMIT 1) AS app_user_id,
            k.created_at AS first_seen_at, k.last_activity_at, k.status, k.stores
         FROM classified k
         WHERE $4::text IS NULL OR k.status = $4
         ORDER BY k.last_activity_at DESC, k.id
         LIMIT $5 OFFSET $6",
    )
    .bind(project.0)
    .bind(store.map(Store::as_str))
    .bind(environment.map(Environment::as_str))
    .bind(status)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
}

// ---- activity ----

const ACTIVITY_DEFAULT_LIMIT: i64 = 50;
const ACTIVITY_MAX_LIMIT: i64 = 200;

#[derive(Deserialize)]
struct ActivityParams {
    /// Both environments when absent.
    environment: Option<Environment>,
    /// Only this customer's purchases.
    app_user_id: Option<String>,
    limit: Option<i64>,
}

#[derive(FromRow)]
struct ActivityRow {
    id: i64,
    recorded_at: DateTime<Utc>,
    store: String,
    environment: String,
    product_id: String,
    app_user_id: Option<String>,
    events: Vec<String>,
    status: String,
    period_type: String,
    expires_at: Option<DateTime<Utc>>,
    auto_renew: bool,
    price_amount_minor: Option<i64>,
    price_currency: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ActivityOut {
    pub id: i64,
    pub recorded_at: DateTime<Utc>,
    pub store: String,
    pub environment: String,
    pub product_id: String,
    pub app_user_id: Option<String>,
    pub events: Vec<String>,
    pub status: String,
    pub period_type: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub auto_renew: bool,
    pub price: Option<Money>,
}

/// Newest recorded purchase transitions (store syncs and notifications), for checking
/// that a store integration works end to end.
async fn activity(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Query(params): Query<ActivityParams>,
) -> ApiResult<Json<Vec<ActivityOut>>> {
    let limit = params.limit.unwrap_or(ACTIVITY_DEFAULT_LIMIT);
    if !(1..=ACTIVITY_MAX_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!(
            "limit must be between 1 and {ACTIVITY_MAX_LIMIT}"
        )));
    }
    let project = ensure_project(&state, project_id).await?;
    let customer = match &params.app_user_id {
        Some(app_user_id) => Some(existing(&state, project, app_user_id).await?.0),
        None => None,
    };
    Ok(Json(
        recent_activity(&state.db, project, params.environment, customer, limit).await?,
    ))
}

/// The project's newest recorded purchase transitions, optionally for one
/// environment or customer.
pub(crate) async fn recent_activity(
    db: &sqlx::PgPool,
    project: ProjectId,
    environment: Option<Environment>,
    customer: Option<Uuid>,
    limit: i64,
) -> sqlx::Result<Vec<ActivityOut>> {
    // Backfilled rows were seeded at migration time, not observed.
    let rows: Vec<ActivityRow> = sqlx::query_as(
        "SELECT h.id, h.recorded_at, h.store, h.environment, h.product_id,
            (SELECT a.app_user_id FROM customer_aliases a
             WHERE a.customer_id = h.customer_id
             ORDER BY a.created_at DESC, a.app_user_id LIMIT 1) AS app_user_id,
            h.events, h.status, h.period_type, h.expires_at, h.auto_renew,
            h.price_amount_minor, h.price_currency
         FROM purchase_history h
         WHERE h.project_id = $1 AND NOT h.backfilled
           AND ($2::text IS NULL OR h.environment = $2)
           AND ($3::uuid IS NULL OR h.customer_id = $3)
         ORDER BY h.id DESC
         LIMIT $4",
    )
    .bind(project.0)
    .bind(environment.map(Environment::as_str))
    .bind(customer)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ActivityOut {
            id: r.id,
            recorded_at: r.recorded_at,
            store: r.store,
            environment: r.environment,
            product_id: r.product_id,
            app_user_id: r.app_user_id,
            events: r.events,
            status: r.status,
            period_type: r.period_type,
            expires_at: r.expires_at,
            auto_renew: r.auto_renew,
            price: match (r.price_amount_minor, r.price_currency) {
                (Some(amount_minor), Some(currency)) => Some(Money {
                    amount_minor,
                    currency,
                }),
                _ => None,
            },
        })
        .collect())
}

// ---- customers ----

async fn get_customer(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, app_user_id)): Path<(Uuid, String)>,
) -> ApiResult<Json<CustomerInfo>> {
    let project = ensure_project(&state, project_id).await?;
    // Unlike the SDK endpoint, a lookup never creates the customer.
    let customer = existing(&state, project, &app_user_id).await?;
    info(&state, project, customer, &app_user_id).await
}

async fn grant(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, app_user_id, identifier)): Path<(Uuid, String, String)>,
    body: Option<Json<Grant>>,
) -> ApiResult<Json<CustomerInfo>> {
    let project = ensure_project(&state, project_id).await?;
    let Json(body) = body.unwrap_or_default();
    grant_entitlement(&state, project, &app_user_id, &identifier, body).await
}

async fn revoke(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, app_user_id, identifier)): Path<(Uuid, String, String)>,
) -> ApiResult<Json<CustomerInfo>> {
    let project = ensure_project(&state, project_id).await?;
    revoke_entitlement(&state, project, &app_user_id, &identifier).await
}

// ---- API keys ----

#[derive(Deserialize)]
struct CreateKey {
    kind: String,
}

#[derive(Serialize, FromRow)]
struct ApiKey {
    id: Uuid,
    kind: String,
    key_prefix: String,
    created_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct CreatedKey {
    #[serde(flatten)]
    meta: ApiKey,
    /// Shown only once.
    key: String,
}

async fn create_key(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(body): Json<CreateKey>,
) -> ApiResult<(StatusCode, Json<CreatedKey>)> {
    let project = ensure_project(&state, project_id).await?;
    let kind = match body.kind.as_str() {
        "public" => KeyKind::Public,
        "secret" => KeyKind::Secret,
        _ => {
            return Err(ApiError::BadRequest(
                "kind must be `public` or `secret`".into(),
            ));
        }
    };
    let key = random_token(kind.prefix());
    let meta: ApiKey = sqlx::query_as(
        "INSERT INTO api_keys (id, project_id, kind, key_hash, key_prefix) VALUES ($1, $2, $3, $4, $5)
         RETURNING id, kind, key_prefix, created_at, revoked_at",
    )
    .bind(Uuid::new_v4())
    .bind(project.0)
    .bind(kind.as_str())
    .bind(hash_key(&key))
    .bind(&key[..11])
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(CreatedKey { meta, key })))
}

async fn list_keys(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> ApiResult<Json<Vec<ApiKey>>> {
    let project = ensure_project(&state, project_id).await?;
    let keys = sqlx::query_as(
        "SELECT id, kind, key_prefix, created_at, revoked_at FROM api_keys WHERE project_id = $1 ORDER BY created_at",
    )
    .bind(project.0)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(keys))
}

async fn revoke_key(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, key_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let r = sqlx::query(
        "UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND project_id = $2 AND revoked_at IS NULL",
    )
    .bind(key_id)
    .bind(project_id)
    .execute(&state.db)
    .await?;
    deleted(r.rows_affected(), "api key")
}

// ---- store credentials ----

#[derive(Serialize, FromRow)]
struct ConfiguredStore {
    store: String,
    updated_at: DateTime<Utc>,
}

async fn put_credentials(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, store)): Path<(Uuid, String)>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<StatusCode> {
    let project = ensure_project(&state, project_id).await?;
    let store = parse_store(&store)?;
    save_credentials(&state.db, &state.cipher, project, store, body).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_credentials(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> ApiResult<Json<Vec<ConfiguredStore>>> {
    let project = ensure_project(&state, project_id).await?;
    let stores = sqlx::query_as(
        "SELECT store, updated_at FROM store_credentials WHERE project_id = $1 ORDER BY store",
    )
    .bind(project.0)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(stores))
}

#[derive(Serialize)]
struct CredentialsCheck {
    ok: bool,
    message: String,
}

/// Calls the store with the saved credentials. A rejection is a result (`ok: false`),
/// not an error; 404 only when no credentials are saved.
async fn test_credentials(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, store)): Path<(Uuid, String)>,
) -> ApiResult<Json<CredentialsCheck>> {
    let project = ensure_project(&state, project_id).await?;
    let store = parse_store(&store)?;
    let result = state
        .adapters
        .check_credentials(&state.db, &state.cipher, project, store)
        .await?;
    Ok(Json(match result {
        Ok(message) => CredentialsCheck { ok: true, message },
        Err(e) => CredentialsCheck {
            ok: false,
            message: e.to_string(),
        },
    }))
}

async fn delete_credentials(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, store)): Path<(Uuid, String)>,
) -> ApiResult<StatusCode> {
    let store = parse_store(&store)?;
    let r = sqlx::query("DELETE FROM store_credentials WHERE project_id = $1 AND store = $2")
        .bind(project_id)
        .bind(store.as_str())
        .execute(&state.db)
        .await?;
    deleted(r.rows_affected(), "credentials")
}

// ---- products ----

const PRODUCT_COLUMNS: &str =
    "id, store, store_product_id, display_name, billing_period, created_at";

#[derive(Deserialize)]
struct CreateProduct {
    store: String,
    store_product_id: String,
    display_name: Option<String>,
    billing_period: Option<String>,
}

#[derive(Serialize, FromRow)]
struct ProductOut {
    id: Uuid,
    store: String,
    store_product_id: String,
    display_name: Option<String>,
    billing_period: Option<String>,
    created_at: DateTime<Utc>,
}

fn parse_billing_period(s: &str) -> ApiResult<BillingPeriod> {
    BillingPeriod::parse(s).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "unknown billing_period `{s}` (P1W, P1M, P2M, P3M, P6M, P1Y)"
        ))
    })
}

async fn create_product(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(body): Json<CreateProduct>,
) -> ApiResult<(StatusCode, Json<ProductOut>)> {
    let project = ensure_project(&state, project_id).await?;
    let store = parse_store(&body.store)?;
    if body.store_product_id.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "store_product_id must not be empty".into(),
        ));
    }
    let billing_period = body
        .billing_period
        .as_deref()
        .map(parse_billing_period)
        .transpose()?;
    let product = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO products (id, project_id, store, store_product_id, display_name, billing_period)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING {PRODUCT_COLUMNS}"
    )))
    .bind(Uuid::new_v4())
    .bind(project.0)
    .bind(store.as_str())
    .bind(body.store_product_id.trim())
    .bind(body.display_name)
    .bind(billing_period.map(BillingPeriod::as_str))
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            ApiError::Conflict("product already exists for this store".into())
        } else {
            e.into()
        }
    })?;
    Ok((StatusCode::CREATED, Json(product)))
}

async fn list_products(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> ApiResult<Json<Vec<ProductOut>>> {
    let project = ensure_project(&state, project_id).await?;
    let products = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PRODUCT_COLUMNS} FROM products WHERE project_id = $1 ORDER BY created_at"
    )))
    .bind(project.0)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(products))
}

/// Fields that are absent stay unchanged; `null` clears them.
#[derive(Deserialize)]
struct UpdateProduct {
    #[serde(default, deserialize_with = "present")]
    display_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    billing_period: Option<Option<String>>,
}

/// Tells an explicit `null` (`Some(None)`) from an absent field (`None`).
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

async fn update_product(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, product_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateProduct>,
) -> ApiResult<Json<ProductOut>> {
    let billing_period = match &body.billing_period {
        Some(Some(p)) => Some(Some(parse_billing_period(p)?)),
        Some(None) => Some(None),
        None => None,
    };
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE products SET
            display_name = CASE WHEN $3 THEN $4 ELSE display_name END,
            billing_period = CASE WHEN $5 THEN $6 ELSE billing_period END
         WHERE id = $1 AND project_id = $2
         RETURNING {PRODUCT_COLUMNS}"
    )))
    .bind(product_id)
    .bind(project_id)
    .bind(body.display_name.is_some())
    .bind(body.display_name.flatten())
    .bind(billing_period.is_some())
    .bind(billing_period.flatten().map(BillingPeriod::as_str))
    .fetch_optional(&state.db)
    .await?
    .map(Json)
    .ok_or_else(|| ApiError::not_found("product not found"))
}

async fn delete_product(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, product_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let r = sqlx::query("DELETE FROM products WHERE id = $1 AND project_id = $2")
        .bind(product_id)
        .bind(project_id)
        .execute(&state.db)
        .await?;
    deleted(r.rows_affected(), "product")
}

// ---- entitlements ----

#[derive(Deserialize)]
struct CreateEntitlement {
    identifier: String,
    description: Option<String>,
}

#[derive(Serialize, FromRow)]
pub(crate) struct EntitlementOut {
    pub id: Uuid,
    pub identifier: String,
    pub description: Option<String>,
    pub product_ids: Vec<Uuid>,
    pub created_at: DateTime<Utc>,
}

async fn create_entitlement(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(body): Json<CreateEntitlement>,
) -> ApiResult<(StatusCode, Json<EntitlementOut>)> {
    let project = ensure_project(&state, project_id).await?;
    if body.identifier.trim().is_empty() {
        return Err(ApiError::BadRequest("identifier must not be empty".into()));
    }
    let ent = sqlx::query_as(
        "INSERT INTO entitlements (id, project_id, identifier, description) VALUES ($1, $2, $3, $4)
         RETURNING id, identifier, description, ARRAY[]::uuid[] AS product_ids, created_at",
    )
    .bind(Uuid::new_v4())
    .bind(project.0)
    .bind(body.identifier.trim())
    .bind(body.description)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            ApiError::Conflict("entitlement identifier already exists".into())
        } else {
            e.into()
        }
    })?;
    Ok((StatusCode::CREATED, Json(ent)))
}

async fn list_entitlements(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> ApiResult<Json<Vec<EntitlementOut>>> {
    let project = ensure_project(&state, project_id).await?;
    Ok(Json(entitlements_of(&state.db, project).await?))
}

/// The project's entitlements with their product ids, oldest first.
pub(crate) async fn entitlements_of(
    db: &sqlx::PgPool,
    project: ProjectId,
) -> sqlx::Result<Vec<EntitlementOut>> {
    sqlx::query_as(
        "SELECT e.id, e.identifier, e.description, e.created_at,
            COALESCE(array_agg(ep.product_id ORDER BY ep.product_id)
                FILTER (WHERE ep.product_id IS NOT NULL), ARRAY[]::uuid[]) AS product_ids
         FROM entitlements e LEFT JOIN entitlement_products ep ON ep.entitlement_id = e.id
         WHERE e.project_id = $1
         GROUP BY e.id ORDER BY e.created_at",
    )
    .bind(project.0)
    .fetch_all(db)
    .await
}

async fn delete_entitlement(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, entitlement_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let r = sqlx::query("DELETE FROM entitlements WHERE id = $1 AND project_id = $2")
        .bind(entitlement_id)
        .bind(project_id)
        .execute(&state.db)
        .await?;
    deleted(r.rows_affected(), "entitlement")
}

async fn attach_product(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, entitlement_id, product_id)): Path<(Uuid, Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    // Both must belong to the project.
    let r = sqlx::query(
        "INSERT INTO entitlement_products (entitlement_id, product_id)
         SELECT e.id, p.id FROM entitlements e, products p
         WHERE e.id = $1 AND p.id = $2 AND e.project_id = $3 AND p.project_id = $3
         ON CONFLICT DO NOTHING",
    )
    .bind(entitlement_id)
    .bind(product_id)
    .bind(project_id)
    .execute(&state.db)
    .await?;
    if r.rows_affected() == 0 {
        let exists: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM entitlement_products ep JOIN entitlements e ON e.id = ep.entitlement_id
             WHERE ep.entitlement_id = $1 AND ep.product_id = $2 AND e.project_id = $3",
        )
        .bind(entitlement_id)
        .bind(product_id)
        .bind(project_id)
        .fetch_optional(&state.db)
        .await?;
        if exists.is_none() {
            return Err(ApiError::not_found("entitlement or product not found"));
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn detach_product(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, entitlement_id, product_id)): Path<(Uuid, Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let r = sqlx::query(
        "DELETE FROM entitlement_products ep USING entitlements e
         WHERE ep.entitlement_id = e.id AND e.project_id = $3
           AND ep.entitlement_id = $1 AND ep.product_id = $2",
    )
    .bind(entitlement_id)
    .bind(product_id)
    .bind(project_id)
    .execute(&state.db)
    .await?;
    deleted(r.rows_affected(), "attachment")
}

// ---- outgoing webhook endpoints ----

#[derive(Deserialize)]
struct CreateWebhook {
    url: String,
}

#[derive(Serialize, FromRow)]
struct WebhookOut {
    id: Uuid,
    url: String,
    created_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct CreatedWebhook {
    #[serde(flatten)]
    meta: WebhookOut,
    /// Signing secret, shown only once.
    secret: String,
}

async fn create_webhook(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(body): Json<CreateWebhook>,
) -> ApiResult<(StatusCode, Json<CreatedWebhook>)> {
    let project = ensure_project(&state, project_id).await?;
    let url =
        check_endpoint_url(&body.url, state.webhook_allow_private).map_err(ApiError::BadRequest)?;
    let id = Uuid::new_v4();
    let secret = random_token("whsec_");
    let sealed = state
        .cipher
        .seal(secret.as_bytes(), &endpoint_secret_aad(id));
    let meta = sqlx::query_as(
        "INSERT INTO webhook_endpoints (id, project_id, url, sealed_secret) VALUES ($1, $2, $3, $4)
         RETURNING id, url, created_at",
    )
    .bind(id)
    .bind(project.0)
    .bind(url.as_str())
    .bind(sealed)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(CreatedWebhook { meta, secret })))
}

async fn list_webhooks(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> ApiResult<Json<Vec<WebhookOut>>> {
    let project = ensure_project(&state, project_id).await?;
    let hooks = sqlx::query_as(
        "SELECT id, url, created_at FROM webhook_endpoints WHERE project_id = $1 ORDER BY created_at",
    )
    .bind(project.0)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(hooks))
}

const DELIVERY_STATUSES: [&str; 3] = ["pending", "delivered", "failed"];

#[derive(Deserialize)]
struct DeliveryParams {
    status: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize, FromRow)]
struct DeliveryOut {
    id: Uuid,
    endpoint_id: Uuid,
    url: String,
    event_id: Uuid,
    event_type: String,
    status: String,
    attempts: i32,
    last_error: Option<String>,
    /// When the next attempt is due; only for pending deliveries.
    next_attempt_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    delivered_at: Option<DateTime<Utc>>,
    payload: serde_json::Value,
}

/// Newest outgoing webhook deliveries of the project's endpoints.
async fn list_deliveries(
    _: Admin,
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Query(params): Query<DeliveryParams>,
) -> ApiResult<Json<Vec<DeliveryOut>>> {
    let limit = params.limit.unwrap_or(ACTIVITY_DEFAULT_LIMIT);
    if !(1..=ACTIVITY_MAX_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!(
            "limit must be between 1 and {ACTIVITY_MAX_LIMIT}"
        )));
    }
    if let Some(status) = &params.status
        && !DELIVERY_STATUSES.contains(&status.as_str())
    {
        return Err(ApiError::BadRequest(format!(
            "unknown status `{status}` (pending, delivered, failed)"
        )));
    }
    let project = ensure_project(&state, project_id).await?;
    let deliveries = sqlx::query_as(
        "SELECT o.id, o.endpoint_id, e.url, o.event_id, o.event_type, o.status, o.attempts,
            o.last_error, CASE WHEN o.status = 'pending' THEN o.next_attempt_at END AS next_attempt_at,
            o.created_at, o.delivered_at, o.payload
         FROM outbox o JOIN webhook_endpoints e ON e.id = o.endpoint_id
         WHERE e.project_id = $1 AND ($2::text IS NULL OR o.status = $2)
         ORDER BY o.created_at DESC, o.id
         LIMIT $3",
    )
    .bind(project.0)
    .bind(params.status)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(deliveries))
}

/// Sends a delivery again as soon as the worker runs: pending ones skip their
/// backoff, failed and delivered ones are queued once more.
async fn retry_delivery(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, delivery_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let project = ensure_project(&state, project_id).await?;
    let row: Option<(bool,)> = sqlx::query_as(
        "SELECT o.locked_until IS NOT NULL AND o.locked_until > now()
         FROM outbox o JOIN webhook_endpoints e ON e.id = o.endpoint_id
         WHERE o.id = $1 AND e.project_id = $2",
    )
    .bind(delivery_id)
    .bind(project.0)
    .fetch_optional(&state.db)
    .await?;
    match row {
        None => return Err(ApiError::not_found("delivery not found")),
        Some((true,)) => {
            return Err(ApiError::Conflict(
                "the delivery is being sent right now".into(),
            ));
        }
        Some((false,)) => {}
    }
    // A failed delivery keeps its attempt count, so it gets one more try.
    sqlx::query(
        "UPDATE outbox SET status = 'pending', next_attempt_at = now(), delivered_at = NULL
         WHERE id = $1 AND (locked_until IS NULL OR locked_until <= now())",
    )
    .bind(delivery_id)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_webhook(
    _: Admin,
    State(state): State<AppState>,
    Path((project_id, webhook_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let r = sqlx::query("DELETE FROM webhook_endpoints WHERE id = $1 AND project_id = $2")
        .bind(webhook_id)
        .bind(project_id)
        .execute(&state.db)
        .await?;
    deleted(r.rows_affected(), "webhook endpoint")
}
