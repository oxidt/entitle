mod common;

use axum::http::{Method, StatusCode};
use chrono::{Duration, Utc};
use common::TestApp;
use entitle_core::testing::sub;
use entitle_core::{CustomerId, Environment, Store};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn sandbox_purchases_grant_nothing_when_switched_off(db: PgPool) {
    let app = TestApp::new(db).await;
    let alice = app.customer("alice").await;
    let now = Utc::now();
    let mut s = sub(
        Store::AppStore,
        "pro_monthly",
        now - Duration::days(1),
        Some(now + Duration::days(30)),
    );
    s.environment = Environment::Sandbox;
    s.customer_ref = Some(CustomerId(
        alice["customer_id"].as_str().unwrap().parse().unwrap(),
    ));
    assert_eq!(app.notify("apple", "e1", vec![s]).await, StatusCode::OK);
    // Default: sandbox counts (development projects).
    assert!(app.customer("alice").await["entitlements"]["pro"].is_object());

    let (s, project) = app
        .admin(
            Method::PATCH,
            &format!("/admin/v1/projects/{}", app.project),
            json!({ "sandbox_grants_entitlements": false }),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(project["sandbox_grants_entitlements"], false);

    let info = app.customer("alice").await;
    assert_eq!(info["entitlements"], json!({}));
    // Still recorded, just not granting.
    assert_eq!(info["purchases"][0]["environment"], "sandbox");
}
