//! Loads the inputs of [`entitle_core::metrics::compute`] for one project.
//!
//! v1 loads the project's whole production purchase history into memory on every
//! request. Fine for thousands of subscribers; beyond that this needs snapshots
//! (e.g. materialized daily state) instead of replaying from the start.

use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use entitle_core::metrics::{
    HistoryRow, Metrics, MetricsInput, RENEWAL_TOLERANCE_DAYS, Range, compute,
};
use entitle_core::{BillingPeriod, Money, ProjectId, Store};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::db::parse_enum;

#[derive(FromRow)]
struct Row {
    id: i64,
    purchase_id: Uuid,
    customer_id: Option<Uuid>,
    store: String,
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
}

impl Row {
    fn into_history(self) -> anyhow::Result<HistoryRow> {
        Ok(HistoryRow {
            id: self.id,
            purchase_id: self.purchase_id,
            customer_id: self.customer_id,
            store: parse_enum(&self.store)?,
            product_id: self.product_id,
            kind: parse_enum(&self.kind)?,
            status: parse_enum(&self.status)?,
            period_type: parse_enum(&self.period_type)?,
            purchased_at: self.purchased_at,
            expires_at: self.expires_at,
            grace_period_expires_at: self.grace_period_expires_at,
            auto_renew: self.auto_renew,
            price: match (self.price_amount_minor, self.price_currency) {
                (Some(amount_minor), Some(currency)) => Some(Money {
                    amount_minor,
                    currency,
                }),
                _ => None,
            },
            as_of: self.as_of,
            events: self
                .events
                .iter()
                .map(|e| parse_enum(e))
                .collect::<anyhow::Result<_>>()?,
            backfilled: self.backfilled,
        })
    }
}

fn midnight(d: NaiveDate) -> DateTime<Utc> {
    d.and_time(NaiveTime::MIN).and_utc()
}

pub async fn load_and_compute(
    db: &PgPool,
    project: ProjectId,
    currency: &str,
    range: &Range,
) -> anyhow::Result<Metrics> {
    let (from, to) = (range.from(), range.to());
    // Renewals recorded up to the tolerance after `to` decide whether a lapse
    // before `to` was churn.
    let rows_until = midnight(to) + Duration::days(RENEWAL_TOLERANCE_DAYS);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, purchase_id, customer_id, store, product_id, kind, status, period_type,
            purchased_at, expires_at, grace_period_expires_at, auto_renew, price_amount_minor,
            price_currency, as_of, events, backfilled
         FROM purchase_history
         WHERE project_id = $1 AND environment = 'production' AND as_of < $2",
    )
    .bind(project.0)
    .bind(rows_until)
    .fetch_all(db)
    .await?;
    let rows = rows
        .into_iter()
        .map(Row::into_history)
        .collect::<anyhow::Result<Vec<_>>>()?;

    let configured: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT store, store_product_id, billing_period FROM products
         WHERE project_id = $1 AND billing_period IS NOT NULL",
    )
    .bind(project.0)
    .fetch_all(db)
    .await?;
    let mut periods = HashMap::new();
    for (store, product, period) in configured {
        let store: Store = parse_enum(&store)?;
        let period = BillingPeriod::parse(&period)
            .ok_or_else(|| anyhow::anyhow!("invalid billing period {period}"))?;
        periods.insert((store, product), period);
    }

    let customers_created: Vec<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT created_at FROM customers
         WHERE project_id = $1 AND created_at >= $2 AND created_at < $3",
    )
    .bind(project.0)
    .bind(midnight(from))
    .bind(midnight(to))
    .fetch_all(db)
    .await?;

    let history_start: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT min(recorded_at) FROM purchase_history WHERE project_id = $1")
            .bind(project.0)
            .fetch_one(db)
            .await?;

    let fx = crate::fx::load(db).await?;
    Ok(compute(
        &MetricsInput {
            rows: &rows,
            periods: &periods,
            fx: &fx,
            customers_created: &customers_created,
            currency,
            history_start,
            now: Utc::now(),
        },
        range,
    ))
}
