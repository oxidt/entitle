//! Domain types shared by the server and every store adapter.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// One app (or family of apps) served by this instance.
    ProjectId
);
id_type!(
    /// A customer inside a project. Also the stable account token handed to the
    /// stores: Apple `appAccountToken`, Google `obfuscatedExternalAccountId`,
    /// Creem checkout metadata `entitle_customer_id`.
    CustomerId
);

/// Where a purchase was made.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Store {
    Creem,
    AppStore,
    PlayStore,
}

impl Store {
    pub const ALL: [Store; 3] = [Store::Creem, Store::AppStore, Store::PlayStore];

    /// Stable string used in URLs, JSON and the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Store::Creem => "creem",
            Store::AppStore => "app_store",
            Store::PlayStore => "play_store",
        }
    }

    /// Path segment used by inbound webhook URLs: `/webhooks/{creem|apple|google}/…`.
    pub fn webhook_slug(self) -> &'static str {
        match self {
            Store::Creem => "creem",
            Store::AppStore => "apple",
            Store::PlayStore => "google",
        }
    }

    pub fn from_webhook_slug(s: &str) -> Option<Store> {
        Store::ALL.into_iter().find(|st| st.webhook_slug() == s)
    }

    pub fn parse(s: &str) -> Option<Store> {
        Store::ALL.into_iter().find(|st| st.as_str() == s)
    }
}

impl fmt::Display for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    Sandbox,
    Production,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Environment::Sandbox => "sandbox",
            Environment::Production => "production",
        }
    }

    pub fn parse(s: &str) -> Option<Environment> {
        match s {
            "sandbox" => Some(Environment::Sandbox),
            "production" => Some(Environment::Production),
            _ => None,
        }
    }
}

/// Subscription vs. one-time purchase.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurchaseKind {
    /// Auto-renewing subscription. `expires_at` is always set.
    Subscription,
    /// One-time purchase (non-consumable / lifetime). `expires_at` is `None`
    /// and it grants its entitlements forever unless refunded or revoked.
    OneTime,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodType {
    Trial,
    Intro,
    Normal,
}

/// Normalized lifecycle state of a purchase, as last reported by the store.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurchaseStatus {
    /// Paid and renewing (or a completed one-time purchase).
    Active,
    /// Auto-renew turned off / cancellation scheduled; access continues until `expires_at`.
    Cancelled,
    /// Renewal payment failed, store still grants access until `grace_period_expires_at`.
    GracePeriod,
    /// Renewal payment failed, store retries billing; no access.
    BillingRetry,
    /// Paused by the user (Google Play, Creem); no access.
    Paused,
    /// Ended normally.
    Expired,
    /// Money returned to the customer; access revoked.
    Refunded,
    /// Access revoked without a refund (e.g. Apple family sharing removed, Google voided purchase, chargeback).
    Revoked,
}

impl PurchaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PurchaseStatus::Active => "active",
            PurchaseStatus::Cancelled => "cancelled",
            PurchaseStatus::GracePeriod => "grace_period",
            PurchaseStatus::BillingRetry => "billing_retry",
            PurchaseStatus::Paused => "paused",
            PurchaseStatus::Expired => "expired",
            PurchaseStatus::Refunded => "refunded",
            PurchaseStatus::Revoked => "revoked",
        }
    }

    pub fn is_revoked(self) -> bool {
        matches!(self, PurchaseStatus::Refunded | PurchaseStatus::Revoked)
    }
}

/// A price in the currency's ISO 4217 minor units (cents for EUR, yen for JPY,
/// fils for KWD) plus ISO 4217 currency code. See [`crate::currency`].
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub amount_minor: i64,
    pub currency: String,
}

/// The normalized state of one purchase (one subscription or one one-time
/// order) at a point in time. Adapters produce these; the server persists them.
///
/// Identity of a purchase is `(store, original_transaction_id)`:
/// - App Store: `originalTransactionId`
/// - Play Store: the purchase token
/// - Creem: subscription id (`sub_…`) for subscriptions, order id (`ord_…`) for one-time
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurchaseSnapshot {
    pub store: Store,
    pub environment: Environment,
    pub original_transaction_id: String,
    /// Latest transaction: Apple `transactionId`, Google `latestOrderId`, Creem latest order id
    /// (falls back to the original id when the store has no per-renewal id).
    pub store_transaction_id: String,
    /// The store's product id (`com.app.pro.monthly`, `prod_…`).
    pub product_id: String,
    pub kind: PurchaseKind,
    pub status: PurchaseStatus,
    pub period_type: PeriodType,
    /// When the current period / transaction was purchased.
    pub purchased_at: DateTime<Utc>,
    /// End of the current paid period. `None` for one-time purchases.
    pub expires_at: Option<DateTime<Utc>>,
    /// Only meaningful with [`PurchaseStatus::GracePeriod`].
    pub grace_period_expires_at: Option<DateTime<Utc>>,
    pub auto_renew: bool,
    pub price: Option<Money>,
    /// The customer's account token as echoed back by the store
    /// (`appAccountToken`, `obfuscatedExternalAccountId`, Creem metadata), if any.
    pub customer_ref: Option<CustomerId>,
    /// The store's own customer id, needed for Creem portal links (`cust_…`).
    pub store_customer_id: Option<String>,
    /// Store-side timestamp this snapshot describes (notification `signedDate`,
    /// `eventTimeMillis`, Creem `created_at`, or the store API response time).
    /// Used to reject out-of-order updates: an older `as_of` never overwrites a newer one.
    pub as_of: DateTime<Utc>,
}

/// A product configured in a project: a store product id that may unlock entitlements.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Product {
    pub store: Store,
    pub store_product_id: String,
}

impl Product {
    pub fn new(store: Store, store_product_id: impl Into<String>) -> Self {
        Product {
            store,
            store_product_id: store_product_id.into(),
        }
    }
}
