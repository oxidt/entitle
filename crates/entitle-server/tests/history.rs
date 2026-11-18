mod common;

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Duration, Utc};
use common::TestApp;
use entitle_core::testing::sub;
use entitle_core::{CustomerId, PurchaseSnapshot, PurchaseStatus, Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

fn cid(info: &Value) -> CustomerId {
    CustomerId(info["customer_id"].as_str().unwrap().parse().unwrap())
}

fn apple(owner: Option<CustomerId>, now: DateTime<Utc>, as_of: DateTime<Utc>) -> PurchaseSnapshot {
    let mut s = sub(
        Store::AppStore,
        "pro_monthly",
        now - Duration::days(1),
        Some(now + Duration::days(30)),
    );
    s.original_transaction_id = "1000".into();
    s.customer_ref = owner;
    s.as_of = as_of;
    s
}

/// `(customer_id, status, events)` of every history row, oldest first.
async fn history(app: &TestApp) -> Vec<(Option<Uuid>, String, Vec<String>)> {
    sqlx::query_as("SELECT customer_id, status, events FROM purchase_history ORDER BY id")
        .fetch_all(&app.db)
        .await
        .unwrap()
}

fn events(rows: &[(Option<Uuid>, String, Vec<String>)]) -> Vec<Vec<String>> {
    rows.iter().map(|(_, _, e)| e.clone()).collect()
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn unlinked_purchase_is_recorded_without_webhook_endpoints(db: PgPool) {
    let app = TestApp::new(db).await;
    let now = Utc::now();
    app.notify("apple", "n1", vec![apple(None, now, now)]).await;
    assert_eq!(app.count("SELECT count(*) FROM outbox").await, 0);
    assert_eq!(
        history(&app).await,
        [(None, "active".into(), vec!["initial_purchase".into()])]
    );
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn stale_and_identical_snapshots_write_nothing(db: PgPool) {
    let app = TestApp::new(db).await;
    let alice = cid(&app.customer("alice").await);
    let now = Utc::now();
    app.notify("apple", "n1", vec![apple(Some(alice), now, now)])
        .await;
    assert_eq!(history(&app).await.len(), 1);

    // Stale: older as_of with a different state.
    let mut stale = apple(Some(alice), now, now - Duration::minutes(1));
    stale.status = PurchaseStatus::Expired;
    app.notify("apple", "n2", vec![stale]).await;
    // Identical re-sync, newer as_of: by notification and by restore.
    app.notify(
        "apple",
        "n3",
        vec![apple(Some(alice), now, now + Duration::minutes(1))],
    )
    .await;
    app.fakes.apple.fetch.lock().unwrap().insert(
        "tx".into(),
        vec![apple(Some(alice), now, now + Duration::minutes(2))],
    );
    let (s, _) = app
        .sdk(
            Method::POST,
            "/customers/alice/sync",
            Some(json!({"store": "app_store", "id": "tx"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(history(&app).await.len(), 1);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn transitions_record_their_events(db: PgPool) {
    let app = TestApp::new(db).await;
    let alice = cid(&app.customer("alice").await);
    let now = Utc::now();
    let mut s = apple(Some(alice), now, now);
    app.notify("apple", "n1", vec![s.clone()]).await;

    s.expires_at = Some(now + Duration::days(60));
    s.store_transaction_id = "renewed".into();
    s.as_of = now + Duration::minutes(1);
    app.notify("apple", "n2", vec![s.clone()]).await;

    s.status = PurchaseStatus::Expired;
    s.auto_renew = false;
    s.as_of = now + Duration::minutes(2);
    app.notify("apple", "n3", vec![s.clone()]).await;

    s.status = PurchaseStatus::Refunded;
    s.as_of = now + Duration::minutes(3);
    app.notify("apple", "n4", vec![s]).await;

    let rows = history(&app).await;
    assert!(rows.iter().all(|(c, _, _)| *c == Some(alice.0)));
    assert_eq!(
        events(&rows),
        [
            vec!["initial_purchase"],
            vec!["renewal"],
            vec!["expiration"],
            vec!["refund"],
        ]
    );
    let (non_decreasing,): (bool,) = sqlx::query_as(
        "SELECT bool_and(as_of >= prev) FROM (
            SELECT as_of, lag(as_of) OVER (ORDER BY id) AS prev FROM purchase_history) t",
    )
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert!(non_decreasing);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn transfer_records_new_owner(db: PgPool) {
    let app = TestApp::new(db).await;
    let alice = cid(&app.customer("alice").await);
    let bob = cid(&app.customer("bob").await);
    let now = Utc::now();
    app.notify("apple", "n1", vec![apple(None, now, now)]).await;
    app.fakes
        .apple
        .fetch
        .lock()
        .unwrap()
        .insert("tx".into(), vec![apple(None, now, now)]);
    for who in ["alice", "bob"] {
        let (s, _) = app
            .sdk(
                Method::POST,
                &format!("/customers/{who}/sync"),
                Some(json!({"store": "app_store", "id": "tx"})),
            )
            .await;
        assert_eq!(s, StatusCode::OK);
    }
    let owners: Vec<_> = history(&app).await.into_iter().map(|(c, _, _)| c).collect();
    assert_eq!(owners, [None, Some(alice.0), Some(bob.0)]);
    // Linking is not a new purchase: only the first sighting counts.
    assert_eq!(
        events(&history(&app).await),
        [vec!["initial_purchase".to_owned()], vec![], vec![]]
    );
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn customer_deletion_unlinks_but_keeps_history(db: PgPool) {
    let app = TestApp::new(db).await;
    let alice = cid(&app.customer("alice").await);
    let now = Utc::now();
    app.notify("apple", "n1", vec![apple(Some(alice), now, now)])
        .await;

    let (_, export) = app
        .secret(Method::GET, "/customers/alice/export", None)
        .await;
    assert_eq!(
        export["purchase_history"][0]["events"],
        json!(["initial_purchase"])
    );

    let (s, _) = app.secret(Method::DELETE, "/customers/alice", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(app.count("SELECT count(*) FROM purchases").await, 0);
    assert_eq!(
        history(&app).await,
        [(None, "active".into(), vec!["initial_purchase".into()])]
    );
}
