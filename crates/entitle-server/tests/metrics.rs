mod common;

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Duration, Utc};
use common::TestApp;
use entitle_core::testing::sub;
use entitle_core::{CustomerId, Environment, Money, PurchaseSnapshot, Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

fn snapshot(
    store: Store,
    product: &str,
    owner: CustomerId,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    price: (i64, &str),
) -> PurchaseSnapshot {
    let mut s = sub(store, product, start, Some(end));
    s.customer_ref = Some(owner);
    s.price = Some(Money {
        amount_minor: price.0,
        currency: price.1.into(),
    });
    s
}

fn point(m: &Value, day: DateTime<Utc>) -> &Value {
    let start = day.date_naive().to_string();
    m["points"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["start"] == start)
        .unwrap_or_else(|| panic!("no point {start}"))
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn metrics_from_recorded_history(db: PgPool) {
    let app = TestApp::new(db).await;
    let now = Utc::now();
    let day = Duration::days(1);
    let cid = |info: Value| CustomerId(info["customer_id"].as_str().unwrap().parse().unwrap());
    let alice = cid(app.customer("alice").await);
    let carol = cid(app.customer("carol").await);

    // Alice: monthly $9.99, renewed once.
    let first = snapshot(
        Store::AppStore,
        "pro_monthly",
        alice,
        now - day * 50,
        now - day * 20,
        (999, "USD"),
    );
    let mut renewal = snapshot(
        Store::AppStore,
        "pro_monthly",
        alice,
        now - day * 20,
        now + day * 10,
        (999, "USD"),
    );
    renewal.store_transaction_id = "renewal".into();
    assert_eq!(app.notify("apple", "a1", vec![first]).await, StatusCode::OK);
    assert_eq!(
        app.notify("apple", "a2", vec![renewal]).await,
        StatusCode::OK
    );
    // Carol: a currency without an ECB rate.
    let aed = snapshot(
        Store::Creem,
        "prod_pro",
        carol,
        now - day * 5,
        now + day * 25,
        (3670, "AED"),
    );
    assert_eq!(app.notify("creem", "c1", vec![aed]).await, StatusCode::OK);
    // Sandbox purchases are excluded.
    let mut sandbox = snapshot(
        Store::AppStore,
        "pro_monthly",
        carol,
        now - day * 3,
        now + day * 27,
        (999, "USD"),
    );
    sandbox.environment = Environment::Sandbox;
    sandbox.original_transaction_id = "sandbox".into();
    assert_eq!(
        app.notify("apple", "s1", vec![sandbox]).await,
        StatusCode::OK
    );

    let from = (now - day * 60).date_naive();
    let to = (now + day).date_naive();
    let (s, m) = app
        .call(
            Method::GET,
            &format!(
                "/admin/v1/projects/{}/metrics?from={from}&to={to}&interval=day",
                app.project
            ),
            Some(common::ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{m}");
    assert_eq!(m["currency"], "USD");
    assert!(m["history_start"].is_string(), "{m}");
    assert_eq!(m["points"].as_array().unwrap().len(), 61);

    let bought = point(&m, now - day * 50);
    assert_eq!(bought["new_mrr_minor"], 999);
    assert_eq!(bought["revenue_minor"], 999);
    assert_eq!(bought["transactions"], 1);
    assert_eq!(bought["new_trials"], 0);
    assert_eq!(bought["churn_rate"], Value::Null);

    let renewed = point(&m, now - day * 20);
    assert_eq!(renewed["revenue_minor"], 999);
    assert_eq!(renewed["churned_mrr_minor"], 0);
    assert_eq!(renewed["churn_rate"], 0.0);

    let today = point(&m, now);
    assert_eq!(today["mrr_minor"], 999);
    assert_eq!(today["active_subscriptions"], 2);
    assert_eq!(today["active_trials"], 0);
    let total = |field: &str| {
        m["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p[field].as_i64().unwrap())
            .sum::<i64>()
    };
    assert_eq!(total("revenue_minor"), 1998);
    assert_eq!(total("transactions"), 3);
    assert_eq!(total("new_customers"), 2);
    assert_eq!(total("new_mrr_minor"), 999);
    assert_eq!(
        m["unconverted"],
        json!([{"currency": "AED", "revenue_minor": 3670, "mrr_minor": 3670}])
    );
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn metrics_rejects_bad_parameters(db: PgPool) {
    let app = TestApp::new(db).await;
    let get = |query: String, project: Uuid| {
        let app = &app;
        async move {
            app.call(
                Method::GET,
                &format!("/admin/v1/projects/{project}/metrics{query}"),
                Some(common::ADMIN),
                None,
            )
            .await
        }
    };
    for query in [
        "",
        "?from=2026-01-01&to=2026-02-01",
        "?from=2026-01-01&to=2026-02-01&interval=year",
        "?from=2026-13-01&to=2026-02-01&interval=day",
        "?from=2026-02-01&to=2026-02-01&interval=day",
        "?from=2026-03-01&to=2026-02-01&interval=month",
        "?from=2000-01-01&to=2026-01-01&interval=day",
    ] {
        let (s, body) = get(query.into(), app.project).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["error"]["code"], "bad_request", "{query}");
    }
    let (s, body) = get(
        "?from=2000-01-01&to=2026-01-01&interval=month".into(),
        app.project,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = get(
        "?from=2026-01-01&to=2026-02-01&interval=week".into(),
        Uuid::new_v4(),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    let (s, _) = app
        .call(
            Method::GET,
            &format!(
                "/admin/v1/projects/{}/metrics?from=2026-01-01&to=2026-02-01&interval=week",
                app.project
            ),
            Some(&app.sk),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
