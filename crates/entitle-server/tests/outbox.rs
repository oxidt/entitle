mod common;

use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::post;
use chrono::{Duration, Utc};
use common::TestApp;
use entitle_core::testing::sub;
use entitle_core::{CustomerId, Store};
use entitle_server::crypto::sign_webhook;
use entitle_server::outbox::{Egress, MAX_ATTEMPTS, deliver_due};
use serde_json::{Value, json};
use sqlx::PgPool;

#[derive(Clone, Default)]
struct Receiver {
    status: Arc<Mutex<u16>>,
    received: Arc<Mutex<Vec<(HeaderMap, String)>>>,
}

/// A local HTTP endpoint that records deliveries and answers with `status`.
async fn receiver() -> (Receiver, String) {
    let r = Receiver::default();
    *r.status.lock().unwrap() = 200;
    let rc = r.clone();
    let app = axum::Router::new().route(
        "/hook",
        post(move |headers: HeaderMap, body: String| {
            let rc = rc.clone();
            async move {
                rc.received.lock().unwrap().push((headers, body));
                StatusCode::from_u16(*rc.status.lock().unwrap()).unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (r, url)
}

async fn setup(db: PgPool, url: &str) -> (TestApp, String) {
    let app = TestApp::new(db).await;
    let (_, hook) = app
        .admin(
            Method::POST,
            &format!("/admin/v1/projects/{}/webhooks", app.project),
            json!({"url": url}),
        )
        .await;
    let secret = hook["secret"].as_str().unwrap().to_owned();
    let alice = CustomerId(
        app.customer("alice").await["customer_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    );
    let now = Utc::now();
    let mut s = sub(
        Store::AppStore,
        "pro_monthly",
        now,
        Some(now + Duration::days(30)),
    );
    s.customer_ref = Some(alice);
    assert_eq!(app.notify("apple", "n1", vec![s]).await, StatusCode::OK);
    (app, secret)
}

async fn row(app: &TestApp) -> (String, i32, bool, Option<String>) {
    sqlx::query_as("SELECT status, attempts, next_attempt_at > now(), last_error FROM outbox")
        .fetch_one(&app.db)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn delivers_signed_payload(db: PgPool) {
    let (rx, url) = receiver().await;
    let (app, secret) = setup(db, &url).await;
    let http = Egress::Any(reqwest::Client::new());
    assert_eq!(
        deliver_due(&app.db, &common::cipher(), &http)
            .await
            .unwrap(),
        1
    );
    assert_eq!(row(&app).await.0, "delivered");
    assert_eq!(
        deliver_due(&app.db, &common::cipher(), &http)
            .await
            .unwrap(),
        0,
        "nothing left"
    );

    let (headers, body) = rx.received.lock().unwrap()[0].clone();
    let sig = headers["entitle-signature"].to_str().unwrap();
    let (t, v1) = sig.split_once(",v1=").unwrap();
    let t: i64 = t.strip_prefix("t=").unwrap().parse().unwrap();
    assert!((Utc::now().timestamp() - t).abs() < 60);
    assert_eq!(v1, sign_webhook(&secret, t, body.as_bytes()));
    let payload: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(payload["type"], "initial_purchase");
    assert_eq!(payload["project_id"], json!(app.project));
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn retries_with_backoff_then_gives_up(db: PgPool) {
    let (rx, url) = receiver().await;
    let (app, _) = setup(db, &url).await;
    let http = Egress::Any(reqwest::Client::new());
    *rx.status.lock().unwrap() = 500;

    deliver_due(&app.db, &common::cipher(), &http)
        .await
        .unwrap();
    let (status, attempts, scheduled_later, error) = row(&app).await;
    assert_eq!(
        (status.as_str(), attempts, scheduled_later),
        ("pending", 1, true)
    );
    assert!(error.unwrap().contains("500"));
    // Not due yet.
    assert_eq!(
        deliver_due(&app.db, &common::cipher(), &http)
            .await
            .unwrap(),
        0
    );

    // Endpoint recovers; make the retry due.
    *rx.status.lock().unwrap() = 204;
    sqlx::query("UPDATE outbox SET next_attempt_at = now() - interval '1 second'")
        .execute(&app.db)
        .await
        .unwrap();
    deliver_due(&app.db, &common::cipher(), &http)
        .await
        .unwrap();
    assert_eq!(row(&app).await.0, "delivered");
    assert_eq!(row(&app).await.1, 2);

    // A delivery on its last attempt is marked failed.
    *rx.status.lock().unwrap() = 500;
    sqlx::query("UPDATE outbox SET status = 'pending', attempts = $1, next_attempt_at = now()")
        .bind(MAX_ATTEMPTS - 1)
        .execute(&app.db)
        .await
        .unwrap();
    deliver_due(&app.db, &common::cipher(), &http)
        .await
        .unwrap();
    assert_eq!(row(&app).await.0, "failed");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn locked_rows_are_skipped(db: PgPool) {
    let (rx, url) = receiver().await;
    let (app, _) = setup(db, &url).await;
    let mut other_worker = app.db.begin().await.unwrap();
    sqlx::query("SELECT id FROM outbox FOR UPDATE")
        .execute(&mut *other_worker)
        .await
        .unwrap();
    let http = Egress::Any(reqwest::Client::new());
    assert_eq!(
        deliver_due(&app.db, &common::cipher(), &http)
            .await
            .unwrap(),
        0
    );
    other_worker.rollback().await.unwrap();
    assert_eq!(
        deliver_due(&app.db, &common::cipher(), &http)
            .await
            .unwrap(),
        1
    );
    assert_eq!(rx.received.lock().unwrap().len(), 1);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn no_locks_are_held_while_sending(db: PgPool) {
    // An endpoint that answers only when the test says so.
    let arrived = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (a, r) = (arrived.clone(), release.clone());
    let slow = axum::Router::new().route(
        "/hook",
        post(move || {
            let (a, r) = (a.clone(), r.clone());
            async move {
                a.notify_one();
                r.notified().await;
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, slow).await.unwrap() });
    let (app, _) = setup(db, &url).await;

    let worker = {
        let db = app.db.clone();
        tokio::spawn(async move {
            deliver_due(&db, &common::cipher(), &Egress::Any(reqwest::Client::new())).await
        })
    };
    arrived.notified().await;

    // Mid-send: the row is leased (another worker skips it) but not locked.
    let http = Egress::Any(reqwest::Client::new());
    assert_eq!(
        deliver_due(&app.db, &common::cipher(), &http)
            .await
            .unwrap(),
        0
    );
    let mut tx = app.db.begin().await.unwrap();
    sqlx::query("SELECT id FROM outbox FOR UPDATE NOWAIT")
        .execute(&mut *tx)
        .await
        .expect("row must not be locked during the HTTP send");
    tx.rollback().await.unwrap();

    release.notify_one();
    assert_eq!(worker.await.unwrap().unwrap(), 1);
    assert_eq!(row(&app).await.0, "delivered");
    assert_eq!(
        app.count("SELECT count(*) FROM outbox WHERE locked_until IS NOT NULL")
            .await,
        0
    );
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn expired_leases_are_retried(db: PgPool) {
    let (rx, url) = receiver().await;
    let (app, _) = setup(db, &url).await;
    let http = Egress::Any(reqwest::Client::new());
    // A worker claimed the row and crashed before recording the result.
    sqlx::query("UPDATE outbox SET locked_until = now() + interval '1 hour'")
        .execute(&app.db)
        .await
        .unwrap();
    assert_eq!(
        deliver_due(&app.db, &common::cipher(), &http)
            .await
            .unwrap(),
        0
    );
    sqlx::query("UPDATE outbox SET locked_until = now() - interval '1 second'")
        .execute(&app.db)
        .await
        .unwrap();
    assert_eq!(
        deliver_due(&app.db, &common::cipher(), &http)
            .await
            .unwrap(),
        1
    );
    assert_eq!(row(&app).await.0, "delivered");
    assert_eq!(rx.received.lock().unwrap().len(), 1);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn admin_lists_failed_deliveries_and_retries_them(db: PgPool) {
    let (rx, url) = receiver().await;
    *rx.status.lock().unwrap() = 500;
    let (app, _) = setup(db, &url).await;
    let http = Egress::Any(reqwest::Client::new());
    let base = format!("/admin/v1/projects/{}/webhooks/deliveries", app.project);
    deliver_due(&app.db, &common::cipher(), &http)
        .await
        .unwrap();

    let (s, list) = app
        .call(Method::GET, &base, Some(common::ADMIN), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let d = &list[0];
    assert_eq!(d["status"], "pending");
    assert_eq!(d["attempts"], 1);
    assert_eq!(d["url"], url);
    assert_eq!(d["event_type"], "initial_purchase");
    assert!(d["last_error"].as_str().unwrap().contains("500"), "{d}");
    assert!(d["next_attempt_at"].is_string());
    assert_eq!(d["payload"]["type"], "initial_purchase");
    let (_, delivered) = app
        .call(
            Method::GET,
            &format!("{base}?status=delivered"),
            Some(common::ADMIN),
            None,
        )
        .await;
    assert_eq!(delivered, json!([]));

    // The backoff would delay the next attempt; a retry makes it due now.
    *rx.status.lock().unwrap() = 200;
    assert_eq!(
        deliver_due(&app.db, &common::cipher(), &http)
            .await
            .unwrap(),
        0
    );
    let id = d["id"].as_str().unwrap();
    let (s, _) = app
        .call(
            Method::POST,
            &format!("{base}/{id}/retry"),
            Some(common::ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(
        deliver_due(&app.db, &common::cipher(), &http)
            .await
            .unwrap(),
        1
    );
    let (_, list) = app
        .call(Method::GET, &base, Some(common::ADMIN), None)
        .await;
    assert_eq!(list[0]["status"], "delivered");
    assert_eq!(list[0]["attempts"], 2);
    assert_eq!(list[0]["last_error"], Value::Null);
    assert_eq!(list[0]["next_attempt_at"], Value::Null);

    let (s, _) = app
        .call(
            Method::GET,
            &format!("{base}?status=lost"),
            Some(common::ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app
        .call(
            Method::POST,
            &format!("{base}/{}/retry", uuid::Uuid::new_v4()),
            Some(common::ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
