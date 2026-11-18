mod common;

use chrono::{DateTime, Utc};
use common::{APP_APPLE_ID, BUNDLE_ID, CaOptions, TestCa, credentials};
use entitle_apple::AppleAdapter;
use entitle_core::{
    AdapterError, CustomerId, Environment, InboundNotification, Money, ParsedNotification,
    PeriodType, PurchaseKind, PurchaseSnapshot, PurchaseStatus, StoreAdapter,
};
use serde_json::{Value, json};

macro_rules! fixture {
    ($name:literal) => {
        serde_json::from_slice::<Value>(include_bytes!(concat!("fixtures/", $name, ".json")))
            .unwrap()
    };
}

// Nothing listens here: notifications must never reach the network.
const NO_NETWORK: &str = "http://127.0.0.1:9";
const TOKEN: &str = "7e3fb20b-4cdb-47cc-936d-99d65f608138";

fn body(signed_payload: &str) -> InboundNotification {
    InboundNotification {
        body: serde_json::to_vec(&json!({ "signedPayload": signed_payload })).unwrap(),
        ..Default::default()
    }
}

fn at(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap()
}

async fn parse(fixture: Value) -> ParsedNotification {
    let ca = TestCa::new();
    ca.adapter(NO_NETWORK, NO_NETWORK)
        .parse_notification(&credentials(), &body(&ca.sign_notification(&fixture)))
        .await
        .unwrap()
}

async fn parse_one(fixture: Value) -> PurchaseSnapshot {
    let mut parsed = parse(fixture).await;
    assert_eq!(parsed.snapshots.len(), 1);
    parsed.snapshots.remove(0)
}

async fn reject(adapter: &AppleAdapter, payload: &str) -> AdapterError {
    adapter
        .parse_notification(&credentials(), &body(payload))
        .await
        .unwrap_err()
}

#[tokio::test]
async fn subscribed_is_an_active_trial() {
    let parsed = parse(fixture!("subscribed")).await;
    assert_eq!(parsed.event_id, "7f1c1e8a-1f0e-4a5b-9a51-000000000001");
    assert_eq!(
        parsed.snapshots,
        vec![PurchaseSnapshot {
            store: entitle_core::Store::AppStore,
            environment: Environment::Sandbox,
            original_transaction_id: "2000000100000001".into(),
            store_transaction_id: "2000000100000001".into(),
            product_id: "com.example.app.pro.monthly".into(),
            kind: PurchaseKind::Subscription,
            status: PurchaseStatus::Active,
            period_type: PeriodType::Trial,
            purchased_at: at(1767225600000),
            expires_at: Some(at(1769904000000)),
            grace_period_expires_at: None,
            auto_renew: true,
            price: Some(Money {
                amount_minor: 0,
                currency: "EUR".into()
            }),
            customer_ref: Some(CustomerId(TOKEN.parse().unwrap())),
            store_customer_id: None,
            as_of: at(1768435200000),
        }]
    );
}

#[tokio::test]
async fn did_renew_is_active_with_the_new_period_and_price_in_cents() {
    let s = parse_one(fixture!("did_renew")).await;
    assert_eq!(s.status, PurchaseStatus::Active);
    assert_eq!(s.period_type, PeriodType::Intro);
    assert_eq!(s.store_transaction_id, "2000000100000002");
    assert_eq!(s.purchased_at, at(1769904000000));
    assert_eq!(s.expires_at, Some(at(1772323200000)));
    assert_eq!(
        s.price,
        Some(Money {
            amount_minor: 199,
            currency: "EUR".into()
        })
    );
}

#[tokio::test]
async fn price_uses_the_currency_minor_unit() {
    for (currency, milliunits, minor) in [
        ("EUR", 4990, 499),
        ("JPY", 480_000, 480),
        ("KWD", 1_250, 1_250),
    ] {
        let mut n = fixture!("did_renew");
        n["data"]["transaction"]["price"] = json!(milliunits);
        n["data"]["transaction"]["currency"] = json!(currency);
        let s = parse_one(n).await;
        assert_eq!(
            s.price,
            Some(Money {
                amount_minor: minor,
                currency: currency.into()
            })
        );
    }
}

#[tokio::test]
async fn auto_renew_disabled_is_cancelled() {
    let s = parse_one(fixture!("did_change_renewal_status")).await;
    assert_eq!(s.status, PurchaseStatus::Cancelled);
    assert!(!s.auto_renew);
    assert_eq!(s.expires_at, Some(at(1769904000000)));
}

#[tokio::test]
async fn renewal_pref_change_keeps_the_current_product() {
    let s = parse_one(fixture!("did_change_renewal_pref")).await;
    assert_eq!(s.status, PurchaseStatus::Active);
    assert_eq!(s.product_id, "com.example.app.pro.monthly");
    assert_eq!(s.period_type, PeriodType::Normal);
}

#[tokio::test]
async fn failed_renewal_in_grace_period() {
    let s = parse_one(fixture!("did_fail_to_renew_grace_period")).await;
    assert_eq!(s.status, PurchaseStatus::GracePeriod);
    assert_eq!(s.grace_period_expires_at, Some(at(1770508800000)));
    assert!(s.auto_renew);
}

#[tokio::test]
async fn failed_renewal_without_grace_period_is_billing_retry() {
    let s = parse_one(fixture!("did_fail_to_renew")).await;
    assert_eq!(s.status, PurchaseStatus::BillingRetry);
    assert_eq!(s.grace_period_expires_at, None);
}

#[tokio::test]
async fn grace_period_expired_is_billing_retry() {
    let s = parse_one(fixture!("grace_period_expired")).await;
    assert_eq!(s.status, PurchaseStatus::BillingRetry);
}

#[tokio::test]
async fn expired() {
    let s = parse_one(fixture!("expired")).await;
    assert_eq!(s.status, PurchaseStatus::Expired);
    assert!(!s.auto_renew);
}

#[tokio::test]
async fn refund_is_refunded() {
    let s = parse_one(fixture!("refund")).await;
    assert_eq!(s.status, PurchaseStatus::Refunded);
}

#[tokio::test]
async fn refund_reversed_is_active_again() {
    let s = parse_one(fixture!("refund_reversed")).await;
    assert_eq!(s.status, PurchaseStatus::Active);
}

#[tokio::test]
async fn revoke_is_revoked_and_unlinked() {
    let s = parse_one(fixture!("revoke")).await;
    assert_eq!(s.status, PurchaseStatus::Revoked);
    assert_eq!(s.customer_ref, None);
}

#[tokio::test]
async fn price_increase_keeps_the_state() {
    let s = parse_one(fixture!("price_increase")).await;
    assert_eq!(s.status, PurchaseStatus::Active);
    assert_eq!(s.price.unwrap().amount_minor, 499);
}

#[tokio::test]
async fn offer_redeemed_is_a_normal_period() {
    let s = parse_one(fixture!("offer_redeemed")).await;
    assert_eq!(s.status, PurchaseStatus::Active);
    assert_eq!(s.product_id, "com.example.app.pro.yearly");
    assert_eq!(s.period_type, PeriodType::Normal);
    assert_eq!(s.price.unwrap().amount_minor, 2999);
}

#[tokio::test]
async fn one_time_purchase_never_expires() {
    let s = parse_one(fixture!("one_time_charge")).await;
    assert_eq!(s.kind, PurchaseKind::OneTime);
    assert_eq!(s.status, PurchaseStatus::Active);
    assert_eq!(s.expires_at, None);
    assert!(!s.auto_renew);
    assert_eq!(s.original_transaction_id, "2000000200000001");
    assert_eq!(s.price.unwrap().amount_minor, 9999);
}

#[tokio::test]
async fn consumables_and_non_renewing_subscriptions_are_acknowledged_without_snapshots() {
    for product_type in ["Consumable", "Non-Renewing Subscription"] {
        let mut fixture = fixture!("one_time_charge");
        fixture["data"]["transaction"]["type"] = product_type.into();
        let parsed = parse(fixture).await;
        assert!(parsed.snapshots.is_empty(), "{product_type}");
    }
}

#[tokio::test]
async fn test_notification_has_no_snapshots() {
    let parsed = parse(fixture!("test")).await;
    assert_eq!(parsed.event_id, "7f1c1e8a-1f0e-4a5b-9a51-000000000015");
    assert!(parsed.snapshots.is_empty());
}

fn to_production(mut fixture: Value) -> Value {
    fixture["data"]["environment"] = "Production".into();
    fixture["data"]["transaction"]["environment"] = "Production".into();
    fixture["data"]["renewal"]["environment"] = "Production".into();
    fixture
}

#[tokio::test]
async fn production_notification_checks_the_app_apple_id() {
    let ca = TestCa::new();
    let adapter = ca.adapter(NO_NETWORK, NO_NETWORK);
    let fixture = to_production(fixture!("did_renew"));
    let payload = ca.sign_notification(&fixture);

    let parsed = adapter
        .parse_notification(&credentials(), &body(&payload))
        .await
        .unwrap();
    assert_eq!(parsed.snapshots[0].environment, Environment::Production);

    let mut other_app = credentials();
    other_app.app_apple_id = Some(APP_APPLE_ID + 1);
    let err = adapter
        .parse_notification(&other_app, &body(&payload))
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");

    let mut missing = credentials();
    missing.app_apple_id = None;
    let err = adapter
        .parse_notification(&missing, &body(&payload))
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");
}

#[tokio::test]
async fn wrong_bundle_id_is_rejected() {
    let ca = TestCa::new();
    let mut fixture = fixture!("did_renew");
    fixture["data"]["bundleId"] = "com.other.app".into();
    let err = reject(
        &ca.adapter(NO_NETWORK, NO_NETWORK),
        &ca.sign_notification(&fixture),
    )
    .await;
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");

    // The notification is ours, but the transaction inside belongs to another app.
    let mut fixture = fixture!("did_renew");
    fixture["data"]["transaction"]["bundleId"] = "com.other.app".into();
    let err = reject(
        &ca.adapter(NO_NETWORK, NO_NETWORK),
        &ca.sign_notification(&fixture),
    )
    .await;
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");
    assert_ne!(BUNDLE_ID, "com.other.app");
}

#[tokio::test]
async fn tampered_signature_is_rejected() {
    let ca = TestCa::new();
    let payload = ca.sign_notification(&fixture!("did_renew"));
    let (rest, signature) = payload.rsplit_once('.').unwrap();
    let flipped = if signature.starts_with('A') { "B" } else { "A" };
    let tampered = format!("{rest}.{flipped}{}", &signature[1..]);
    let err = reject(&ca.adapter(NO_NETWORK, NO_NETWORK), &tampered).await;
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");
}

#[tokio::test]
async fn tampered_payload_is_rejected() {
    let ca = TestCa::new();
    let payload = ca.sign_notification(&fixture!("did_renew"));
    let other = ca.sign_notification(&fixture!("expired"));
    let mut parts: Vec<&str> = payload.split('.').collect();
    parts[1] = other.split('.').nth(1).unwrap();
    let err = reject(&ca.adapter(NO_NETWORK, NO_NETWORK), &parts.join(".")).await;
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");
}

#[tokio::test]
async fn tampered_nested_transaction_is_rejected() {
    let ca = TestCa::new();
    let untrusted = TestCa::new();
    let mut fixture = fixture!("did_renew");
    let tx = fixture["data"]["transaction"].take();
    fixture["data"]["signedTransactionInfo"] = untrusted.sign(&tx).into();
    fixture["data"]
        .as_object_mut()
        .unwrap()
        .remove("transaction");
    let err = reject(
        &ca.adapter(NO_NETWORK, NO_NETWORK),
        &ca.sign_notification(&fixture),
    )
    .await;
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");
}

#[tokio::test]
async fn untrusted_chain_is_rejected() {
    let ca = TestCa::new();
    let payload = TestCa::new().sign_notification(&fixture!("did_renew"));
    let err = reject(&ca.adapter(NO_NETWORK, NO_NETWORK), &payload).await;
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");

    // The real adapter only trusts Apple Root CA G3.
    let err = reject(&AppleAdapter::new(), &payload).await;
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");
}

#[tokio::test]
async fn chain_without_apple_oids_is_rejected() {
    let ca = TestCa::with(CaOptions {
        without_oids: true,
        ..Default::default()
    });
    let payload = ca.sign_notification(&fixture!("did_renew"));
    let err = reject(&ca.adapter(NO_NETWORK, NO_NETWORK), &payload).await;
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");
}

#[tokio::test]
async fn leaf_expired_before_signing_is_rejected() {
    let ca = TestCa::with(CaOptions {
        leaf_expires: Some((2025, 12, 31)),
        ..Default::default()
    });
    let payload = ca.sign_notification(&fixture!("did_renew"));
    let err = reject(&ca.adapter(NO_NETWORK, NO_NETWORK), &payload).await;
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");

    // Validity is checked at `signedDate`, so the same leaf was fine for older notifications.
    let mut old = fixture!("did_renew");
    old["signedDate"] = 1735689600000i64.into(); // 2025-01-01
    old["data"]["transaction"]["signedDate"] = 1735689600000i64.into();
    old["data"]["renewal"]["signedDate"] = 1735689600000i64.into();
    let parsed = ca
        .adapter(NO_NETWORK, NO_NETWORK)
        .parse_notification(&credentials(), &body(&ca.sign_notification(&old)))
        .await
        .unwrap();
    assert_eq!(parsed.snapshots.len(), 1);
}

#[tokio::test]
async fn unsigned_payloads_are_rejected() {
    let ca = TestCa::new();
    let adapter = ca.adapter(NO_NETWORK, NO_NETWORK);
    let b64 = |v: &Value| {
        app_store_server_library::crypto::jws::b64url_encode(&serde_json::to_vec(v).unwrap())
    };
    // An Xcode/LocalTesting environment must not switch verification off.
    for environment in ["Xcode", "LocalTesting", "Sandbox"] {
        let mut fixture = fixture!("test");
        fixture["data"]["environment"] = environment.into();
        let unsigned = format!("{}.{}.", b64(&json!({"alg": "none"})), b64(&fixture));
        let err = reject(&adapter, &unsigned).await;
        assert!(
            matches!(err, AdapterError::InvalidSignature(_)),
            "{environment}: {err:?}"
        );
    }
    let err = reject(&adapter, "garbage").await;
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");
}

#[tokio::test]
async fn non_json_bodies_are_malformed() {
    let ca = TestCa::new();
    let adapter = ca.adapter(NO_NETWORK, NO_NETWORK);
    for raw in [&b""[..], b"not json", br#"{"payload": "x"}"#] {
        let err = adapter
            .parse_notification(
                &credentials(),
                &InboundNotification {
                    body: raw.to_vec(),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Malformed(_)), "{err:?}");
    }
}
