use chrono::{DateTime, TimeZone, Utc};
use entitle_core::{
    AdapterError, CreemCredentials, CustomerId, Environment, InboundNotification, Money,
    ParsedNotification, PeriodType, PurchaseKind, PurchaseSnapshot, PurchaseStatus, Store,
    StoreAdapter,
};
use entitle_creem::CreemAdapter;
use hmac::{KeyInit, Mac};

const SECRET: &str = "whsec_test_secret";
const CUSTOMER: &str = "7f0c3a52-4f0e-4b8e-9c4a-2f1d8a7e6b10";
const SUB: &str = "sub_6pC2lNB6joCRQIZ1aMrTpi";
const CUST: &str = "cust_1OcIK1GEuVvXZwD19tjq2z";

fn credentials(api_key: &str) -> CreemCredentials {
    CreemCredentials {
        api_key: api_key.into(),
        webhook_secret: SECRET.into(),
        test_mode: true,
    }
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn notification(body: &[u8], signature: Option<String>) -> InboundNotification {
    let mut n = InboundNotification {
        body: body.to_vec(),
        ..Default::default()
    };
    if let Some(signature) = signature {
        n.headers.insert("creem-signature".into(), signature);
    }
    n
}

/// A signed delivery, parsed with credentials whose empty API key makes any
/// network call fail with `Credentials`: these tests never reach Creem.
async fn parse(body: &[u8]) -> Result<ParsedNotification, AdapterError> {
    CreemAdapter::new()
        .parse_notification(
            &credentials(""),
            &notification(body, Some(sign(SECRET, body))),
        )
        .await
}

async fn one(body: &[u8]) -> PurchaseSnapshot {
    let mut parsed = parse(body).await.unwrap();
    assert_eq!(parsed.snapshots.len(), 1, "{parsed:?}");
    parsed.snapshots.remove(0)
}

fn t(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn event_time(offset_secs: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(1_767_225_600_000 + offset_secs * 1000)
        .unwrap()
}

fn customer() -> Option<CustomerId> {
    Some(CustomerId(CUSTOMER.parse().unwrap()))
}

fn eur(amount_minor: i64) -> Option<Money> {
    Some(Money {
        amount_minor,
        currency: "EUR".into(),
    })
}

#[tokio::test]
async fn subscription_paid_maps_every_field() {
    let body = include_bytes!("fixtures/subscription_paid.json");
    let parsed = parse(body).await.unwrap();
    assert_eq!(parsed.event_id, "evt_subscription_paid");
    assert_eq!(
        parsed.snapshots,
        vec![PurchaseSnapshot {
            store: Store::Creem,
            environment: Environment::Sandbox,
            original_transaction_id: SUB.into(),
            store_transaction_id: "tran_5yMaWzAl3jxuGJMCOrYWwk".into(),
            product_id: "prod_monthly".into(),
            kind: PurchaseKind::Subscription,
            status: PurchaseStatus::Active,
            period_type: PeriodType::Normal,
            purchased_at: t("2026-01-01T00:00:00Z"),
            expires_at: Some(t("2026-02-01T00:00:00Z")),
            grace_period_expires_at: None,
            auto_renew: true,
            price: eur(999),
            customer_ref: customer(),
            store_customer_id: Some(CUST.into()),
            as_of: event_time(2),
        }]
    );
}

#[tokio::test]
async fn subscription_events_map_to_statuses() {
    use PurchaseStatus as S;
    let cases: [(&[u8], S, bool); 10] = [
        (
            include_bytes!("fixtures/subscription_active.json"),
            S::Active,
            true,
        ),
        (
            include_bytes!("fixtures/subscription_paid.json"),
            S::Active,
            true,
        ),
        (
            include_bytes!("fixtures/subscription_update.json"),
            S::Active,
            true,
        ),
        (
            include_bytes!("fixtures/subscription_trialing.json"),
            S::Active,
            true,
        ),
        (
            include_bytes!("fixtures/subscription_scheduled_cancel.json"),
            S::Cancelled,
            false,
        ),
        (
            include_bytes!("fixtures/subscription_past_due.json"),
            S::GracePeriod,
            true,
        ),
        (
            include_bytes!("fixtures/subscription_unpaid.json"),
            S::BillingRetry,
            true,
        ),
        (
            include_bytes!("fixtures/subscription_paused.json"),
            S::Paused,
            true,
        ),
        (
            include_bytes!("fixtures/subscription_canceled.json"),
            S::Expired,
            false,
        ),
        // The payload still says `active`; the event type decides.
        (
            include_bytes!("fixtures/subscription_expired.json"),
            S::Expired,
            false,
        ),
    ];
    for (body, status, auto_renew) in cases {
        let s = one(body).await;
        let name = String::from_utf8_lossy(&body[..60]).to_string();
        assert_eq!(s.status, status, "{name}");
        assert_eq!(s.auto_renew, auto_renew, "{name}");
        assert_eq!(s.original_transaction_id, SUB, "{name}");
        assert_eq!(s.customer_ref, customer(), "{name}");
        assert_eq!(s.store_customer_id.as_deref(), Some(CUST), "{name}");
        assert!(s.expires_at.is_some(), "{name}");
        let grace = (status == S::GracePeriod)
            .then(|| s.expires_at.unwrap() + entitle_creem::PAST_DUE_GRACE_PERIOD);
        assert_eq!(s.grace_period_expires_at, grace, "{name}");
    }
}

#[tokio::test]
async fn trial_sets_the_period_type() {
    let s = one(include_bytes!("fixtures/subscription_trialing.json")).await;
    assert_eq!(s.period_type, PeriodType::Trial);
    assert_eq!(s.expires_at, Some(t("2026-01-08T00:00:00Z")));
    // No transaction yet: falls back to the subscription id.
    assert_eq!(s.store_transaction_id, SUB);
}

#[tokio::test]
async fn checkout_completed_for_a_subscription() {
    let s = one(include_bytes!(
        "fixtures/checkout_completed_subscription.json"
    ))
    .await;
    assert_eq!(s.kind, PurchaseKind::Subscription);
    assert_eq!(s.original_transaction_id, SUB);
    assert_eq!(s.product_id, "prod_monthly");
    assert_eq!(s.status, PurchaseStatus::Active);
    // The embedded subscription has only the product id, so no price.
    assert_eq!(s.price, None);
    assert_eq!(s.customer_ref, customer());
    assert_eq!(s.store_customer_id.as_deref(), Some(CUST));
    assert_eq!(s.as_of, event_time(30));
}

#[tokio::test]
async fn checkout_completed_for_a_one_time_purchase() {
    let parsed = parse(include_bytes!("fixtures/checkout_completed_onetime.json"))
        .await
        .unwrap();
    assert_eq!(
        parsed.snapshots,
        vec![PurchaseSnapshot {
            store: Store::Creem,
            environment: Environment::Sandbox,
            original_transaction_id: "ord_2Ld8f9wQnVx3kPz7yRt1Ab".into(),
            store_transaction_id: "ord_2Ld8f9wQnVx3kPz7yRt1Ab".into(),
            product_id: "prod_lifetime".into(),
            kind: PurchaseKind::OneTime,
            status: PurchaseStatus::Active,
            period_type: PeriodType::Normal,
            purchased_at: t("2026-01-02T00:00:00Z"),
            expires_at: None,
            grace_period_expires_at: None,
            auto_renew: false,
            price: eur(4999),
            customer_ref: customer(),
            store_customer_id: Some(CUST.into()),
            as_of: event_time(31),
        }]
    );
}

#[tokio::test]
async fn refunds_and_disputes_revoke() {
    let s = one(include_bytes!("fixtures/refund_created.json")).await;
    assert_eq!(s.status, PurchaseStatus::Refunded);
    assert_eq!(s.original_transaction_id, SUB);
    assert!(!s.auto_renew);
    // The refunded subscription has no metadata; the checkout's is used.
    assert_eq!(s.customer_ref, customer());

    let s = one(include_bytes!("fixtures/refund_created_onetime.json")).await;
    assert_eq!(s.status, PurchaseStatus::Refunded);
    assert_eq!(s.kind, PurchaseKind::OneTime);
    assert_eq!(s.original_transaction_id, "ord_2Ld8f9wQnVx3kPz7yRt1Ab");
    assert_eq!(s.customer_ref, customer());

    let s = one(include_bytes!("fixtures/dispute_created.json")).await;
    assert_eq!(s.status, PurchaseStatus::Revoked);
    assert_eq!(s.original_transaction_id, SUB);
    // Dispute payloads carry no metadata at all.
    assert_eq!(s.customer_ref, None);
    assert_eq!(s.store_customer_id.as_deref(), Some(CUST));
}

#[tokio::test]
async fn partial_refunds_do_not_revoke() {
    let full: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/refund_created_onetime.json")).unwrap();
    let partial_by_status = {
        let mut e = full.clone();
        e["object"]["transaction"]["status"] = "partialRefund".into();
        e
    };
    let partial_by_amount = {
        let mut e = full.clone();
        e["object"]["transaction"]["refunded_amount"] = 500.into();
        e
    };
    for event in [partial_by_status, partial_by_amount] {
        let body = serde_json::to_vec(&event).unwrap();
        let parsed = parse(&body).await.unwrap();
        assert_eq!(parsed.event_id, "evt_refund_created_onetime");
        assert!(parsed.snapshots.is_empty(), "{parsed:?}");
    }
    // Refunded in full (amount given explicitly): revoked.
    let mut e = full;
    e["object"]["transaction"]["refunded_amount"] = 1209.into();
    let s = one(&serde_json::to_vec(&e).unwrap()).await;
    assert_eq!(s.status, PurchaseStatus::Refunded);
}

#[tokio::test]
async fn unrelated_events_have_no_snapshots() {
    let parsed = parse(include_bytes!("fixtures/credits_granted.json"))
        .await
        .unwrap();
    assert_eq!(parsed.event_id, "evt_credits_granted");
    assert!(parsed.snapshots.is_empty());
}

#[tokio::test]
async fn production_credentials_give_production_snapshots() {
    let body = include_bytes!("fixtures/subscription_paid.json");
    let creds = CreemCredentials {
        test_mode: false,
        ..credentials("")
    };
    let parsed = CreemAdapter::new()
        .parse_notification(&creds, &notification(body, Some(sign(SECRET, body))))
        .await
        .unwrap();
    assert_eq!(parsed.snapshots[0].environment, Environment::Production);
}

/// `subscription.active` as documented has no period dates: the adapter asks the
/// API, which here fails on the empty API key instead of reaching the network.
#[tokio::test]
async fn subscription_without_period_is_completed_from_the_api() {
    let err = parse(include_bytes!(
        "fixtures/subscription_active_without_period.json"
    ))
    .await
    .unwrap_err();
    assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");
}

#[tokio::test]
async fn bad_signatures_are_rejected() {
    let body = include_bytes!("fixtures/subscription_paid.json");
    let adapter = CreemAdapter::new();
    let creds = credentials("");

    let missing = adapter
        .parse_notification(&creds, &notification(body, None))
        .await;
    let wrong_secret = adapter
        .parse_notification(&creds, &notification(body, Some(sign("other", body))))
        .await;
    let mut tampered = body.to_vec();
    let at = tampered.iter().position(|&b| b == b'9').unwrap();
    tampered[at] = b'1';
    let tampered = adapter
        .parse_notification(&creds, &notification(&tampered, Some(sign(SECRET, body))))
        .await;
    let garbage = adapter
        .parse_notification(&creds, &notification(body, Some("not-hex".into())))
        .await;
    let no_secret = adapter
        .parse_notification(
            &CreemCredentials {
                webhook_secret: String::new(),
                ..creds.clone()
            },
            &notification(body, Some(sign("", body))),
        )
        .await;
    for result in [missing, wrong_secret, tampered, garbage, no_secret] {
        assert!(
            matches!(result, Err(AdapterError::InvalidSignature(_))),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn signed_garbage_is_malformed() {
    for body in [
        &b"not json"[..],
        br#"{"id":"evt_1","eventType":"subscription.paid","created_at":1,"object":{"id":"sub_1"}}"#,
        br#"{"id":"evt_1","eventType":"subscription.paid","object":{}}"#,
    ] {
        let err = parse(body).await.unwrap_err();
        assert!(matches!(err, AdapterError::Malformed(_)), "{err:?}");
    }
}
