mod common;

use common::{BUNDLE_ID, TestCa, credentials};
use entitle_core::{AdapterError, Environment, PurchaseKind, PurchaseStatus, StoreAdapter};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

macro_rules! fixture {
    ($name:literal) => {
        serde_json::from_slice::<Value>(include_bytes!(concat!("fixtures/", $name, ".json")))
            .unwrap()
    };
}

const NOT_FOUND: &str = r#"{"errorCode": 4040010, "errorMessage": "Transaction id not found."}"#;

/// Transaction and renewal info of a fixture, as the API returns them in `environment`.
fn signed_data(fixture: Value, environment: &str) -> (Value, Value) {
    let mut tx = fixture["data"]["transaction"].clone();
    let mut renewal = fixture["data"]["renewal"].clone();
    tx["environment"] = environment.into();
    renewal["environment"] = environment.into();
    (tx, renewal)
}

async fn mount_transaction(server: &MockServer, ca: &TestCa, id: &str, tx: &Value) {
    Mock::given(method("GET"))
        .and(path(format!("/inApps/v1/transactions/{id}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"signedTransactionInfo": ca.sign(tx)})),
        )
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_not_found(server: &MockServer) {
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_raw(NOT_FOUND, "application/json"))
        .mount(server)
        .await;
}

fn jwt_part(request: &Request, index: usize) -> Value {
    use base64::Engine;
    let auth = request.headers["authorization"].to_str().unwrap();
    let token = auth.strip_prefix("Bearer ").unwrap();
    let part = token.split('.').nth(index).unwrap();
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part)
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn subscription_state_from_production() {
    let ca = TestCa::new();
    let production = MockServer::start().await;
    let (tx, renewal) = signed_data(fixture!("did_change_renewal_status"), "Production");
    mount_transaction(&production, &ca, "2000000100000002", &tx).await;
    Mock::given(method("GET"))
        .and(path("/inApps/v1/subscriptions/2000000100000001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "environment": "Production",
            "bundleId": BUNDLE_ID,
            "appAppleId": common::APP_APPLE_ID,
            "data": [{
                "subscriptionGroupIdentifier": "21000001",
                "lastTransactions": [{
                    "status": 1,
                    "originalTransactionId": "2000000100000001",
                    "signedTransactionInfo": ca.sign(&tx),
                    "signedRenewalInfo": ca.sign(&renewal),
                }]
            }]
        })))
        .expect(1)
        .mount(&production)
        .await;

    let snapshots = ca
        .adapter(&production.uri(), "http://127.0.0.1:9")
        .fetch_purchases(&credentials(), "2000000100000002")
        .await
        .unwrap();
    assert_eq!(snapshots.len(), 1);
    let s = &snapshots[0];
    assert_eq!(s.environment, Environment::Production);
    assert_eq!(s.kind, PurchaseKind::Subscription);
    assert_eq!(s.status, PurchaseStatus::Cancelled);
    assert_eq!(s.original_transaction_id, "2000000100000001");
    assert_eq!(s.as_of.timestamp_millis(), 1768435200000);

    // App Store Server API JWT.
    let requests = production.received_requests().await.unwrap();
    let credentials = credentials();
    let header = jwt_part(&requests[0], 0);
    let claims = jwt_part(&requests[0], 1);
    assert_eq!(header["alg"], "ES256");
    assert_eq!(header["kid"], credentials.key_id);
    assert_eq!(claims["iss"], credentials.issuer_id);
    assert_eq!(claims["aud"], "appstoreconnect-v1");
    assert_eq!(claims["bid"], BUNDLE_ID);
    assert!(claims["exp"].as_i64() > claims["iat"].as_i64());
}

#[tokio::test]
async fn falls_back_to_sandbox_when_production_does_not_know_the_transaction() {
    let ca = TestCa::new();
    let production = MockServer::start().await;
    let sandbox = MockServer::start().await;
    mount_not_found(&production).await;
    let (tx, _) = signed_data(fixture!("one_time_charge"), "Sandbox");
    mount_transaction(&sandbox, &ca, "2000000200000001", &tx).await;

    let snapshots = ca
        .adapter(&production.uri(), &sandbox.uri())
        .fetch_purchases(&credentials(), "2000000200000001")
        .await
        .unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].environment, Environment::Sandbox);
    assert_eq!(snapshots[0].kind, PurchaseKind::OneTime);
    assert_eq!(snapshots[0].status, PurchaseStatus::Active);
    assert_eq!(production.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn falls_back_to_sandbox_when_production_rejects_an_unreleased_app() {
    let ca = TestCa::new();
    let production = MockServer::start().await;
    let sandbox = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&production)
        .await;
    let (tx, _) = signed_data(fixture!("one_time_charge"), "Sandbox");
    mount_transaction(&sandbox, &ca, "2000000200000001", &tx).await;

    let snapshots = ca
        .adapter(&production.uri(), &sandbox.uri())
        .fetch_purchases(&credentials(), "2000000200000001")
        .await
        .unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].environment, Environment::Sandbox);
}

#[tokio::test]
async fn family_shared_revocation_from_the_api_is_revoked() {
    for (ownership, expected) in [
        ("FAMILY_SHARED", PurchaseStatus::Revoked),
        ("PURCHASED", PurchaseStatus::Refunded),
    ] {
        let ca = TestCa::new();
        let production = MockServer::start().await;
        let (mut tx, _) = signed_data(fixture!("one_time_charge"), "Production");
        tx["inAppOwnershipType"] = ownership.into();
        tx["revocationDate"] = 1768435200000i64.into();
        tx["revocationReason"] = 0.into();
        mount_transaction(&production, &ca, "2000000200000001", &tx).await;
        let snapshots = ca
            .adapter(&production.uri(), "http://127.0.0.1:9")
            .fetch_purchases(&credentials(), "2000000200000001")
            .await
            .unwrap();
        assert_eq!(snapshots[0].status, expected, "{ownership}");
    }
}

#[tokio::test]
async fn only_subscriptions_and_non_consumables_are_supported() {
    for product_type in [
        Some("Consumable"),
        Some("Non-Renewing Subscription"),
        Some("Some Future Type"),
        None,
    ] {
        let ca = TestCa::new();
        let production = MockServer::start().await;
        let (mut tx, _) = signed_data(fixture!("one_time_charge"), "Production");
        match product_type {
            Some(t) => tx["type"] = t.into(),
            None => drop(tx.as_object_mut().unwrap().remove("type")),
        }
        mount_transaction(&production, &ca, "2000000200000001", &tx).await;
        let err = ca
            .adapter(&production.uri(), "http://127.0.0.1:9")
            .fetch_purchases(&credentials(), "2000000200000001")
            .await
            .unwrap_err();
        let AdapterError::Unsupported(message) = err else {
            panic!("{product_type:?}: {err:?}");
        };
        assert!(
            message.contains(product_type.unwrap_or("missing")),
            "{message}"
        );
    }
}

#[tokio::test]
async fn unknown_everywhere_is_not_found() {
    let ca = TestCa::new();
    let production = MockServer::start().await;
    let sandbox = MockServer::start().await;
    mount_not_found(&production).await;
    mount_not_found(&sandbox).await;
    let err = ca
        .adapter(&production.uri(), &sandbox.uri())
        .fetch_purchases(&credentials(), "2000000999999999")
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::NotFound), "{err:?}");
}

#[tokio::test]
async fn api_errors_are_mapped() {
    let ca = TestCa::new();
    for (status, expected) in [(401, "credentials"), (500, "upstream")] {
        // A 401 only counts as a credentials error once sandbox rejects the key too.
        let production = MockServer::start().await;
        let sandbox = MockServer::start().await;
        for server in [&production, &sandbox] {
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(server)
                .await;
        }
        let err = ca
            .adapter(&production.uri(), &sandbox.uri())
            .fetch_purchases(&credentials(), "2000000100000002")
            .await
            .unwrap_err();
        let ok = match expected {
            "credentials" => matches!(err, AdapterError::Credentials(_)),
            _ => matches!(err, AdapterError::Upstream(_)),
        };
        assert!(ok, "{status}: {err:?}");
    }
}

#[tokio::test]
async fn unverifiable_api_response_is_an_upstream_error() {
    let ca = TestCa::new();
    let production = MockServer::start().await;
    let (tx, _) = signed_data(fixture!("one_time_charge"), "Production");
    mount_transaction(&production, &TestCa::new(), "2000000200000001", &tx).await;
    let err = ca
        .adapter(&production.uri(), "http://127.0.0.1:9")
        .fetch_purchases(&credentials(), "2000000200000001")
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::Upstream(_)), "{err:?}");
}

#[tokio::test]
async fn bad_input_fails_before_any_request() {
    let ca = TestCa::new();
    let production = MockServer::start().await;
    let adapter = ca.adapter(&production.uri(), &production.uri());

    for id in ["", "abc", "1/../../v2", "1?x=1"] {
        let err = adapter
            .fetch_purchases(&credentials(), id)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::NotFound), "{id}: {err:?}");
    }

    let mut bad_key = credentials();
    bad_key.private_key = "not a key".into();
    let err = adapter
        .fetch_purchases(&bad_key, "2000000100000002")
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");

    assert!(production.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn check_credentials_requests_a_sandbox_test_notification() {
    let ca = TestCa::new();
    let production = MockServer::start().await;
    let sandbox = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/inApps/v1/notifications/test"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"testNotificationToken": "t"})),
        )
        .expect(1)
        .mount(&sandbox)
        .await;
    let adapter = ca.adapter(&production.uri(), &sandbox.uri());
    let summary = adapter.check_credentials(&credentials()).await.unwrap();
    assert!(summary.contains("test notification"), "{summary}");
    let request = &sandbox.received_requests().await.unwrap()[0];
    assert_eq!(jwt_part(request, 1)["bid"], BUNDLE_ID);
}

#[tokio::test]
async fn check_credentials_reports_a_rejected_key() {
    let ca = TestCa::new();
    let sandbox = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&sandbox)
        .await;
    let adapter = ca.adapter(&sandbox.uri(), &sandbox.uri());
    let err = adapter.check_credentials(&credentials()).await.unwrap_err();
    assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");

    let mut bad = credentials();
    bad.private_key = "not a key".into();
    let err = adapter.check_credentials(&bad).await.unwrap_err();
    assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");
}
