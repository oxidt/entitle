//! Pure domain logic of entitle: no HTTP, no database.
//!
//! - [`model`]: ids, stores, normalized purchase snapshots.
//! - [`entitlements`]: which entitlements a customer has right now.
//! - [`identity`]: anonymous ids, login/merge rules, purchase ownership.
//! - [`events`]: lifecycle events derived from state changes.
//! - [`adapter`]: the contract store adapter crates implement.
//! - [`currency`]: ISO 4217 minor units for store price conversions.
//! - [`billing`]: billing periods and monthly (MRR) normalization of prices.
//! - [`fx`]: ECB reference rates and currency conversion.
//! - [`metrics`]: revenue metrics (MRR, movement, revenue, churn) from purchase history.

pub mod adapter;
pub mod billing;
pub mod currency;
pub mod entitlements;
pub mod events;
pub mod fx;
pub mod identity;
pub mod metrics;
pub mod model;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use adapter::{
    AdapterError, AppleCredentials, CheckoutAdapter, CheckoutRequest, CheckoutSession,
    CreemCredentials, GoogleCredentials, InboundNotification, ParsedNotification, StoreAdapter,
};
pub use billing::{BillingPeriod, billing_period, monthly_amount};
pub use entitlements::{
    ActiveEntitlement, EntitlementMapping, EntitlementSource, PromotionalGrant,
    compute_entitlements,
};
pub use events::{EventType, diff_events};
pub use fx::{DailyRates, EcbParseError, FxRates, PerEur, parse_ecb_xml};
pub use model::{
    CustomerId, Environment, Money, PeriodType, Product, ProjectId, PurchaseKind, PurchaseSnapshot,
    PurchaseStatus, Store,
};
