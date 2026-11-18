use chrono::{DateTime, Utc};
use entitle_core::{
    AdapterError, CheckoutAdapter, CheckoutRequest, CreemCredentials, CustomerId, Environment,
    Money, PeriodType, PurchaseKind, PurchaseSnapshot, PurchaseStatus, Store, StoreAdapter,
};
use entitle_creem::CreemAdapter;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_KEY: &str = "creem_test_key";
const CUSTOMER: &str = "7f0c3a52-4f0e-4b8e-9c4a-2f1d8a7e6b10";
const SUB: &str = "sub_6pC2lNB6joCRQIZ1aMrTpi";
const CUST: &str = "cust_1OcIK1GEuVvXZwD19tjq2z";

macro_rules! fixture {
    ($name:literal) => {
        serde_json::from_slice::<Value>(include_bytes!(concat!("fixtures/", $name, ".json")))
            .unwrap()
    };
}

fn credentials() -> CreemCredentials {
    CreemCredentials {
        api_key: API_KEY.into(),
        webhook_secret: "whsec_test".into(),
        test_mode: true,
    }
}

fn adapter(server: &MockServer) -> CreemAdapter {
    CreemAdapter::with_base_url(format!("{}/v1", server.uri()))
}

fn t(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn customer() -> CustomerId {
    CustomerId(CUSTOMER.parse().unwrap())
}

fn eur(amount_minor: i64) -> Option<Money> {
    Some(Money {
        amount_minor,
        currency: "EUR".into(),
    })
}

async fn mount_get(server: &MockServer, route: &str, param: &str, id: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(route))
        .and(query_param(param, id))
        .and(header("x-api-key", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(server)
        .await;
}

async fn fetch_one(server: &MockServer, id: &str) -> PurchaseSnapshot {
    let mut snapshots = adapter(server)
        .fetch_purchases(&credentials(), id)
        .await
        .unwrap();
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    snapshots.remove(0)
}

#[tokio::test]
async fn fetch_subscription_by_id() {
    let server = MockServer::start().await;
    let body = fixture!("api_subscription");
    mount_get(&server, "/v1/subscriptions", "subscription_id", SUB, body).await;
    let before = Utc::now();
    let s = fetch_one(&server, SUB).await;
    assert!(s.as_of >= before);
    assert_eq!(
        s,
        PurchaseSnapshot {
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
            customer_ref: Some(customer()),
            store_customer_id: Some(CUST.into()),
            as_of: s.as_of,
        }
    );
}

#[tokio::test]
async fn fetch_subscription_checkout() {
    let server = MockServer::start().await;
    let body = fixture!("api_checkout");
    let id = "ch_4l0N34kxo16AhRKUHFUuXr";
    mount_get(&server, "/v1/checkouts", "checkout_id", id, body).await;
    let s = fetch_one(&server, id).await;
    assert_eq!(s.original_transaction_id, SUB);
    assert_eq!(s.kind, PurchaseKind::Subscription);
    assert_eq!(s.price, eur(999));
    assert_eq!(s.customer_ref, Some(customer()));
}

/// The checkout as `checkout.completed` carries it; the API returns the same object.
fn one_time_checkout() -> Value {
    fixture!("checkout_completed_onetime")["object"].clone()
}

#[tokio::test]
async fn fetch_one_time_checkout_matches_the_webhook() {
    let server = MockServer::start().await;
    let id = "ch_7bQm2Xc9LpR4tZs1VwYe3N";
    mount_get(
        &server,
        "/v1/checkouts",
        "checkout_id",
        id,
        one_time_checkout(),
    )
    .await;
    let s = fetch_one(&server, id).await;
    assert_eq!(
        s,
        PurchaseSnapshot {
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
            customer_ref: Some(customer()),
            store_customer_id: Some(CUST.into()),
            as_of: s.as_of,
        }
    );
}

#[tokio::test]
async fn one_time_checkout_without_order_currency_uses_the_product_currency() {
    let server = MockServer::start().await;
    let id = "ch_7bQm2Xc9LpR4tZs1VwYe3N";
    let mut body = one_time_checkout();
    body["order"].as_object_mut().unwrap().remove("currency");
    mount_get(&server, "/v1/checkouts", "checkout_id", id, body).await;
    assert_eq!(fetch_one(&server, id).await.price, eur(4999));
}

#[tokio::test]
async fn unpaid_or_incomplete_checkouts_have_no_purchases() {
    let server = MockServer::start().await;
    let mut unpaid = one_time_checkout();
    unpaid["order"]["status"] = "pending".into();
    mount_get(&server, "/v1/checkouts", "checkout_id", "ch_unpaid", unpaid).await;
    let mut open = one_time_checkout();
    open["status"] = "pending".into();
    mount_get(&server, "/v1/checkouts", "checkout_id", "ch_open", open).await;
    for id in ["ch_unpaid", "ch_open"] {
        let snapshots = adapter(&server)
            .fetch_purchases(&credentials(), id)
            .await
            .unwrap();
        assert_eq!(snapshots, Vec::new(), "{id}");
    }
}

#[tokio::test]
async fn create_checkout_sends_the_customer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/checkouts"))
        .and(header("x-api-key", API_KEY))
        .and(body_json(json!({
            "product_id": "prod_monthly",
            "request_id": CUSTOMER,
            "success_url": "https://example.com/done",
            "metadata": {"source": "web", "entitle_customer_id": CUSTOMER},
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "ch_new",
            "checkout_url": "https://checkout.creem.io/ch_new",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let session = adapter(&server)
        .create_checkout(
            &credentials(),
            &CheckoutRequest {
                product_id: "prod_monthly".into(),
                customer: customer(),
                success_url: Some("https://example.com/done".into()),
                metadata: [("source".to_owned(), "web".to_owned())].into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(session.id, "ch_new");
    assert_eq!(session.url, "https://checkout.creem.io/ch_new");
}

#[tokio::test]
async fn portal_url_for_a_customer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/customers/billing"))
        .and(header("x-api-key", API_KEY))
        .and(body_json(json!({"customer_id": CUST})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "customer_portal_link": "https://creem.io/portal/abc",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let url = adapter(&server)
        .portal_url(&credentials(), CUST)
        .await
        .unwrap();
    assert_eq!(url, "https://creem.io/portal/abc");
}

#[tokio::test]
async fn check_credentials_accepts_an_authorized_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/products/prod_entitle_check"))
        .and(header("x-api-key", API_KEY))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let summary = adapter(&server)
        .check_credentials(&credentials())
        .await
        .unwrap();
    assert!(summary.contains("accepted"), "{summary}");
}

#[tokio::test]
async fn check_credentials_reports_a_rejected_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let err = adapter(&server)
        .check_credentials(&credentials())
        .await
        .unwrap_err();
    assert!(matches!(err, AdapterError::Credentials(_)), "{err:?}");
}
