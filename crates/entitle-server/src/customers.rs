//! Customers, aliases, login/merge, CustomerInfo, GDPR export and delete.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use entitle_core::identity::{
    IdentityError, KnownCustomer, LoginPlan, is_anonymous, plan_login, validate_app_user_id,
};
use entitle_core::{
    ActiveEntitlement, CustomerId, Environment, Money, PeriodType, ProjectId, PromotionalGrant,
    PurchaseKind, PurchaseSnapshot, PurchaseStatus, Store, compute_entitlements,
};
use serde::Serialize;
use sqlx::{Acquire, FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::auth::KeyKind;
use crate::error::{ApiError, ApiResult, is_unique_violation};
use crate::purchases::{customer_purchases, load_mapping};

/// A purchase as exposed by the API.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct PurchaseView {
    pub store: Store,
    pub environment: Environment,
    pub product_id: String,
    /// Secret key only: for Google this is the purchase token.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub original_transaction_id: Option<String>,
    /// Secret key only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub store_transaction_id: Option<String>,
    pub kind: PurchaseKind,
    pub status: PurchaseStatus,
    pub period_type: PeriodType,
    pub purchased_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub grace_period_expires_at: Option<DateTime<Utc>>,
    pub auto_renew: bool,
    pub price: Option<Money>,
}

impl From<&PurchaseSnapshot> for PurchaseView {
    fn from(p: &PurchaseSnapshot) -> Self {
        PurchaseView {
            store: p.store,
            environment: p.environment,
            product_id: p.product_id.clone(),
            original_transaction_id: Some(p.original_transaction_id.clone()),
            store_transaction_id: Some(p.store_transaction_id.clone()),
            kind: p.kind,
            status: p.status,
            period_type: p.period_type,
            purchased_at: p.purchased_at,
            expires_at: p.expires_at,
            grace_period_expires_at: p.grace_period_expires_at,
            auto_renew: p.auto_renew,
            price: p.price.clone(),
        }
    }
}

/// RevenueCat-style customer info returned by the SDK API.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CustomerInfo {
    /// Stable id. Pass it as Apple `appAccountToken` / Google `obfuscatedExternalAccountId`.
    pub customer_id: Uuid,
    /// The app user id this request was made for.
    pub app_user_id: String,
    /// Secret key only: other aliases (e.g. anonymous ids) would let a public-key
    /// caller act as that customer.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub aliases: Option<Vec<String>>,
    /// Active entitlements, keyed by identifier.
    pub entitlements: BTreeMap<String, ActiveEntitlement>,
    /// All purchases, newest first.
    pub purchases: Vec<PurchaseView>,
    /// Where the user manages their subscription (App Store / Play Store). `null` for
    /// Creem: use the portal endpoint instead.
    pub management_url: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub request_date: DateTime<Utc>,
}

impl CustomerInfo {
    /// Drops what a public key must not see: the public key ships in apps, so
    /// anyone can call with any app user id.
    pub fn redacted(mut self) -> Self {
        self.aliases = None;
        for p in &mut self.purchases {
            p.original_transaction_id = None;
            p.store_transaction_id = None;
        }
        self
    }

    /// [`Self::redacted`] unless the caller holds the secret key.
    pub fn for_key(self, kind: KeyKind) -> Self {
        match kind {
            KeyKind::Public => self.redacted(),
            KeyKind::Secret => self,
        }
    }
}

pub async fn find(
    conn: &mut PgConnection,
    project: ProjectId,
    app_user_id: &str,
) -> anyhow::Result<Option<CustomerId>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT customer_id FROM customer_aliases WHERE project_id = $1 AND app_user_id = $2",
    )
    .bind(project.0)
    .bind(app_user_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(id,)| CustomerId(id)))
}

/// Most app user ids (aliases) one customer can have.
pub const MAX_ALIASES: i64 = 100;

/// Looks the customer up by app user id, creating it on first sight.
pub async fn get_or_create(
    db: &PgPool,
    project: ProjectId,
    app_user_id: &str,
) -> ApiResult<CustomerId> {
    validate_app_user_id(app_user_id)?;
    let mut conn = db.acquire().await?;
    if let Some(id) = find(&mut conn, project, app_user_id).await? {
        return Ok(id);
    }
    // Reuse the connection: taking a second one from the pool while holding the
    // first can deadlock the pool under concurrent first-sight requests.
    let mut tx = conn.begin().await?;
    match create(&mut tx, project, app_user_id).await {
        Ok(id) => {
            tx.commit().await?;
            Ok(id)
        }
        // Lost a race against a concurrent request creating the same alias.
        Err(e) if is_unique_violation(&e) => {
            drop(tx);
            find(&mut conn, project, app_user_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("alias vanished after conflict").into())
        }
        Err(e) => Err(e.into()),
    }
}

async fn create(
    conn: &mut PgConnection,
    project: ProjectId,
    app_user_id: &str,
) -> Result<CustomerId, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO customers (id, project_id) VALUES ($1, $2)")
        .bind(id)
        .bind(project.0)
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO account_tokens (token, customer_id) VALUES ($1, $1)")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    add_alias(conn, project, CustomerId(id), app_user_id).await?;
    Ok(CustomerId(id))
}

async fn add_alias(
    conn: &mut PgConnection,
    project: ProjectId,
    customer: CustomerId,
    app_user_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO customer_aliases (project_id, app_user_id, customer_id) VALUES ($1, $2, $3)",
    )
    .bind(project.0)
    .bind(app_user_id)
    .bind(customer.0)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn aliases(conn: &mut PgConnection, customer: CustomerId) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT app_user_id FROM customer_aliases WHERE customer_id = $1 ORDER BY created_at, app_user_id",
    )
    .bind(customer.0)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|(a,)| a).collect())
}

async fn active_grants(
    conn: &mut PgConnection,
    customer: CustomerId,
) -> anyhow::Result<Vec<PromotionalGrant>> {
    let rows: Vec<(String, DateTime<Utc>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT e.identifier, g.granted_at, g.expires_at
         FROM promotional_grants g JOIN entitlements e ON e.id = g.entitlement_id
         WHERE g.customer_id = $1 AND g.revoked_at IS NULL",
    )
    .bind(customer.0)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(entitlement, granted_at, expires_at)| PromotionalGrant {
            entitlement,
            granted_at,
            expires_at,
        })
        .collect())
}

async fn compute(
    conn: &mut PgConnection,
    project: ProjectId,
    customer: CustomerId,
    purchases: &[PurchaseSnapshot],
    now: DateTime<Utc>,
) -> anyhow::Result<BTreeMap<String, ActiveEntitlement>> {
    let (sandbox_grants,): (bool,) =
        sqlx::query_as("SELECT sandbox_grants_entitlements FROM projects WHERE id = $1")
            .bind(project.0)
            .fetch_one(&mut *conn)
            .await?;
    let production: Vec<PurchaseSnapshot>;
    let purchases = if sandbox_grants {
        purchases
    } else {
        production = purchases
            .iter()
            .filter(|p| p.environment == Environment::Production)
            .cloned()
            .collect();
        &production
    };
    let mapping = load_mapping(conn, project).await?;
    let grants = active_grants(conn, customer).await?;
    Ok(compute_entitlements(purchases, &mapping, &grants, now))
}

pub async fn entitlements(
    conn: &mut PgConnection,
    project: ProjectId,
    customer: CustomerId,
    now: DateTime<Utc>,
) -> anyhow::Result<BTreeMap<String, ActiveEntitlement>> {
    let purchases = customer_purchases(conn, customer).await?;
    compute(conn, project, customer, &purchases, now).await
}

fn management_url(
    purchases: &[PurchaseSnapshot],
    play_package: Option<&str>,
    now: DateTime<Utc>,
) -> Option<String> {
    let latest = purchases
        .iter()
        .filter(|p| p.kind == PurchaseKind::Subscription && p.expires_at.is_some_and(|e| e > now))
        .max_by_key(|p| p.expires_at)?;
    match latest.store {
        Store::AppStore => Some("https://apps.apple.com/account/subscriptions".into()),
        Store::PlayStore => Some(match play_package {
            Some(package) => format!(
                "https://play.google.com/store/account/subscriptions?sku={}&package={package}",
                latest.product_id
            ),
            None => format!(
                "https://play.google.com/store/account/subscriptions?sku={}",
                latest.product_id
            ),
        }),
        Store::Creem => None,
    }
}

pub async fn info(
    conn: &mut PgConnection,
    project: ProjectId,
    customer: CustomerId,
    app_user_id: &str,
    play_package: Option<&str>,
    now: DateTime<Utc>,
) -> anyhow::Result<CustomerInfo> {
    let (first_seen_at,): (DateTime<Utc>,) =
        sqlx::query_as("SELECT created_at FROM customers WHERE id = $1")
            .bind(customer.0)
            .fetch_one(&mut *conn)
            .await?;
    let purchases = customer_purchases(conn, customer).await?;
    let entitlements = compute(conn, project, customer, &purchases, now).await?;
    Ok(CustomerInfo {
        customer_id: customer.0,
        app_user_id: app_user_id.to_owned(),
        aliases: Some(aliases(conn, customer).await?),
        entitlements,
        management_url: management_url(&purchases, play_package, now),
        purchases: purchases.iter().map(PurchaseView::from).collect(),
        first_seen_at,
        request_date: now,
    })
}

/// Logs `current` in as `new`. Returns the customer `new` now resolves to and
/// whether `new` was seen for the first time.
pub async fn log_in(
    db: &PgPool,
    project: ProjectId,
    current: &str,
    new: &str,
) -> ApiResult<(CustomerId, bool)> {
    validate_app_user_id(current)?;
    validate_app_user_id(new)?;
    if is_anonymous(new) {
        return Err(IdentityError::LoginToAnonymous.into());
    }
    let mut tx = db.begin().await?;
    let current_customer = match find(&mut tx, project, current).await? {
        Some(id) => {
            sqlx::query("SELECT id FROM customers WHERE id = $1 FOR UPDATE")
                .bind(id.0)
                .execute(&mut *tx)
                .await?;
            Some(KnownCustomer {
                id,
                aliases: aliases(&mut tx, id).await?,
            })
        }
        None => None,
    };
    let target = find(&mut tx, project, new).await?;
    let plan = plan_login(current_customer.as_ref(), target);
    // Every alias is returned to the secret key and sent in every webhook: bound
    // them, so repeated logins cannot grow one customer without limit.
    let added = match (&plan, &current_customer) {
        (LoginPlan::AttachAlias(id), _) => Some((*id, 1)),
        (LoginPlan::Merge { into, .. }, Some(from)) => Some((*into, from.aliases.len() as i64)),
        _ => None,
    };
    if let Some((customer, added)) = added {
        // Lock the target so concurrent logins into it are counted one after another.
        sqlx::query("SELECT id FROM customers WHERE id = $1 FOR UPDATE")
            .bind(customer.0)
            .execute(&mut *tx)
            .await?;
        let (count,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM customer_aliases WHERE customer_id = $1")
                .bind(customer.0)
                .fetch_one(&mut *tx)
                .await?;
        if count + added > MAX_ALIASES {
            return Err(ApiError::Conflict(format!(
                "customer has reached the limit of {MAX_ALIASES} app user ids"
            )));
        }
    }
    let result = match plan {
        LoginPlan::AlreadyLinked(id) => Ok((id, false)),
        LoginPlan::AttachAlias(id) => add_alias(&mut tx, project, id, new)
            .await
            .map(|()| (id, true)),
        LoginPlan::Merge { from, into } => merge(&mut tx, from, into).await.map(|()| (into, false)),
        LoginPlan::Switch { target: Some(id) } => Ok((id, false)),
        LoginPlan::Switch { target: None } => {
            create(&mut tx, project, new).await.map(|id| (id, true))
        }
    };
    match result {
        Ok(r) => {
            tx.commit().await?;
            Ok(r)
        }
        Err(e) if is_unique_violation(&e) => Err(ApiError::Conflict(
            "concurrent login for the same app user id, retry".into(),
        )),
        Err(e) => Err(e.into()),
    }
}

/// Moves everything owned by `from` to `into` and deletes `from`.
async fn merge(
    conn: &mut PgConnection,
    from: CustomerId,
    into: CustomerId,
) -> Result<(), sqlx::Error> {
    for table in [
        "customer_aliases",
        "account_tokens",
        "purchases",
        "purchase_history",
        "promotional_grants",
        "outbox",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET customer_id = $2 WHERE customer_id = $1"
        )))
        .bind(from.0)
        .bind(into.0)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query("DELETE FROM customers WHERE id = $1")
        .bind(from.0)
        .execute(conn)
        .await?;
    Ok(())
}

/// GDPR erasure: hard-deletes the customer, aliases, account tokens, purchases,
/// promotional grants and every outgoing webhook payload about them. Their
/// `purchase_history` rows stay for aggregate metrics, unlinked (FK sets
/// `customer_id` to NULL).
pub async fn delete(db: &PgPool, customer: CustomerId) -> ApiResult<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM outbox WHERE customer_id = $1")
        .bind(customer.0)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM purchases WHERE customer_id = $1")
        .bind(customer.0)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM customers WHERE id = $1")
        .bind(customer.0)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Serialize, FromRow)]
struct GrantExport {
    entitlement: String,
    granted_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, FromRow)]
struct DeliveryExport {
    payload: serde_json::Value,
    status: String,
    attempts: i32,
}

#[derive(Serialize, FromRow)]
struct HistoryExport {
    purchase_id: Uuid,
    store: String,
    environment: String,
    store_transaction_id: String,
    product_id: String,
    kind: String,
    status: String,
    period_type: String,
    purchased_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    grace_period_expires_at: Option<DateTime<Utc>>,
    auto_renew: bool,
    price_amount_minor: Option<i64>,
    price_currency: Option<String>,
    as_of: DateTime<Utc>,
    events: Vec<String>,
    backfilled: bool,
    recorded_at: DateTime<Utc>,
}

/// GDPR access request: everything stored about the customer.
pub async fn export(
    conn: &mut PgConnection,
    project: ProjectId,
    customer: CustomerId,
    now: DateTime<Utc>,
) -> anyhow::Result<serde_json::Value> {
    let (created_at,): (DateTime<Utc>,) =
        sqlx::query_as("SELECT created_at FROM customers WHERE id = $1")
            .bind(customer.0)
            .fetch_one(&mut *conn)
            .await?;
    let tokens: Vec<(Uuid,)> =
        sqlx::query_as("SELECT token FROM account_tokens WHERE customer_id = $1 ORDER BY token")
            .bind(customer.0)
            .fetch_all(&mut *conn)
            .await?;
    let purchases = customer_purchases(conn, customer).await?;
    let grants: Vec<GrantExport> = sqlx::query_as(
        "SELECT e.identifier AS entitlement, g.granted_at, g.expires_at, g.revoked_at
         FROM promotional_grants g JOIN entitlements e ON e.id = g.entitlement_id
         WHERE g.customer_id = $1 ORDER BY g.granted_at",
    )
    .bind(customer.0)
    .fetch_all(&mut *conn)
    .await?;
    let deliveries: Vec<DeliveryExport> = sqlx::query_as(
        "SELECT payload, status, attempts FROM outbox WHERE customer_id = $1 ORDER BY created_at",
    )
    .bind(customer.0)
    .fetch_all(&mut *conn)
    .await?;
    let history: Vec<HistoryExport> = sqlx::query_as(
        "SELECT purchase_id, store, environment, store_transaction_id, product_id, kind, status,
            period_type, purchased_at, expires_at, grace_period_expires_at, auto_renew,
            price_amount_minor, price_currency, as_of, events, backfilled, recorded_at
         FROM purchase_history WHERE customer_id = $1 ORDER BY as_of, id",
    )
    .bind(customer.0)
    .fetch_all(&mut *conn)
    .await?;
    let entitlements = compute(conn, project, customer, &purchases, now).await?;
    Ok(serde_json::json!({
        "customer_id": customer.0,
        "created_at": created_at,
        "aliases": aliases(conn, customer).await?,
        "account_tokens": tokens.into_iter().map(|(t,)| t).collect::<Vec<_>>(),
        "entitlements": entitlements,
        "purchases": purchases,
        "purchase_history": history,
        "promotional_grants": grants,
        "webhook_deliveries": deliveries,
        "exported_at": now,
    }))
}
