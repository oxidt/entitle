use std::sync::OnceLock;

use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::KeyPair as _;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use entitle_core::{
    AdapterError, CustomerId, Environment, GoogleCredentials, InboundNotification, Money,
    PeriodType, PurchaseKind, PurchaseSnapshot, PurchaseStatus, StoreAdapter,
};
use entitle_google::{Endpoints, GoogleAdapter};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, header, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PACKAGE: &str = "com.example.app";
const AUDIENCE: &str = "https://entitle.example.com/webhooks/google/p1";
const KID: &str = "test-kid";
const PUSH_EMAIL: &str = "pubsub@example.iam.gserviceaccount.com";
const CUSTOMER: &str = "7f0c3a52-4f0e-4b8e-9c4a-2f1d8a7e6b10";
const API: &str = "/androidpublisher/v3/applications/com.example.app/purchases";

/// Test RSA key (PKCS#8 PEM) and its JWKS. Signs both OIDC tokens and the service-account grant.
fn key() -> &'static (String, Value) {
    static KEY: OnceLock<(String, Value)> = OnceLock::new();
    KEY.get_or_init(|| {
        let pair = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let der: Pkcs8V1Der = pair.as_der().unwrap();
        let body = STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            lines.join("\n")
        );
        let public = pair.public_key();
        let jwks = json!({"keys": [{
            "kty": "RSA", "alg": "RS256", "use": "sig", "kid": KID,
            "n": URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),
            "e": URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero()),
        }]});
        (pem, jwks)
    })
}

fn credentials() -> GoogleCredentials {
    GoogleCredentials {
        service_account_json: json!({
            "type": "service_account",
            "client_email": "entitle@example.iam.gserviceaccount.com",
            "private_key_id": "sa-key",
            "private_key": key().0,
        }),
        package_name: PACKAGE.into(),
        pubsub_audience: AUDIENCE.into(),
        pubsub_service_account_email: PUSH_EMAIL.into(),
    }
}

fn oidc_token(claims: Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.into());
    let key = EncodingKey::from_rsa_pem(key().0.as_bytes()).unwrap();
    jsonwebtoken::encode(&header, &claims, &key).unwrap()
}

fn claims() -> Value {
    let now = Utc::now().timestamp();
    json!({
        "iss": "https://accounts.google.com",
        "aud": AUDIENCE,
        "email": PUSH_EMAIL,
        "email_verified": true,
        "iat": now,
        "exp": now + 3600,
    })
}

fn push(body: &[u8], token: Option<&str>) -> InboundNotification {
    let mut n = InboundNotification {
        body: body.to_vec(),
        ..Default::default()
    };
    if let Some(t) = token {
        n.headers
            .insert("authorization".into(), format!("Bearer {t}"));
    }
    n
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Mock server with Google's JWKS and token endpoints.
async fn setup() -> (MockServer, GoogleAdapter) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/certs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&key().1))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion=",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "at-1", "expires_in": 3599})),
        )
        .mount(&server)
        .await;
    let adapter = GoogleAdapter::with_endpoints(Endpoints {
        api_base: server.uri(),
        token_url: format!("{}/token", server.uri()),
        jwks_url: format!("{}/certs", server.uri()),
    });
    (server, adapter)
}

async fn mock_get(server: &MockServer, url: &str, status: u16, body: Value) {
    Mock::given(method("GET"))
        .and(path(url))
        .and(header("authorization", "Bearer at-1"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn mock_subscription(server: &MockServer, token: &str, fixture_name: &str) {
    let url = format!("{API}/subscriptionsv2/tokens/{token}");
    mock_get(server, &url, 200, fixture(fixture_name)).await;
}

async fn expect_ack(server: &MockServer, url: &str, status: u16, times: u64) {
    Mock::given(method("POST"))
        .and(path(url))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({})))
        .expect(times)
        .mount(server)
        .await;
}

fn ts(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

// ---------- OIDC verification ----------

async fn assert_rejected(token: Option<String>) {
    let (server, adapter) = setup().await;
    let body = include_bytes!("fixtures/push_subscription.json");
    let err = adapter
        .parse_notification(&credentials(), &push(body, token.as_deref()))
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");
    // Nothing beyond the JWKS may be fetched for a rejected push.
    for r in server.received_requests().await.unwrap() {
        assert_eq!(r.url.path(), "/certs");
    }
}

#[tokio::test]
async fn missing_authorization_is_rejected_offline() {
    let (server, adapter) = setup().await;
    for n in [
        push(b"{}", None),
        push(b"{}", Some("not-a-jwt")),
        InboundNotification::default(),
    ] {
        let err = adapter
            .parse_notification(&credentials(), &n)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::InvalidSignature(_)), "{err:?}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn wrong_audience_is_rejected() {
    let mut c = claims();
    c["aud"] = json!("https://attacker.example.com");
    assert_rejected(Some(oidc_token(c))).await;
}

#[tokio::test]
async fn wrong_issuer_is_rejected() {
    let mut c = claims();
    c["iss"] = json!("https://evil.example.com");
    assert_rejected(Some(oidc_token(c))).await;
}

#[tokio::test]
async fn expired_token_is_rejected() {
    let mut c = claims();
    let past = Utc::now().timestamp() - 3600;
    c["iat"] = json!(past - 3600);
    c["exp"] = json!(past);
    assert_rejected(Some(oidc_token(c))).await;
}

#[tokio::test]
async fn other_service_account_is_rejected() {
    let mut c = claims();
    c["email"] = json!("attacker@evil.iam.gserviceaccount.com");
    assert_rejected(Some(oidc_token(c))).await;
}

#[tokio::test]
async fn unverified_email_is_rejected() {
    let mut c = claims();
    c["email_verified"] = json!(false);
    assert_rejected(Some(oidc_token(c))).await;
}

#[tokio::test]
async fn missing_email_is_rejected() {
    let mut c = claims();
    c.as_object_mut().unwrap().remove("email");
    assert_rejected(Some(oidc_token(c))).await;
}

#[tokio::test]
async fn empty_service_account_email_is_a_credentials_error() {
    let (server, adapter) = setup().await;
    let mut c = credentials();
    c.pubsub_service_account_email = String::new();
    let body = include_bytes!("fixtures/push_subscription.json");
    let err = adapter
        .parse_notification(&c, &push(body, Some(&oidc_token(claims()))))
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn bad_signature_is_rejected() {
    let token = oidc_token(claims());
    let (rest, sig) = token.rsplit_once('.').unwrap();
    let mut sig = URL_SAFE_NO_PAD.decode(sig).unwrap();
    sig[0] ^= 0xff;
    assert_rejected(Some(format!("{rest}.{}", URL_SAFE_NO_PAD.encode(sig)))).await;
}

// ---------- notifications ----------

#[tokio::test]
async fn subscription_notification_fetches_state() {
    let (server, adapter) = setup().await;
    mock_subscription(&server, "sub-active-token", "sub_active.json").await;
    let body = include_bytes!("fixtures/push_subscription.json");
    let token = oidc_token(claims());

    let parsed = adapter
        .parse_notification(&credentials(), &push(body, Some(&token)))
        .await
        .unwrap();
    assert_eq!(parsed.event_id, "11111111111");
    let expected = PurchaseSnapshot {
        store: entitle_core::Store::PlayStore,
        environment: Environment::Production,
        original_transaction_id: "sub-active-token".into(),
        store_transaction_id: "GPA.1111-2222-3333-44444..2".into(),
        product_id: "pro_monthly".into(),
        kind: PurchaseKind::Subscription,
        status: PurchaseStatus::Active,
        period_type: PeriodType::Normal,
        purchased_at: ts("2026-08-01T10:00:00Z"),
        expires_at: Some(ts("2099-01-01T00:00:00Z")),
        grace_period_expires_at: None,
        auto_renew: true,
        price: Some(Money {
            amount_minor: 499,
            currency: "EUR".into(),
        }),
        customer_ref: Some(CustomerId(CUSTOMER.parse().unwrap())),
        store_customer_id: None,
        as_of: DateTime::from_timestamp_millis(1788256800123).unwrap(),
    };
    assert_eq!(parsed.snapshots, vec![expected]);

    // JWKS and access token are cached across notifications.
    adapter
        .parse_notification(&credentials(), &push(body, Some(&token)))
        .await
        .unwrap();
    let paths: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    assert_eq!(paths.iter().filter(|p| *p == "/certs").count(), 1);
    assert_eq!(paths.iter().filter(|p| *p == "/token").count(), 1);
}

#[tokio::test]
async fn test_notification_has_no_snapshots() {
    let (_server, adapter) = setup().await;
    let body = include_bytes!("fixtures/push_test.json");
    let parsed = adapter
        .parse_notification(&credentials(), &push(body, Some(&oidc_token(claims()))))
        .await
        .unwrap();
    assert_eq!(parsed.event_id, "44444444444");
    assert!(parsed.snapshots.is_empty());
}

#[tokio::test]
async fn other_package_is_rejected() {
    let (_server, adapter) = setup().await;
    let body = include_bytes!("fixtures/push_wrong_package.json");
    let err = adapter
        .parse_notification(&credentials(), &push(body, Some(&oidc_token(claims()))))
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::Malformed(_)), "{err:?}");
}

#[tokio::test]
async fn garbage_body_is_malformed() {
    let (_server, adapter) = setup().await;
    let err = adapter
        .parse_notification(&credentials(), &push(b"nope", Some(&oidc_token(claims()))))
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::Malformed(_)), "{err:?}");
}

#[tokio::test]
async fn one_time_notification_fetches_and_acknowledges_product() {
    let (server, adapter) = setup().await;
    let url = format!("{API}/productsv2/tokens/product-token");
    mock_get(&server, &url, 200, fixture("product_purchased.json")).await;
    expect_ack(
        &server,
        &format!("{API}/products/lifetime/tokens/product-token:acknowledge"),
        200,
        1,
    )
    .await;
    let body = include_bytes!("fixtures/push_one_time.json");
    let parsed = adapter
        .parse_notification(&credentials(), &push(body, Some(&oidc_token(claims()))))
        .await
        .unwrap();
    let [s] = parsed.snapshots.as_slice() else {
        panic!("{:?}", parsed.snapshots)
    };
    assert_eq!(s.kind, PurchaseKind::OneTime);
    assert_eq!(s.status, PurchaseStatus::Active);
    assert_eq!(s.product_id, "lifetime");
    assert_eq!(s.store_transaction_id, "GPA.5555-6666-7777-88888");
    assert_eq!(s.expires_at, None);
    assert_eq!(s.purchased_at, ts("2026-08-01T10:00:00Z"));
    assert_eq!(s.customer_ref, Some(CustomerId(CUSTOMER.parse().unwrap())));
}

#[tokio::test]
async fn voided_notification_marks_refunded() {
    let (server, adapter) = setup().await;
    mock_subscription(&server, "sub-active-token", "sub_active.json").await;
    let body = include_bytes!("fixtures/push_voided.json");
    let parsed = adapter
        .parse_notification(&credentials(), &push(body, Some(&oidc_token(claims()))))
        .await
        .unwrap();
    let [s] = parsed.snapshots.as_slice() else {
        panic!("{:?}", parsed.snapshots)
    };
    assert_eq!(s.status, PurchaseStatus::Refunded);
    assert!(!s.auto_renew);
    assert_eq!(s.original_transaction_id, "sub-active-token");
}

#[tokio::test]
async fn partially_voided_purchase_is_not_revoked() {
    // No purchase mocks: a partial refund must not even look the purchase up.
    let (_server, adapter) = setup().await;
    let data = json!({
        "version": "1.0",
        "packageName": "com.example.app",
        "eventTimeMillis": "1788256800123",
        "voidedPurchaseNotification": {
            "purchaseToken": "product-token",
            "orderId": "GPA.1111-2222-3333-55555",
            "productType": 2,
            "refundType": 2
        }
    });
    let body = json!({
        "message": {"data": STANDARD.encode(data.to_string()), "messageId": "44444444444"},
        "subscription": "projects/example/subscriptions/play-rtdn"
    });
    let parsed = adapter
        .parse_notification(
            &credentials(),
            &push(body.to_string().as_bytes(), Some(&oidc_token(claims()))),
        )
        .await
        .unwrap();
    assert_eq!(parsed.event_id, "44444444444");
    assert!(parsed.snapshots.is_empty(), "{:?}", parsed.snapshots);
}

// ---------- fetch_purchases ----------

#[tokio::test]
async fn subscription_states_map_to_statuses() {
    use PurchaseStatus::*;
    let cases = [
        ("sub_active.json", Active, PeriodType::Normal, true),
        ("sub_trial.json", Active, PeriodType::Trial, true),
        ("sub_grace.json", GracePeriod, PeriodType::Normal, true),
        ("sub_on_hold.json", BillingRetry, PeriodType::Normal, true),
        ("sub_paused.json", Paused, PeriodType::Normal, true),
        ("sub_cancelled.json", Cancelled, PeriodType::Normal, false),
        ("sub_expired.json", Expired, PeriodType::Normal, false),
    ];
    for (name, status, period, auto_renew) in cases {
        let (server, adapter) = setup().await;
        mock_subscription(&server, "tok", name).await;
        // Only the trial fixture is unacknowledged.
        let ack = u64::from(name == "sub_trial.json");
        expect_ack(
            &server,
            &format!("{API}/subscriptions/pro_monthly/tokens/tok:acknowledge"),
            200,
            ack,
        )
        .await;
        let before = Utc::now();
        let snapshots = adapter
            .fetch_purchases(&credentials(), "tok")
            .await
            .unwrap();
        let [s] = snapshots.as_slice() else {
            panic!("{name}: {snapshots:?}")
        };
        assert_eq!(s.status, status, "{name}");
        assert_eq!(s.period_type, period, "{name}");
        assert_eq!(s.auto_renew, auto_renew, "{name}");
        assert_eq!(s.environment, Environment::Production, "{name}");
        assert_eq!(
            s.grace_period_expires_at.is_some(),
            status == GracePeriod,
            "{name}"
        );
        assert!(s.as_of >= before && s.as_of <= Utc::now(), "{name}");
        server.verify().await;
    }
}

#[tokio::test]
async fn price_uses_the_currency_minor_unit() {
    let (server, adapter) = setup().await;
    for (currency, units, nanos, minor) in [
        ("EUR", "4", 990_000_000, 499),
        ("JPY", "480", 0, 480),
        ("KWD", "1", 250_000_000, 1_250),
    ] {
        let mut sub = fixture("sub_active.json");
        sub["lineItems"][0]["autoRenewingPlan"]["recurringPrice"] =
            json!({"currencyCode": currency, "units": units, "nanos": nanos});
        let token = format!("tok-{currency}");
        mock_get(
            &server,
            &format!("{API}/subscriptionsv2/tokens/{token}"),
            200,
            sub,
        )
        .await;
        let snapshots = adapter
            .fetch_purchases(&credentials(), &token)
            .await
            .unwrap();
        assert_eq!(
            snapshots[0].price,
            Some(Money {
                amount_minor: minor,
                currency: currency.into(),
            })
        );
    }
}

#[tokio::test]
async fn upgrade_returns_the_replaced_purchase_too() {
    let (server, adapter) = setup().await;
    mock_subscription(&server, "sub-new-token", "sub_upgraded.json").await;
    mock_subscription(&server, "sub-old-token", "sub_replaced.json").await;
    let snapshots = adapter
        .fetch_purchases(&credentials(), "sub-new-token")
        .await
        .unwrap();
    let statuses: Vec<_> = snapshots
        .iter()
        .map(|s| (s.original_transaction_id.as_str(), s.status))
        .collect();
    assert_eq!(
        statuses,
        [
            ("sub-new-token", PurchaseStatus::Active),
            ("sub-old-token", PurchaseStatus::Expired)
        ]
    );
}

#[tokio::test]
async fn test_purchase_is_sandbox_and_acknowledged() {
    let (server, adapter) = setup().await;
    mock_subscription(&server, "tok", "sub_test_purchase.json").await;
    expect_ack(
        &server,
        &format!("{API}/subscriptions/pro_monthly/tokens/tok:acknowledge"),
        200,
        1,
    )
    .await;
    let snapshots = adapter
        .fetch_purchases(&credentials(), "tok")
        .await
        .unwrap();
    assert_eq!(snapshots[0].environment, Environment::Sandbox);
}

#[tokio::test]
async fn failed_acknowledgement_still_returns_state() {
    let (server, adapter) = setup().await;
    mock_subscription(&server, "tok", "sub_test_purchase.json").await;
    expect_ack(
        &server,
        &format!("{API}/subscriptions/pro_monthly/tokens/tok:acknowledge"),
        500,
        1,
    )
    .await;
    let snapshots = adapter
        .fetch_purchases(&credentials(), "tok")
        .await
        .unwrap();
    assert_eq!(snapshots.len(), 1);
}

#[tokio::test]
async fn one_time_purchase_falls_back_to_products() {
    let (server, adapter) = setup().await;
    let sub = format!("{API}/subscriptionsv2/tokens/product-token");
    mock_get(&server, &sub, 400, json!({"error": {"code": 400}})).await;
    let product = format!("{API}/productsv2/tokens/product-token");
    mock_get(&server, &product, 200, fixture("product_purchased.json")).await;
    expect_ack(
        &server,
        &format!("{API}/products/lifetime/tokens/product-token:acknowledge"),
        200,
        1,
    )
    .await;
    let snapshots = adapter
        .fetch_purchases(&credentials(), "product-token")
        .await
        .unwrap();
    assert_eq!(snapshots[0].kind, PurchaseKind::OneTime);
}

#[tokio::test]
async fn unknown_token_is_not_found() {
    let (_server, adapter) = setup().await;
    for id in ["missing", "../evil", ""] {
        let err = adapter
            .fetch_purchases(&credentials(), id)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::NotFound), "{id}: {err:?}");
    }
}

#[tokio::test]
async fn rejected_service_account_is_a_credentials_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .mount(&server)
        .await;
    let adapter = GoogleAdapter::with_endpoints(Endpoints {
        api_base: server.uri(),
        token_url: format!("{}/token", server.uri()),
        jwks_url: format!("{}/certs", server.uri()),
    });
    let err = adapter
        .fetch_purchases(&credentials(), "tok")
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");

    let mut bad = credentials();
    bad.service_account_json = json!({"client_email": "x"});
    let err = adapter.fetch_purchases(&bad, "tok").await.unwrap_err();
    assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");
}

#[tokio::test]
async fn check_credentials_lists_voided_purchases() {
    let (server, adapter) = setup().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/androidpublisher/v3/applications/{}/purchases/voidedpurchases",
            credentials().package_name
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let summary = adapter.check_credentials(&credentials()).await.unwrap();
    assert!(summary.contains(&credentials().package_name), "{summary}");
}

#[tokio::test]
async fn check_credentials_reports_missing_access_and_unknown_packages() {
    for (status, needle) in [(403, "rejected"), (404, "does not know the package")] {
        let (server, adapter) = setup().await;
        Mock::given(method("GET"))
            .and(path_regex("/voidedpurchases$"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let err = adapter.check_credentials(&credentials()).await.unwrap_err();
        assert!(
            matches!(&err, AdapterError::Credentials(m) if m.contains(needle)),
            "{status}: {err:?}"
        );
    }
}
