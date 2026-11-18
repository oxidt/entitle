mod common;

use axum::http::{Method, StatusCode};
use chrono::{Duration, Utc};
use common::TestApp;
use entitle_core::testing::sub;
use entitle_core::{PurchaseKind, PurchaseSnapshot, PurchaseStatus, Store};
use serde_json::json;
use sqlx::PgPool;

fn one_time(store: Store, product_id: &str, id: &str, days_ago: i64) -> PurchaseSnapshot {
    let mut s = sub(
        store,
        product_id,
        Utc::now() - Duration::days(days_ago),
        None,
    );
    s.original_transaction_id = id.into();
    s.kind = PurchaseKind::OneTime;
    s.auto_renew = false;
    s
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn lists_active_store_lifetime_purchases_per_product(db: PgPool) {
    let app = TestApp::new(db).await;
    let now = Utc::now();
    let mut refunded = one_time(Store::AppStore, "pro_monthly", "a3", 1);
    refunded.status = PurchaseStatus::Refunded;
    app.notify(
        "apple",
        "n1",
        vec![
            // Granting `pro` (attached in TestApp) and lifetime: the suspicious kind.
            one_time(Store::AppStore, "pro_monthly", "a1", 10),
            one_time(Store::AppStore, "pro_monthly", "a2", 2),
            refunded,
            one_time(Store::AppStore, "coins_100", "a4", 3),
            // Subscriptions are not listed.
            sub(
                Store::AppStore,
                "pro_monthly",
                now,
                Some(now + Duration::days(30)),
            ),
        ],
    )
    .await;
    app.notify(
        "google",
        "g1",
        vec![one_time(Store::PlayStore, "pro_yearly", "g-token", 5)],
    )
    .await;
    let row_count = app.count("SELECT count(*) FROM purchases").await;

    let (s, body) = app
        .call(
            Method::GET,
            &format!("/admin/v1/projects/{}/lifetime-purchases", app.project),
            Some(common::ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let rows: Vec<_> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["store"].as_str().unwrap(),
                r["product_id"].as_str().unwrap(),
                r["environment"].as_str().unwrap(),
                r["entitlements"].clone(),
                r["active_purchases"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("app_store", "coins_100", "production", json!([]), 1),
            ("app_store", "pro_monthly", "production", json!(["pro"]), 2),
            ("play_store", "pro_yearly", "production", json!(["pro"]), 1),
        ]
    );
    let pro = &body[1];
    assert!(pro["first_purchased_at"].as_str() < pro["last_purchased_at"].as_str());
    // Read-only.
    assert_eq!(app.count("SELECT count(*) FROM purchases").await, row_count);
}
