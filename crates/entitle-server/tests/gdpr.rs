mod common;

use axum::http::{Method, StatusCode};
use chrono::{Duration, Utc};
use common::TestApp;
use entitle_core::testing::sub;
use entitle_core::{CustomerId, Store};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn export_then_erase(db: PgPool) {
    let app = TestApp::new(db).await;
    app.admin(
        Method::POST,
        &format!("/admin/v1/projects/{}/webhooks", app.project),
        json!({"url": "http://127.0.0.1:9/hook"}),
    )
    .await;
    let info = app.customer("alice").await;
    let alice = CustomerId(info["customer_id"].as_str().unwrap().parse().unwrap());
    let now = Utc::now();
    let mut s = sub(
        Store::Creem,
        "prod_pro",
        now,
        Some(now + Duration::days(30)),
    );
    s.customer_ref = Some(alice);
    s.store_customer_id = Some("cust_9".into());
    app.notify("creem", "evt_1", vec![s.clone()]).await;
    app.secret(
        Method::POST,
        "/customers/alice/entitlements/pro/grant",
        None,
    )
    .await;

    let (st, export) = app
        .secret(Method::GET, "/customers/alice/export", None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(export["aliases"], json!(["alice"]));
    assert_eq!(export["purchases"][0]["store_customer_id"], "cust_9");
    assert_eq!(export["promotional_grants"].as_array().unwrap().len(), 1);
    assert_eq!(export["webhook_deliveries"].as_array().unwrap().len(), 1);
    assert!(export["entitlements"]["pro"].is_object());

    let (st, _) = app.secret(Method::DELETE, "/customers/alice", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    for table in [
        "customers",
        "customer_aliases",
        "account_tokens",
        "purchases",
        "promotional_grants",
        "outbox",
    ] {
        assert_eq!(
            app.count(&format!("SELECT count(*) FROM {table}")).await,
            0,
            "{table} not erased"
        );
    }
    let (st, _) = app.secret(Method::DELETE, "/customers/alice", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = app
        .secret(Method::GET, "/customers/alice/export", None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // A later store notification with the erased token is stored unlinked.
    s.as_of = now + Duration::minutes(1);
    app.notify("creem", "evt_2", vec![s]).await;
    assert_eq!(
        app.count("SELECT count(*) FROM purchases WHERE customer_id IS NULL")
            .await,
        1
    );
    let fresh = app.customer("alice").await;
    assert_ne!(fresh["customer_id"], info["customer_id"]);
    assert_eq!(fresh["entitlements"], json!({}));
}
