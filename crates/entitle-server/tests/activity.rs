mod common;

use axum::http::{Method, StatusCode};
use chrono::{Duration, Utc};
use common::TestApp;
use entitle_core::testing::sub;
use entitle_core::{CustomerId, Environment, Money, Store};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

impl TestApp {
    async fn activity(&self, query: &str) -> (StatusCode, Value) {
        self.call(
            Method::GET,
            &format!("/admin/v1/projects/{}/activity?{query}", self.project),
            Some(common::ADMIN),
            None,
        )
        .await
    }
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn activity_lists_newest_transitions_per_environment(db: PgPool) {
    let app = TestApp::new(db).await;
    let now = Utc::now();
    let day = Duration::days(1);
    let info = app.customer("alice").await;
    let alice = CustomerId(info["customer_id"].as_str().unwrap().parse().unwrap());

    let mut prod = sub(
        Store::AppStore,
        "pro_monthly",
        now - day,
        Some(now + day * 29),
    );
    prod.customer_ref = Some(alice);
    prod.price = Some(Money {
        amount_minor: 999,
        currency: "EUR".into(),
    });
    assert_eq!(app.notify("apple", "p1", vec![prod]).await, StatusCode::OK);

    let mut first = sub(
        Store::AppStore,
        "pro_monthly",
        now,
        Some(now + Duration::minutes(5)),
    );
    first.environment = Environment::Sandbox;
    first.original_transaction_id = "sandbox".into();
    first.customer_ref = Some(alice);
    let mut renewal = first.clone();
    renewal.store_transaction_id = "sandbox-renewal".into();
    renewal.purchased_at = now + Duration::minutes(5);
    renewal.expires_at = Some(now + Duration::minutes(10));
    renewal.as_of = first.as_of + Duration::minutes(5);
    assert_eq!(app.notify("apple", "s1", vec![first]).await, StatusCode::OK);
    assert_eq!(
        app.notify("apple", "s2", vec![renewal]).await,
        StatusCode::OK
    );

    let (s, rows) = app.activity("environment=sandbox").await;
    assert_eq!(s, StatusCode::OK, "{rows}");
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["events"], serde_json::json!(["renewal"]));
    assert_eq!(rows[1]["events"], serde_json::json!(["initial_purchase"]));
    assert_eq!(rows[0]["store"], "app_store");
    assert_eq!(rows[0]["app_user_id"], "alice");
    assert_eq!(rows[0]["price"], Value::Null);

    let (_, rows) = app.activity("environment=production").await;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["price"]["amount_minor"], 999);
    assert_eq!(rows[0]["price"]["currency"], "EUR");

    let (_, rows) = app.activity("environment=sandbox&limit=1").await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["events"], serde_json::json!(["renewal"]));

    // Without an environment: both, newest first.
    let (_, rows) = app.activity("").await;
    let envs: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["environment"].as_str().unwrap())
        .collect();
    assert_eq!(envs, ["sandbox", "sandbox", "production"]);

    // One customer's history.
    app.customer("bob").await;
    let (s, rows) = app.activity("app_user_id=alice").await;
    assert_eq!(s, StatusCode::OK, "{rows}");
    assert_eq!(rows.as_array().unwrap().len(), 3);
    let (_, rows) = app.activity("app_user_id=bob").await;
    assert_eq!(rows.as_array().unwrap().len(), 0);
    let (s, _) = app.activity("app_user_id=nobody").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn activity_rejects_bad_parameters(db: PgPool) {
    let app = TestApp::new(db).await;
    for query in [
        "environment=staging",
        "environment=sandbox&limit=0",
        "environment=sandbox&limit=201",
    ] {
        let (s, body) = app.activity(query).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{query}: {body}");
    }
    let (s, _) = app
        .call(
            Method::GET,
            &format!(
                "/admin/v1/projects/{}/activity?environment=sandbox",
                Uuid::new_v4()
            ),
            Some(common::ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app
        .call(
            Method::GET,
            &format!(
                "/admin/v1/projects/{}/activity?environment=sandbox",
                app.project
            ),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
