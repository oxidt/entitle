//! Builders for tests (enabled in this crate's tests and via the `testing` feature).

use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::model::{
    Environment, PeriodType, PurchaseKind, PurchaseSnapshot, PurchaseStatus, Store,
};

/// A fixed reference time plus `days` days.
pub fn at(days: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + Duration::days(days)
}

/// An active, auto-renewing production subscription with a deterministic
/// original transaction id derived from store and product.
pub fn sub(
    store: Store,
    product_id: &str,
    purchased_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
) -> PurchaseSnapshot {
    PurchaseSnapshot {
        store,
        environment: Environment::Production,
        original_transaction_id: format!("{store}-{product_id}"),
        store_transaction_id: format!("{store}-{product_id}-1"),
        product_id: product_id.into(),
        kind: PurchaseKind::Subscription,
        status: PurchaseStatus::Active,
        period_type: PeriodType::Normal,
        purchased_at,
        expires_at,
        grace_period_expires_at: None,
        auto_renew: true,
        price: None,
        customer_ref: None,
        store_customer_id: None,
        as_of: purchased_at,
    }
}
