//! Persistence of normalized purchase snapshots: idempotent, order-safe upserts
//! plus ownership changes and outgoing events.

use chrono::{DateTime, SubsecRound, Utc};
use entitle_core::identity::{Claim, Ownership, resolve_owner};
use entitle_core::{
    CustomerId, EntitlementMapping, EventType, Money, Product, ProjectId, PurchaseSnapshot, Store,
    diff_events,
};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::db::{enum_str, parse_enum};
use crate::outbox;

pub const PURCHASE_COLUMNS: &str = "id, customer_id, store, environment, original_transaction_id, \
    store_transaction_id, product_id, kind, status, period_type, purchased_at, expires_at, \
    grace_period_expires_at, auto_renew, price_amount_minor, price_currency, customer_ref, \
    store_customer_id, as_of";

#[derive(Debug, FromRow)]
pub struct PurchaseRow {
    pub id: Uuid,
    pub customer_id: Option<Uuid>,
    pub store: String,
    pub environment: String,
    pub original_transaction_id: String,
    pub store_transaction_id: String,
    pub product_id: String,
    pub kind: String,
    pub status: String,
    pub period_type: String,
    pub purchased_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub grace_period_expires_at: Option<DateTime<Utc>>,
    pub auto_renew: bool,
    pub price_amount_minor: Option<i64>,
    pub price_currency: Option<String>,
    pub customer_ref: Option<Uuid>,
    pub store_customer_id: Option<String>,
    pub as_of: DateTime<Utc>,
}

impl PurchaseRow {
    pub fn snapshot(&self) -> anyhow::Result<PurchaseSnapshot> {
        Ok(PurchaseSnapshot {
            store: parse_enum(&self.store)?,
            environment: parse_enum(&self.environment)?,
            original_transaction_id: self.original_transaction_id.clone(),
            store_transaction_id: self.store_transaction_id.clone(),
            product_id: self.product_id.clone(),
            kind: parse_enum(&self.kind)?,
            status: parse_enum(&self.status)?,
            period_type: parse_enum(&self.period_type)?,
            purchased_at: self.purchased_at,
            expires_at: self.expires_at,
            grace_period_expires_at: self.grace_period_expires_at,
            auto_renew: self.auto_renew,
            price: match (self.price_amount_minor, &self.price_currency) {
                (Some(amount_minor), Some(currency)) => Some(Money {
                    amount_minor,
                    currency: currency.clone(),
                }),
                _ => None,
            },
            customer_ref: self.customer_ref.map(CustomerId),
            store_customer_id: self.store_customer_id.clone(),
            as_of: self.as_of,
        })
    }
}

pub async fn customer_purchases(
    conn: &mut PgConnection,
    customer: CustomerId,
) -> anyhow::Result<Vec<PurchaseSnapshot>> {
    let rows: Vec<PurchaseRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PURCHASE_COLUMNS} FROM purchases WHERE customer_id = $1 ORDER BY purchased_at DESC"
    )))
    .bind(customer.0)
    .fetch_all(conn)
    .await?;
    rows.iter().map(PurchaseRow::snapshot).collect()
}

pub async fn load_mapping(
    conn: &mut PgConnection,
    project: ProjectId,
) -> anyhow::Result<EntitlementMapping> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT p.store, p.store_product_id, e.identifier
         FROM entitlement_products ep
         JOIN products p ON p.id = ep.product_id
         JOIN entitlements e ON e.id = ep.entitlement_id
         WHERE p.project_id = $1",
    )
    .bind(project.0)
    .fetch_all(conn)
    .await?;
    let mut mapping = EntitlementMapping::new();
    for (store, product_id, identifier) in rows {
        let store: Store = parse_enum(&store)?;
        mapping.attach(Product::new(store, product_id), identifier);
    }
    Ok(mapping)
}

/// A restore of a purchase whose store account token belongs to another customer.
#[derive(Debug, thiserror::Error)]
#[error("this purchase belongs to another customer (its store account token points to them)")]
pub struct TransferRefused;

/// Who is submitting a snapshot.
#[derive(Debug, Clone, Copy)]
pub enum Source {
    /// Store notification: link via the snapshot's `customer_ref` if unowned.
    Notification,
    /// The customer restored / synced it from their device.
    Restore(CustomerId),
}

/// Applies one snapshot inside the caller's transaction:
/// - older snapshots (`as_of` before the stored one) never overwrite state,
/// - ownership follows [`resolve_owner`],
/// - events are derived from the state change and queued for the owner.
/// - every change of stored state or owner is appended to `purchase_history`.
pub async fn apply_snapshot(
    conn: &mut PgConnection,
    project: ProjectId,
    new: &PurchaseSnapshot,
    source: Source,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let new = &to_micros(new);
    // `FOR UPDATE` below cannot lock a row that does not exist yet: serialize the
    // first sighting of a purchase too (concurrent webhooks / restores).
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "purchase:{project}:{}:{}",
            new.store, new.original_transaction_id
        ))
        .execute(&mut *conn)
        .await?;
    let existing: Option<PurchaseRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PURCHASE_COLUMNS} FROM purchases
         WHERE project_id = $1 AND store = $2 AND original_transaction_id = $3
         FOR UPDATE"
    )))
    .bind(project.0)
    .bind(new.store.as_str())
    .bind(&new.original_transaction_id)
    .fetch_optional(&mut *conn)
    .await?;

    let old_owner = existing
        .as_ref()
        .and_then(|r| r.customer_id.map(CustomerId));
    let old_state = existing.as_ref().map(PurchaseRow::snapshot).transpose()?;
    let token_owner = match new.customer_ref {
        Some(token) => resolve_token(conn, project, token).await?,
        None => None,
    };
    let claim = match source {
        Source::Notification => Claim::Notification { token_owner },
        // A token of a purely anonymous customer only proves "same install": after a
        // reinstall or on a new device that id is gone, and the store account restoring
        // the purchase is the real proof of ownership. Only identified tokens refuse.
        Source::Restore(customer) => Claim::Restore {
            customer,
            token_owner: match token_owner {
                Some(owner) if is_identified(conn, owner).await? => Some(owner),
                _ => None,
            },
        },
    };
    let ownership = resolve_owner(old_owner, claim);
    let new_owner = match ownership {
        Ownership::Refuse => return Err(TransferRefused.into()),
        Ownership::Keep => old_owner,
        Ownership::Assign(c) => Some(c),
        Ownership::Transfer { to, .. } => Some(to),
    };
    let stale = old_state.as_ref().is_some_and(|old| old.as_of > new.as_of);
    if stale && ownership == Ownership::Keep {
        // Log the row id, not the transaction id: for Google that is the purchase token.
        tracing::debug!(
            store = %new.store,
            purchase_id = ?existing.as_ref().map(|r| r.id),
            "ignoring stale snapshot"
        );
        return Ok(());
    }

    let current = match (&old_state, stale) {
        (Some(old), true) => old.clone(),
        _ => new.clone(),
    };
    let purchase_id = existing.as_ref().map_or_else(Uuid::new_v4, |row| row.id);
    upsert(conn, project, purchase_id, &current, new_owner, now).await?;

    // History records every transition, linked or not, diffed against the stored
    // state (unlike the webhook events below).
    let transition = diff_events(old_state.as_ref(), &current);
    let state_changed = match &old_state {
        None => true,
        Some(old) => !stale && !same_state(old, &current),
    };
    if state_changed || new_owner != old_owner {
        append_history(
            conn,
            project,
            purchase_id,
            &current,
            new_owner,
            &transition,
            now,
        )
        .await?;
    }

    let Some(owner) = new_owner else {
        // Unlinked purchases produce no events; they are announced once linked.
        return Ok(());
    };
    let mut events = match (old_owner, stale) {
        // Newly linked: announce it as if seen for the first time.
        (None, _) => diff_events(None, &current),
        (Some(_), true) => Vec::new(),
        (Some(_), false) => transition,
    };
    let mut transferred_from = None;
    if let Ownership::Transfer { from, .. } = ownership {
        events.push(EventType::Transfer);
        transferred_from = Some(from);
    }
    for event in events {
        let from = transferred_from.filter(|_| event == EventType::Transfer);
        outbox::enqueue(conn, project, event, owner, &current, from, now).await?;
    }
    Ok(())
}

/// Postgres `timestamptz` keeps microseconds: compare and store what will be read back,
/// or a nanosecond timestamp (Linux `Utc::now()`) looks later than its stored copy.
fn to_micros(s: &PurchaseSnapshot) -> PurchaseSnapshot {
    let t = |d: DateTime<Utc>| d.trunc_subsecs(6);
    PurchaseSnapshot {
        purchased_at: t(s.purchased_at),
        expires_at: s.expires_at.map(t),
        grace_period_expires_at: s.grace_period_expires_at.map(t),
        as_of: t(s.as_of),
        ..s.clone()
    }
}

/// Equal in every column `purchase_history` records, ignoring `as_of`.
fn same_state(a: &PurchaseSnapshot, b: &PurchaseSnapshot) -> bool {
    let norm = |s: &PurchaseSnapshot| PurchaseSnapshot {
        customer_ref: None,
        store_customer_id: None,
        as_of: DateTime::UNIX_EPOCH,
        ..s.clone()
    };
    norm(a) == norm(b)
}

async fn append_history(
    conn: &mut PgConnection,
    project: ProjectId,
    purchase_id: Uuid,
    s: &PurchaseSnapshot,
    owner: Option<CustomerId>,
    events: &[EventType],
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO purchase_history (purchase_id, project_id, customer_id, store, environment,
            store_transaction_id, product_id, kind, status, period_type, purchased_at, expires_at,
            grace_period_expires_at, auto_renew, price_amount_minor, price_currency, as_of,
            events, recorded_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
            $18, $19)",
    )
    .bind(purchase_id)
    .bind(project.0)
    .bind(owner.map(|c| c.0))
    .bind(s.store.as_str())
    .bind(s.environment.as_str())
    .bind(&s.store_transaction_id)
    .bind(&s.product_id)
    .bind(enum_str(&s.kind))
    .bind(s.status.as_str())
    .bind(enum_str(&s.period_type))
    .bind(s.purchased_at)
    .bind(s.expires_at)
    .bind(s.grace_period_expires_at)
    .bind(s.auto_renew)
    .bind(s.price.as_ref().map(|p| p.amount_minor))
    .bind(s.price.as_ref().map(|p| p.currency.clone()))
    .bind(s.as_of)
    .bind(events.iter().map(|e| e.as_str()).collect::<Vec<_>>())
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

async fn upsert(
    conn: &mut PgConnection,
    project: ProjectId,
    id: Uuid,
    s: &PurchaseSnapshot,
    owner: Option<CustomerId>,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO purchases (id, project_id, customer_id, store, environment,
            original_transaction_id, store_transaction_id, product_id, kind, status, period_type,
            purchased_at, expires_at, grace_period_expires_at, auto_renew, price_amount_minor,
            price_currency, customer_ref, store_customer_id, as_of, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
            $18, $19, $20, $21, $21)
         ON CONFLICT (id) DO UPDATE SET
            customer_id = EXCLUDED.customer_id,
            environment = EXCLUDED.environment,
            store_transaction_id = EXCLUDED.store_transaction_id,
            product_id = EXCLUDED.product_id,
            kind = EXCLUDED.kind,
            status = EXCLUDED.status,
            period_type = EXCLUDED.period_type,
            purchased_at = EXCLUDED.purchased_at,
            expires_at = EXCLUDED.expires_at,
            grace_period_expires_at = EXCLUDED.grace_period_expires_at,
            auto_renew = EXCLUDED.auto_renew,
            price_amount_minor = EXCLUDED.price_amount_minor,
            price_currency = EXCLUDED.price_currency,
            customer_ref = COALESCE(EXCLUDED.customer_ref, purchases.customer_ref),
            store_customer_id = COALESCE(EXCLUDED.store_customer_id, purchases.store_customer_id),
            as_of = EXCLUDED.as_of,
            updated_at = EXCLUDED.updated_at",
    )
    .bind(id)
    .bind(project.0)
    .bind(owner.map(|c| c.0))
    .bind(s.store.as_str())
    .bind(s.environment.as_str())
    .bind(&s.original_transaction_id)
    .bind(&s.store_transaction_id)
    .bind(&s.product_id)
    .bind(enum_str(&s.kind))
    .bind(s.status.as_str())
    .bind(enum_str(&s.period_type))
    .bind(s.purchased_at)
    .bind(s.expires_at)
    .bind(s.grace_period_expires_at)
    .bind(s.auto_renew)
    .bind(s.price.as_ref().map(|p| p.amount_minor))
    .bind(s.price.as_ref().map(|p| p.currency.clone()))
    .bind(s.customer_ref.map(|c| c.0))
    .bind(&s.store_customer_id)
    .bind(s.as_of)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// Customer an account token belongs to, within this project.
async fn resolve_token(
    conn: &mut PgConnection,
    project: ProjectId,
    token: CustomerId,
) -> anyhow::Result<Option<CustomerId>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT t.customer_id FROM account_tokens t
         JOIN customers c ON c.id = t.customer_id
         WHERE t.token = $1 AND c.project_id = $2",
    )
    .bind(token.0)
    .bind(project.0)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(id,)| CustomerId(id)))
}

/// Whether the customer has an app user id that is not SDK-generated anonymous.
async fn is_identified(conn: &mut PgConnection, customer: CustomerId) -> anyhow::Result<bool> {
    let (identified,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM customer_aliases
         WHERE customer_id = $1 AND NOT starts_with(app_user_id, $2))",
    )
    .bind(customer.0)
    .bind(entitle_core::identity::ANONYMOUS_PREFIX)
    .fetch_one(conn)
    .await?;
    Ok(identified)
}
