mod common;

use std::collections::BTreeMap;

use axum::http::{Method, StatusCode};
use chrono::{Duration, Utc};
use common::TestApp;
use entitle_core::testing::sub;
use entitle_core::{CustomerId, Environment, PeriodType, PurchaseSnapshot, Store};
use serde_json::Value;
use sqlx::PgPool;

impl TestApp {
    async fn list_customers(&self, query: &str) -> (StatusCode, Value) {
        self.call(
            Method::GET,
            &format!("/admin/v1/projects/{}/customers?{query}", self.project),
            Some(common::ADMIN),
            None,
        )
        .await
    }

    /// `app_user_id` → `status` for the listed customers.
    async fn statuses(&self, query: &str) -> BTreeMap<String, String> {
        let (s, body) = self.list_customers(query).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        body.as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["app_user_id"].as_str().unwrap().to_owned(),
                    c["status"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    async fn buy(&self, app_user_id: &str, mut p: PurchaseSnapshot) {
        let info = self.customer(app_user_id).await;
        p.customer_ref = Some(CustomerId(
            info["customer_id"].as_str().unwrap().parse().unwrap(),
        ));
        let slug = match p.store {
            Store::AppStore => "apple",
            Store::PlayStore => "google",
            Store::Creem => "creem",
        };
        let status = self.notify(slug, app_user_id, vec![p]).await;
        assert_eq!(status, StatusCode::OK);
    }
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn customer_list_classifies_and_filters(db: PgPool) {
    let app = TestApp::new(db).await;
    let now = Utc::now();
    let day = Duration::days(1);

    app.buy(
        "alice",
        sub(
            Store::AppStore,
            "pro_monthly",
            now - day,
            Some(now + day * 29),
        ),
    )
    .await;
    let mut trial = sub(
        Store::PlayStore,
        "pro_trial",
        now - day,
        Some(now + day * 6),
    );
    trial.period_type = PeriodType::Trial;
    app.buy("bob", trial).await;
    app.buy(
        "carol",
        sub(
            Store::AppStore,
            "pro_old",
            now - day * 40,
            Some(now - day * 10),
        ),
    )
    .await;
    app.customer("dave").await;
    let mut sandbox = sub(Store::AppStore, "pro_sandbox", now - day, Some(now + day));
    sandbox.environment = Environment::Sandbox;
    app.buy("erin", sandbox).await;

    assert_eq!(
        app.statuses("").await,
        map(&[
            ("alice", "active"),
            ("bob", "trial"),
            ("carol", "churned"),
            ("dave", "none"),
            ("erin", "active"),
        ])
    );
    assert_eq!(
        app.statuses("status=active").await,
        map(&[("alice", "active"), ("erin", "active")])
    );
    assert_eq!(app.statuses("status=none").await, map(&[("dave", "none")]));
    // Environment and store filters drop customers without a matching purchase.
    assert_eq!(
        app.statuses("environment=production").await,
        map(&[("alice", "active"), ("bob", "trial"), ("carol", "churned")])
    );
    assert_eq!(
        app.statuses("environment=sandbox&store=app_store").await,
        map(&[("erin", "active")])
    );

    let (_, body) = app.list_customers("store=play_store").await;
    assert_eq!(body[0]["stores"], serde_json::json!(["play_store"]));
    assert!(body[0]["customer_id"].is_string());
    assert!(body[0]["first_seen_at"].is_string());
    assert!(body[0]["last_activity_at"].is_string());
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn customer_list_pages_newest_first(db: PgPool) {
    let app = TestApp::new(db).await;
    let now = Utc::now();
    app.customer("first").await;
    app.buy(
        "second",
        sub(Store::AppStore, "a", now, Some(now + Duration::days(1))),
    )
    .await;
    app.customer("third").await;

    let ids = |body: &Value| -> Vec<String> {
        body.as_array()
            .unwrap()
            .iter()
            .map(|c| c["app_user_id"].as_str().unwrap().to_owned())
            .collect()
    };
    let (_, page1) = app.list_customers("limit=2").await;
    let (_, page2) = app.list_customers("limit=2&offset=2").await;
    assert_eq!(ids(&page1), ["third", "second"]);
    assert_eq!(ids(&page2), ["first"]);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn customer_list_rejects_bad_parameters(db: PgPool) {
    let app = TestApp::new(db).await;
    for query in [
        "limit=0",
        "limit=201",
        "offset=-1",
        "status=gone",
        "store=steam",
    ] {
        let (s, body) = app.list_customers(query).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{query}: {body}");
    }
    let (s, _) = app
        .call(
            Method::GET,
            &format!("/admin/v1/projects/{}/customers", uuid::Uuid::new_v4()),
            Some(common::ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
