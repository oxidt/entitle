//! Creem (creem.io, Merchant of Record) adapter: webhooks with `creem-signature` HMAC, subscription/checkout lookups, hosted checkout and customer portal.
//!
//! Wraps the `oxidt-creem` client. The contract is described in `docs/adapters.md`.

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use entitle_core::adapter::CREEM_CUSTOMER_METADATA_KEY;
use entitle_core::{
    AdapterError, CheckoutAdapter, CheckoutRequest, CheckoutSession, CreemCredentials, CustomerId,
    Environment, InboundNotification, Money, ParsedNotification, PeriodType, PurchaseKind,
    PurchaseSnapshot, PurchaseStatus, Store, StoreAdapter,
};
use oxidt_creem::{Client, Event, EventType, Expandable};
use secrecy::SecretString;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

type Metadata = Map<String, Value>;

const SIGNATURE_HEADER: &str = "creem-signature";
/// Access kept after the period end while a renewal payment is `past_due`
/// (Creem retries the charge; it reports no grace period end of its own).
pub const PAST_DUE_GRACE_PERIOD: TimeDelta = TimeDelta::days(7);

#[derive(Debug, Clone, Default)]
pub struct CreemAdapter {
    /// Overrides the Creem API base (including `/v1`) for both modes.
    base_url: Option<String>,
}

impl CreemAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Talk to another API host (base including `/v1`) instead of Creem. For tests.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        CreemAdapter {
            base_url: Some(base_url.into()),
        }
    }

    fn client(&self, credentials: &CreemCredentials) -> Result<Client, AdapterError> {
        let api_key = SecretString::from(credentials.api_key.clone());
        match &self.base_url {
            Some(base_url) => Client::with_base_url(api_key, base_url),
            None => Client::new(api_key, credentials.test_mode),
        }
        .map_err(map_error)
    }

    /// Webhook subscriptions can arrive as a bare id or without period dates
    /// (`subscription.active`, `checkout.completed`): complete them from the API.
    async fn resolve(
        &self,
        credentials: &CreemCredentials,
        subscription: Expandable<Subscription>,
    ) -> Result<Subscription, AdapterError> {
        let id = match subscription {
            Expandable::Object(s) if s.current_period_end_date.is_some() => return Ok(s),
            Expandable::Object(ref s) => s.id.clone(),
            Expandable::Id(ref id) => id.clone(),
        };
        let fetched: Subscription = self
            .client(credentials)?
            .subscription(&id)
            .await
            .map_err(map_error)?
            .into();
        Ok(match subscription {
            Expandable::Object(mut s) => {
                s.current_period_start_date = fetched.current_period_start_date;
                s.current_period_end_date = fetched.current_period_end_date;
                s.metadata = s.metadata.or(fetched.metadata);
                s
            }
            Expandable::Id(_) => fetched,
        })
    }
}

#[async_trait]
impl StoreAdapter for CreemAdapter {
    type Credentials = CreemCredentials;

    async fn parse_notification(
        &self,
        credentials: &CreemCredentials,
        notification: &InboundNotification,
    ) -> Result<ParsedNotification, AdapterError> {
        let signature = notification.header(SIGNATURE_HEADER).ok_or_else(|| {
            AdapterError::InvalidSignature("missing creem-signature header".into())
        })?;
        let event =
            oxidt_creem::verify_webhook(&credentials.webhook_secret, signature, &notification.body)
                .map_err(map_error)?;
        let as_of = event
            .created_at
            .and_then(DateTime::from_timestamp_millis)
            .ok_or_else(|| AdapterError::Malformed("event has no created_at".into()))?;
        let environment = environment(credentials);
        let snapshots = match purchase(&event)? {
            Some(Purchase::Subscription {
                subscription,
                metadata,
                status,
            }) => {
                let subscription = self.resolve(credentials, subscription).await?;
                vec![subscription_snapshot(
                    &subscription,
                    status,
                    metadata.as_ref(),
                    environment,
                    as_of,
                )?]
            }
            Some(Purchase::Order {
                order,
                customer,
                metadata,
                status,
            }) => order_snapshot(
                &order,
                customer.as_deref(),
                metadata.as_ref(),
                status,
                environment,
                as_of,
            )
            .into_iter()
            .collect(),
            None => Vec::new(),
        };
        Ok(ParsedNotification {
            event_id: event.id,
            snapshots,
        })
    }

    async fn fetch_purchases(
        &self,
        credentials: &CreemCredentials,
        id: &str,
    ) -> Result<Vec<PurchaseSnapshot>, AdapterError> {
        let environment = environment(credentials);
        let as_of = Utc::now();
        if id.starts_with("sub_") {
            let subscription = self
                .client(credentials)?
                .subscription(id)
                .await
                .map_err(map_error)?;
            let snapshot =
                subscription_snapshot(&subscription.into(), None, None, environment, as_of)?;
            return Ok(vec![snapshot]);
        }
        if !id.starts_with("ch_") {
            return Err(AdapterError::NotFound);
        }
        let checkout = self
            .client(credentials)?
            .checkout_session(id)
            .await
            .map_err(map_error)?;
        if checkout.status.as_deref() != Some("completed") {
            return Ok(Vec::new());
        }
        let Some(subscription) = checkout.subscription else {
            // A one-time purchase: its order, as `checkout.completed` carries it.
            let Some(order) = checkout.order else {
                return Ok(Vec::new());
            };
            let mut order = Order::from(order);
            if order.currency.is_none() {
                order.currency = checkout
                    .product
                    .as_ref()
                    .and_then(Expandable::object)
                    .and_then(|p| p.currency.clone());
            }
            let customer = checkout.customer.map(api_customer);
            return Ok(order_snapshot(
                &order,
                customer.as_ref().map(customer_id),
                checkout.metadata.as_ref(),
                None,
                environment,
                as_of,
            )
            .into_iter()
            .collect());
        };
        let subscription = self
            .resolve(credentials, expandable_subscription(subscription))
            .await?;
        let snapshot = subscription_snapshot(
            &subscription,
            None,
            checkout.metadata.as_ref(),
            environment,
            as_of,
        )?;
        Ok(vec![snapshot])
    }

    async fn check_credentials(
        &self,
        credentials: &CreemCredentials,
    ) -> Result<String, AdapterError> {
        // Creem has no "who am I" call: an authorized lookup of a missing product is a 404.
        match self
            .client(credentials)?
            .product("prod_entitle_check")
            .await
        {
            Ok(_) | Err(oxidt_creem::Error::Api(404)) => Ok(format!(
                "Creem accepted the API key ({} mode).",
                if credentials.test_mode {
                    "test"
                } else {
                    "live"
                }
            )),
            Err(e) => Err(map_error(e)),
        }
    }
}

#[async_trait]
impl CheckoutAdapter for CreemAdapter {
    async fn create_checkout(
        &self,
        credentials: &CreemCredentials,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, AdapterError> {
        let customer = request.customer.to_string();
        let session = self
            .client(credentials)?
            .checkout(oxidt_creem::Checkout {
                product_id: &request.product_id,
                request_id: Some(&customer),
                success_url: request.success_url.as_deref(),
                metadata: Some(checkout_metadata(request)),
                ..Default::default()
            })
            .await
            .map_err(map_error)?;
        Ok(CheckoutSession {
            id: session.id,
            url: session.checkout_url,
        })
    }

    async fn portal_url(
        &self,
        credentials: &CreemCredentials,
        store_customer_id: &str,
    ) -> Result<String, AdapterError> {
        self.client(credentials)?
            .billing_portal(store_customer_id)
            .await
            .map_err(map_error)
    }
}

fn environment(credentials: &CreemCredentials) -> Environment {
    if credentials.test_mode {
        Environment::Sandbox
    } else {
        Environment::Production
    }
}

fn map_error(error: oxidt_creem::Error) -> AdapterError {
    use oxidt_creem::Error as E;
    match error {
        E::Signature => AdapterError::InvalidSignature(error.to_string()),
        E::Event | E::Identifier => AdapterError::Malformed(error.to_string()),
        E::Api(404) => AdapterError::NotFound,
        E::Configuration | E::Api(401 | 403) => AdapterError::Credentials(error.to_string()),
        E::Transport(_) | E::Api(_) | E::Response => AdapterError::Upstream(error.to_string()),
    }
}

/// Developer metadata plus the customer id, which always wins over a developer key of the same name.
fn checkout_metadata(request: &CheckoutRequest) -> Metadata {
    let mut metadata: Metadata = request
        .metadata
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    metadata.insert(
        CREEM_CUSTOMER_METADATA_KEY.into(),
        request.customer.to_string().into(),
    );
    metadata
}

// Creem objects as webhooks carry them, with parsed dates. Refunds and disputes
// have no oxidt-creem type; API responses are converted with the `From` impls below.

#[derive(Debug, Deserialize)]
struct Subscription {
    id: String,
    status: String,
    product: Expandable<Product>,
    customer: Expandable<Customer>,
    #[serde(default)]
    last_transaction_id: Option<String>,
    #[serde(default)]
    current_period_start_date: Option<DateTime<Utc>>,
    #[serde(default)]
    current_period_end_date: Option<DateTime<Utc>>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    metadata: Option<Metadata>,
}

/// `price` is in the currency's minor units.
#[derive(Debug, Deserialize)]
struct Product {
    id: String,
    #[serde(default)]
    price: Option<i64>,
    #[serde(default)]
    currency: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Customer {
    id: String,
}

#[derive(Debug, Deserialize)]
struct Order {
    id: String,
    product: Expandable<Product>,
    #[serde(default)]
    customer: Option<Expandable<Customer>>,
    #[serde(default)]
    amount: Option<i64>,
    #[serde(default)]
    currency: Option<String>,
    /// `pending`, `paid`, …
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
}

/// The `checkout.completed` object; also nested in refunds and disputes.
#[derive(Debug, Deserialize)]
struct Checkout {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    order: Option<Order>,
    #[serde(default)]
    subscription: Option<Expandable<Subscription>>,
    #[serde(default)]
    customer: Option<Expandable<Customer>>,
    #[serde(default)]
    metadata: Option<Metadata>,
}

/// The charge a refund refers to.
#[derive(Debug, Deserialize)]
struct Transaction {
    /// `refunded`, `partialRefund`, …
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    amount_paid: Option<i64>,
    #[serde(default)]
    refunded_amount: Option<i64>,
}

impl Transaction {
    fn partially_refunded(&self) -> bool {
        self.status.as_deref() == Some("partialRefund")
            || matches!(
                (self.refunded_amount, self.amount_paid),
                (Some(refunded), Some(paid)) if refunded < paid
            )
    }
}

/// The `refund.created` and `dispute.created` object.
#[derive(Debug, Deserialize)]
struct Charge {
    #[serde(default)]
    transaction: Option<Transaction>,
    #[serde(default)]
    subscription: Option<Expandable<Subscription>>,
    #[serde(default)]
    order: Option<Order>,
    #[serde(default)]
    checkout: Option<Checkout>,
    #[serde(default)]
    customer: Option<Expandable<Customer>>,
}

fn date(d: Option<String>) -> Option<DateTime<Utc>> {
    d.and_then(|d| DateTime::parse_from_rfc3339(&d).ok())
        .map(|d| d.with_timezone(&Utc))
}

fn api_product(p: Expandable<oxidt_creem::Product>) -> Expandable<Product> {
    match p {
        Expandable::Id(id) => Expandable::Id(id),
        Expandable::Object(p) => Expandable::Object(Product {
            id: p.id,
            price: p.price,
            currency: p.currency,
        }),
    }
}

fn api_customer(c: Expandable<oxidt_creem::Customer>) -> Expandable<Customer> {
    match c {
        Expandable::Id(id) => Expandable::Id(id),
        Expandable::Object(c) => Expandable::Id(c.id),
    }
}

impl From<oxidt_creem::Subscription> for Subscription {
    fn from(s: oxidt_creem::Subscription) -> Self {
        Subscription {
            id: s.id,
            status: s.status,
            product: api_product(s.product),
            customer: api_customer(s.customer),
            last_transaction_id: s.last_transaction_id,
            current_period_start_date: date(s.current_period_start_date),
            current_period_end_date: date(s.current_period_end_date),
            created_at: date(s.created_at),
            metadata: s.metadata,
        }
    }
}

impl From<oxidt_creem::Order> for Order {
    fn from(o: oxidt_creem::Order) -> Self {
        Order {
            id: o.id,
            product: api_product(o.product),
            customer: o.customer.map(api_customer),
            amount: o.amount,
            currency: o.currency,
            status: o.status,
            created_at: date(o.created_at),
        }
    }
}

fn expandable_subscription(s: Expandable<oxidt_creem::Subscription>) -> Expandable<Subscription> {
    match s {
        Expandable::Id(id) => Expandable::Id(id),
        Expandable::Object(s) => Expandable::Object(s.into()),
    }
}

fn product_id(p: &Expandable<Product>) -> &str {
    match p {
        Expandable::Id(id) => id,
        Expandable::Object(p) => &p.id,
    }
}

fn customer_id(c: &Expandable<Customer>) -> &str {
    match c {
        Expandable::Id(id) => id,
        Expandable::Object(c) => &c.id,
    }
}

/// What an event is about. `status` overrides the state the store object reports.
enum Purchase {
    Subscription {
        subscription: Expandable<Subscription>,
        /// Checkout metadata, for when the subscription carries none.
        metadata: Option<Metadata>,
        status: Option<PurchaseStatus>,
    },
    Order {
        order: Order,
        customer: Option<String>,
        metadata: Option<Metadata>,
        status: Option<PurchaseStatus>,
    },
}

fn object<T: DeserializeOwned>(event: &Event) -> Result<T, AdapterError> {
    serde_json::from_value(event.object.clone())
        .map_err(|e| AdapterError::Malformed(format!("{} object: {e}", event.event_type.as_str())))
}

fn purchase(event: &Event) -> Result<Option<Purchase>, AdapterError> {
    use EventType as T;
    let status = match event.event_type {
        // Creem sends the subscription with its last status (often still `active`) on expiry.
        T::SubscriptionExpired => Some(PurchaseStatus::Expired),
        T::RefundCreated => Some(PurchaseStatus::Refunded),
        T::DisputeCreated => Some(PurchaseStatus::Revoked),
        _ => None,
    };
    match event.event_type {
        T::CheckoutCompleted => {
            let checkout: Checkout = object(event)?;
            if checkout.status.as_deref().is_some_and(|s| s != "completed") {
                return Ok(None);
            }
            if let Some(subscription) = checkout.subscription {
                return Ok(Some(Purchase::Subscription {
                    subscription,
                    metadata: checkout.metadata,
                    status,
                }));
            }
            Ok(checkout.order.map(|order| Purchase::Order {
                order,
                customer: checkout
                    .customer
                    .as_ref()
                    .map(|c| customer_id(c).to_owned()),
                metadata: checkout.metadata,
                status,
            }))
        }
        T::SubscriptionActive
        | T::SubscriptionPaid
        | T::SubscriptionCanceled
        | T::SubscriptionScheduledCancel
        | T::SubscriptionPastDue
        | T::SubscriptionUnpaid
        | T::SubscriptionExpired
        | T::SubscriptionUpdate
        | T::SubscriptionTrialing
        | T::SubscriptionPaused => Ok(Some(Purchase::Subscription {
            subscription: Expandable::Object(object(event)?),
            metadata: None,
            status,
        })),
        T::RefundCreated | T::DisputeCreated => {
            let charge: Charge = object(event)?;
            // A partial refund leaves the purchase (and access) as it is.
            if event.event_type == T::RefundCreated
                && charge
                    .transaction
                    .as_ref()
                    .is_some_and(Transaction::partially_refunded)
            {
                return Ok(None);
            }
            let metadata = charge.checkout.and_then(|c| c.metadata);
            if let Some(subscription) = charge.subscription {
                return Ok(Some(Purchase::Subscription {
                    subscription,
                    metadata,
                    status,
                }));
            }
            let order = charge.order.ok_or_else(|| {
                AdapterError::Malformed("refund or dispute without subscription or order".into())
            })?;
            Ok(Some(Purchase::Order {
                order,
                customer: charge.customer.as_ref().map(|c| customer_id(c).to_owned()),
                metadata,
                status,
            }))
        }
        T::Other(_) => Ok(None),
    }
}

/// Creem subscription status → (status, period type, auto-renew).
fn subscription_status(status: &str) -> Option<(PurchaseStatus, PeriodType, bool)> {
    use PurchaseStatus as S;
    Some(match status {
        "active" => (S::Active, PeriodType::Normal, true),
        "trialing" => (S::Active, PeriodType::Trial, true),
        "scheduled_cancel" => (S::Cancelled, PeriodType::Normal, false),
        "past_due" => (S::GracePeriod, PeriodType::Normal, true),
        "unpaid" => (S::BillingRetry, PeriodType::Normal, true),
        "paused" => (S::Paused, PeriodType::Normal, true),
        "expired" | "canceled" => (S::Expired, PeriodType::Normal, false),
        _ => return None,
    })
}

fn customer_ref(metadata: Option<&Metadata>) -> Option<CustomerId> {
    metadata?
        .get(CREEM_CUSTOMER_METADATA_KEY)?
        .as_str()?
        .parse()
        .ok()
        .map(CustomerId)
}

fn money(amount: Option<i64>, currency: Option<&str>) -> Option<Money> {
    Some(Money {
        amount_minor: amount?,
        currency: currency?.to_ascii_uppercase(),
    })
}

fn subscription_snapshot(
    subscription: &Subscription,
    status: Option<PurchaseStatus>,
    metadata: Option<&Metadata>,
    environment: Environment,
    as_of: DateTime<Utc>,
) -> Result<PurchaseSnapshot, AdapterError> {
    let s = subscription;
    let mapped = subscription_status(&s.status);
    let (status, period_type, auto_renew) = match (status, mapped) {
        (Some(status), mapped) => (status, mapped.map_or(PeriodType::Normal, |m| m.1), false),
        (None, Some(mapped)) => mapped,
        (None, None) => {
            return Err(AdapterError::Malformed(format!(
                "unknown subscription status {:?}",
                s.status
            )));
        }
    };
    let expires_at = s.current_period_end_date.ok_or_else(|| {
        AdapterError::Malformed(format!("subscription {} has no period end", s.id))
    })?;
    let price = s
        .product
        .object()
        .and_then(|p| money(p.price, p.currency.as_deref()));
    Ok(PurchaseSnapshot {
        store: Store::Creem,
        environment,
        original_transaction_id: s.id.clone(),
        store_transaction_id: s
            .last_transaction_id
            .clone()
            .unwrap_or_else(|| s.id.clone()),
        product_id: product_id(&s.product).to_owned(),
        kind: PurchaseKind::Subscription,
        status,
        period_type,
        purchased_at: s
            .current_period_start_date
            .or(s.created_at)
            .unwrap_or(as_of),
        expires_at: Some(expires_at),
        grace_period_expires_at: (status == PurchaseStatus::GracePeriod)
            .then(|| expires_at + PAST_DUE_GRACE_PERIOD),
        auto_renew,
        price,
        customer_ref: customer_ref(s.metadata.as_ref()).or_else(|| customer_ref(metadata)),
        store_customer_id: Some(customer_id(&s.customer).to_owned()),
        as_of,
    })
}

/// A one-time order. `None` while it is not paid (and nothing overrides its status).
fn order_snapshot(
    order: &Order,
    customer: Option<&str>,
    metadata: Option<&Metadata>,
    status: Option<PurchaseStatus>,
    environment: Environment,
    as_of: DateTime<Utc>,
) -> Option<PurchaseSnapshot> {
    let status = match status {
        Some(status) => status,
        None if order.status.as_deref() == Some("paid") => PurchaseStatus::Active,
        None => return None,
    };
    Some(PurchaseSnapshot {
        store: Store::Creem,
        environment,
        original_transaction_id: order.id.clone(),
        store_transaction_id: order.id.clone(),
        product_id: product_id(&order.product).to_owned(),
        kind: PurchaseKind::OneTime,
        status,
        period_type: PeriodType::Normal,
        purchased_at: order.created_at.unwrap_or(as_of),
        expires_at: None,
        grace_period_expires_at: None,
        auto_renew: false,
        price: money(order.amount, order.currency.as_deref()),
        customer_ref: customer_ref(metadata),
        store_customer_id: order
            .customer
            .as_ref()
            .map(customer_id)
            .or(customer)
            .map(str::to_owned),
        as_of,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::TimeZone;

    use super::*;

    const CUSTOMER: &str = "7f0c3a52-4f0e-4b8e-9c4a-2f1d8a7e6b10";

    fn credentials(api_key: &str) -> CreemCredentials {
        CreemCredentials {
            api_key: api_key.into(),
            webhook_secret: "whsec_test".into(),
            test_mode: true,
        }
    }

    fn customer() -> CustomerId {
        CustomerId(CUSTOMER.parse().unwrap())
    }

    fn api_subscription() -> oxidt_creem::Subscription {
        serde_json::from_slice(include_bytes!("../tests/fixtures/api_subscription.json")).unwrap()
    }

    #[test]
    fn api_subscription_maps_to_a_snapshot() {
        let now = Utc::now();
        let s = subscription_snapshot(
            &api_subscription().into(),
            None,
            None,
            Environment::Sandbox,
            now,
        )
        .unwrap();
        assert_eq!(s.store, Store::Creem);
        assert_eq!(s.environment, Environment::Sandbox);
        assert_eq!(s.original_transaction_id, "sub_6pC2lNB6joCRQIZ1aMrTpi");
        assert_eq!(s.store_transaction_id, "tran_5yMaWzAl3jxuGJMCOrYWwk");
        assert_eq!(s.product_id, "prod_monthly");
        assert_eq!(s.kind, PurchaseKind::Subscription);
        assert_eq!(s.status, PurchaseStatus::Active);
        assert!(s.auto_renew);
        assert_eq!(
            s.purchased_at,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
        );
        assert_eq!(
            s.expires_at,
            Some(Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap())
        );
        assert_eq!(
            s.price,
            Some(Money {
                amount_minor: 999,
                currency: "EUR".into(),
            })
        );
        assert_eq!(s.customer_ref, Some(customer()));
        assert_eq!(
            s.store_customer_id.as_deref(),
            Some("cust_1OcIK1GEuVvXZwD19tjq2z")
        );
        assert_eq!(s.as_of, now);
    }

    #[test]
    fn api_checkout_carries_its_subscription() {
        let api: oxidt_creem::CompletedCheckout =
            serde_json::from_slice(include_bytes!("../tests/fixtures/api_checkout.json")).unwrap();
        assert_eq!(api.status.as_deref(), Some("completed"));
        let Expandable::Object(sub) = expandable_subscription(api.subscription.unwrap()) else {
            panic!("expanded subscription expected");
        };
        let s = subscription_snapshot(
            &sub,
            None,
            api.metadata.as_ref(),
            Environment::Production,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.original_transaction_id, "sub_6pC2lNB6joCRQIZ1aMrTpi");
        assert_eq!(s.environment, Environment::Production);
        assert_eq!(s.customer_ref, Some(customer()));
    }

    #[test]
    fn unknown_subscription_status_is_malformed() {
        let mut sub = api_subscription();
        sub.status = "incomplete".into();
        let err = subscription_snapshot(&sub.into(), None, None, Environment::Sandbox, Utc::now())
            .unwrap_err();
        assert!(matches!(err, AdapterError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn checkout_metadata_always_carries_the_customer() {
        let request = CheckoutRequest {
            product_id: "prod_monthly".into(),
            customer: customer(),
            success_url: None,
            metadata: BTreeMap::from([
                ("source".into(), "web".into()),
                (CREEM_CUSTOMER_METADATA_KEY.into(), "spoofed".into()),
            ]),
        };
        let metadata = checkout_metadata(&request);
        assert_eq!(metadata["source"], "web");
        assert_eq!(metadata[CREEM_CUSTOMER_METADATA_KEY], CUSTOMER);
        assert_eq!(customer_ref(Some(&metadata)), Some(customer()));
    }

    #[test]
    fn errors_map_to_the_contract() {
        use oxidt_creem::Error as E;
        assert!(matches!(
            map_error(E::Signature),
            AdapterError::InvalidSignature(_)
        ));
        assert!(matches!(map_error(E::Event), AdapterError::Malformed(_)));
        assert!(matches!(map_error(E::Api(404)), AdapterError::NotFound));
        assert!(matches!(
            map_error(E::Api(401)),
            AdapterError::Credentials(_)
        ));
        assert!(matches!(
            map_error(E::Configuration),
            AdapterError::Credentials(_)
        ));
        assert!(matches!(map_error(E::Api(500)), AdapterError::Upstream(_)));
        assert!(matches!(map_error(E::Response), AdapterError::Upstream(_)));
    }

    #[tokio::test]
    async fn api_calls_without_an_api_key_fail_before_the_network() {
        let adapter = CreemAdapter::new();
        let creds = credentials("");
        for id in ["sub_1", "ch_1"] {
            let err = adapter.fetch_purchases(&creds, id).await.unwrap_err();
            assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");
        }
        let err = adapter.portal_url(&creds, "cust_1").await.unwrap_err();
        assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");
        let request = CheckoutRequest {
            product_id: "prod_monthly".into(),
            customer: customer(),
            success_url: None,
            metadata: BTreeMap::new(),
        };
        let err = adapter.create_checkout(&creds, &request).await.unwrap_err();
        assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");
    }

    #[tokio::test]
    async fn unknown_id_prefix_is_not_found() {
        let err = CreemAdapter::new()
            .fetch_purchases(&credentials("creem_test_key"), "ord_1")
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::NotFound), "{err:?}");
    }
}
