//! Request budgets on the public-key SDK routes that create customers or call a store.

mod common;

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use common::TestApp;
use entitle_server::rate_limit::SdkBudget;
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

/// `GET /v1/customers/{id}` with `key` from the client at `peer`.
async fn get_customer(
    app: &TestApp,
    key: &str,
    id: &str,
    peer: &str,
) -> (StatusCode, Option<String>, Value) {
    let mut req = Request::get(format!("/v1/customers/{id}"))
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let retry_after = res
        .headers()
        .get(header::RETRY_AFTER)
        .map(|v| v.to_str().unwrap().to_owned());
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, retry_after, serde_json::from_slice(&body).unwrap())
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn public_key_calls_are_budgeted_per_client(db: PgPool) {
    let app = TestApp::with_sdk_budget(db, SdkBudget::per_minute(2, 1000)).await;
    for i in 0..2 {
        let (s, _, _) = get_customer(&app, &app.pk, &format!("u{i}"), "10.0.0.1:1").await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, retry_after, body) = get_customer(&app, &app.pk, "u3", "10.0.0.1:1").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(retry_after.as_deref(), Some("30"));
    assert_eq!(body["error"]["code"], "rate_limited");
    // Refused before the customer was created.
    assert_eq!(
        app.count("SELECT count(*) FROM customer_aliases WHERE app_user_id = 'u3'")
            .await,
        0
    );
    // Other clients, and backends with the secret key, are unaffected.
    let (s, _, _) = get_customer(&app, &app.pk, "u3", "10.0.0.2:1").await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = get_customer(&app, &app.sk, "u4", "10.0.0.1:1").await;
    assert_eq!(s, StatusCode::OK);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn public_key_calls_are_budgeted_per_project(db: PgPool) {
    let app = TestApp::with_sdk_budget(db, SdkBudget::per_minute(1000, 2)).await;
    for peer in ["10.0.0.1:1", "10.0.0.2:1"] {
        let (s, _, _) = get_customer(&app, &app.pk, "u", peer).await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, _, body) = get_customer(&app, &app.pk, "u", "10.0.0.3:1").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{body}");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn sync_and_checkout_are_budgeted(db: PgPool) {
    let app = TestApp::with_sdk_budget(db, SdkBudget::per_minute(1, 1000)).await;
    let (s, _) = app
        .sdk(
            Method::POST,
            "/customers/u/checkout",
            Some(serde_json::json!({"product_id": "prod_pro"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, body) = app
        .sdk(
            Method::POST,
            "/customers/u/sync",
            Some(serde_json::json!({"store": "app_store", "id": "1"})),
        )
        .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{body}");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn sync_of_an_unknown_purchase_creates_no_customer(db: PgPool) {
    let app = TestApp::new(db).await;
    let (s, body) = app
        .sdk(
            Method::POST,
            "/customers/stranger/sync",
            Some(serde_json::json!({"store": "app_store", "id": "404"})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "store_not_found");
    assert_eq!(app.count("SELECT count(*) FROM customers").await, 0);
}
