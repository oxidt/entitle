mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Duration, Utc};
use common::TestApp;
use entitle_core::testing::sub;
use entitle_core::{CustomerId, PurchaseKind, PurchaseSnapshot, PurchaseStatus, Store};
use entitle_server::{Adapters, AppState, router};
use serde_json::{Value, json};
use sqlx::PgPool;

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

async fn with_endpoint(app: &TestApp) {
    let (s, _) = app
        .admin(
            Method::POST,
            &format!("/admin/v1/projects/{}/webhooks", app.project),
            json!({"url": "http://127.0.0.1:9/hook"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
}

async fn events(app: &TestApp) -> Vec<String> {
    sqlx::query_as::<_, (String,)>("SELECT event_type FROM outbox ORDER BY created_at, event_type")
        .fetch_all(&app.db)
        .await
        .unwrap()
        .into_iter()
        .map(|(e,)| e)
        .collect()
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn purchase_notification_grants_entitlement(db: PgPool) {
    let app = TestApp::new(db).await;
    with_endpoint(&app).await;
    let alice = app.customer("alice").await;
    let now = Utc::now();
    let mut s = apple(Some(cid(&alice)), now, now);
    s.price = Some(entitle_core::Money {
        amount_minor: 499,
        currency: "EUR".into(),
    });
    assert_eq!(app.notify("apple", "n1", vec![s]).await, StatusCode::OK);

    let info = app.customer("alice").await;
    let pro = &info["entitlements"]["pro"];
    assert_eq!(pro["source"], "app_store");
    assert_eq!(pro["product_id"], "pro_monthly");
    assert_eq!(pro["will_renew"], true);
    assert_eq!(
        info["purchases"][0]["price"],
        json!({"amount_minor": 499, "currency": "EUR"})
    );
    assert_eq!(
        info["management_url"],
        "https://apps.apple.com/account/subscriptions"
    );
    assert_eq!(events(&app).await, ["initial_purchase"]);

    let (payload,): (Value,) = sqlx::query_as("SELECT payload FROM outbox")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(payload["type"], "initial_purchase");
    assert_eq!(payload["customer"]["app_user_ids"], json!(["alice"]));
    assert_eq!(payload["purchase"]["status"], "active");
    assert!(payload["entitlements"]["pro"].is_object());
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn duplicate_notifications_are_ignored(db: PgPool) {
    let app = TestApp::new(db).await;
    with_endpoint(&app).await;
    let alice = app.customer("alice").await;
    let now = Utc::now();
    for _ in 0..3 {
        assert_eq!(
            app.notify("apple", "same-id", vec![apple(Some(cid(&alice)), now, now)])
                .await,
            StatusCode::OK
        );
    }
    assert_eq!(events(&app).await, ["initial_purchase"]);
    assert_eq!(app.count("SELECT count(*) FROM store_events").await, 1);
    assert_eq!(app.count("SELECT count(*) FROM purchases").await, 1);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn older_snapshots_never_overwrite_newer_ones(db: PgPool) {
    let app = TestApp::new(db).await;
    with_endpoint(&app).await;
    let alice = cid(&app.customer("alice").await);
    let now = Utc::now();

    // The renewal (newer) arrives first …
    let mut renewed = apple(Some(alice), now, now);
    renewed.expires_at = Some(now + Duration::days(60));
    renewed.store_transaction_id = "1002".into();
    assert_eq!(
        app.notify("apple", "n2", vec![renewed]).await,
        StatusCode::OK
    );

    // … then a stale "expired" snapshot from before it.
    let mut expired = apple(Some(alice), now, now - Duration::hours(1));
    expired.status = PurchaseStatus::Expired;
    expired.expires_at = Some(now - Duration::hours(2));
    assert_eq!(
        app.notify("apple", "n1", vec![expired]).await,
        StatusCode::OK
    );

    let info = app.customer_secret("alice").await;
    assert_eq!(info["purchases"][0]["status"], "active");
    assert_eq!(info["purchases"][0]["store_transaction_id"], "1002");
    assert!(info["entitlements"]["pro"].is_object());
    assert_eq!(events(&app).await, ["initial_purchase"]);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn lifecycle_events_and_refund(db: PgPool) {
    let app = TestApp::new(db).await;
    with_endpoint(&app).await;
    let alice = cid(&app.customer("alice").await);
    let t0 = Utc::now();
    let at = |mins: i64| t0 + Duration::minutes(mins);

    let mut s = apple(Some(alice), t0, at(0));
    app.notify("apple", "1", vec![s.clone()]).await;
    s.auto_renew = false;
    s.status = PurchaseStatus::Cancelled;
    s.as_of = at(1);
    app.notify("apple", "2", vec![s.clone()]).await;
    let info = app.customer("alice").await;
    assert_eq!(
        info["entitlements"]["pro"]["will_renew"], false,
        "cancelled but still active"
    );

    s.auto_renew = true;
    s.status = PurchaseStatus::Active;
    s.as_of = at(2);
    app.notify("apple", "3", vec![s.clone()]).await;
    s.status = PurchaseStatus::GracePeriod;
    s.grace_period_expires_at = Some(t0 + Duration::days(40));
    s.as_of = at(3);
    app.notify("apple", "4", vec![s.clone()]).await;
    assert_eq!(
        app.customer("alice").await["entitlements"]["pro"]["billing_issue"],
        true
    );

    s.status = PurchaseStatus::Refunded;
    s.as_of = at(4);
    app.notify("apple", "5", vec![s]).await;
    assert_eq!(app.customer("alice").await["entitlements"], json!({}));
    assert_eq!(
        events(&app).await,
        [
            "initial_purchase",
            "cancellation",
            "uncancellation",
            "billing_issue",
            "refund"
        ]
    );
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn nanosecond_timestamps_do_not_cause_spurious_events(db: PgPool) {
    // Postgres keeps microseconds; a snapshot with nanosecond timestamps (Linux
    // `Utc::now()`) must not look renewed compared with its own stored copy.
    let app = TestApp::new(db).await;
    with_endpoint(&app).await;
    let alice = cid(&app.customer("alice").await);
    let t0 = DateTime::from_timestamp(1_900_000_000, 123_456_789).unwrap();

    let mut s = apple(Some(alice), t0, t0);
    app.notify("apple", "1", vec![s.clone()]).await;
    s.as_of = t0 + Duration::minutes(1);
    app.notify("apple", "2", vec![s]).await;
    assert_eq!(events(&app).await, ["initial_purchase"]);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn lifetime_purchase_and_latest_expiry_across_stores(db: PgPool) {
    let app = TestApp::new(db).await;
    let alice = cid(&app.customer("alice").await);
    let now = Utc::now();
    let mut google = sub(
        Store::PlayStore,
        "pro_yearly",
        now,
        Some(now + Duration::days(365)),
    );
    google.customer_ref = Some(alice);
    app.notify("apple", "a", vec![apple(Some(alice), now, now)])
        .await;
    app.notify("google", "g", vec![google]).await;
    let info = app.customer("alice").await;
    assert_eq!(info["entitlements"]["pro"]["source"], "play_store");
    assert_eq!(
        info["management_url"],
        "https://play.google.com/store/account/subscriptions?sku=pro_yearly&package=com.test"
    );

    let mut lifetime = sub(Store::Creem, "prod_pro", now, None);
    lifetime.kind = PurchaseKind::OneTime;
    lifetime.auto_renew = false;
    lifetime.customer_ref = Some(alice);
    lifetime.store_customer_id = Some("cust_1".into());
    app.notify("creem", "c", vec![lifetime]).await;
    let pro = &app.customer("alice").await["entitlements"]["pro"];
    assert_eq!(pro["source"], "creem");
    assert_eq!(pro["expires_at"], Value::Null);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn rejects_bad_notifications(db: PgPool) {
    let app = TestApp::new(db).await;
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!("/webhooks/apple/{}", app.project))
        .body(axum::body::Body::from("{}"))
        .unwrap();
    use tower::ServiceExt;
    let res = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "missing signature");

    assert_eq!(
        app.notify("paypal", "x", vec![]).await,
        StatusCode::NOT_FOUND
    );
    let (s, _) = app
        .call(
            Method::DELETE,
            &format!("/admin/v1/projects/{}/credentials/play_store", app.project),
            Some(common::ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(
        app.notify("google", "x", vec![]).await,
        StatusCode::NOT_FOUND,
        "store not configured"
    );
    assert_eq!(app.count("SELECT count(*) FROM store_events").await, 0);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn unlinked_purchase_is_linked_by_restore(db: PgPool) {
    let app = TestApp::new(db).await;
    with_endpoint(&app).await;
    let now = Utc::now();
    // No account token: purchase is stored unlinked, no events.
    app.notify("apple", "n1", vec![apple(None, now, now)]).await;
    assert_eq!(
        app.count("SELECT count(*) FROM purchases WHERE customer_id IS NULL")
            .await,
        1
    );
    assert!(events(&app).await.is_empty());

    app.fakes.apple.fetch.lock().unwrap().insert(
        "tx-1".into(),
        vec![apple(None, now, now + Duration::seconds(1))],
    );
    let (s, info) = app
        .sdk(
            Method::POST,
            "/customers/alice/sync",
            Some(json!({"store": "app_store", "id": "tx-1"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{info}");
    assert!(info["entitlements"]["pro"].is_object());
    assert_eq!(events(&app).await, ["initial_purchase"]);

    let (s, _) = app
        .sdk(
            Method::POST,
            "/customers/alice/sync",
            Some(json!({"store": "app_store", "id": "unknown"})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn restore_transfers_purchase_between_customers(db: PgPool) {
    let app = TestApp::new(db).await;
    with_endpoint(&app).await;
    let alice = cid(&app.customer("alice").await);
    let now = Utc::now();
    // Bought without an account token, linked to alice by her restore.
    app.notify("apple", "n1", vec![apple(None, now, now)]).await;
    app.fakes
        .apple
        .fetch
        .lock()
        .unwrap()
        .insert("tx".into(), vec![apple(None, now, now)]);
    let sync = |who: &'static str| {
        let app = &app;
        async move {
            app.sdk(
                Method::POST,
                &format!("/customers/{who}/sync"),
                Some(json!({"store": "app_store", "id": "tx"})),
            )
            .await
        }
    };
    assert_eq!(sync("alice").await.0, StatusCode::OK);

    let (s, bob) = sync("bob").await;
    assert_eq!(s, StatusCode::OK);
    assert!(bob["entitlements"]["pro"].is_object());
    assert_eq!(app.customer("alice").await["entitlements"], json!({}));
    assert_eq!(events(&app).await, ["initial_purchase", "transfer"]);
    let (payload,): (Value,) =
        sqlx::query_as("SELECT payload FROM outbox WHERE event_type = 'transfer'")
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(payload["transferred_from"]["customer_id"], json!(alice));
    assert_eq!(payload["customer"]["app_user_ids"], json!(["bob"]));

    // A later notification with alice's (stale) token does not move it back.
    let mut renewed = apple(Some(alice), now, now + Duration::minutes(1));
    renewed.expires_at = Some(now + Duration::days(60));
    app.notify("apple", "n2", vec![renewed]).await;
    assert!(app.customer("bob").await["entitlements"]["pro"].is_object());
    assert_eq!(app.customer("alice").await["entitlements"], json!({}));
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn restore_of_another_customers_purchase_is_refused(db: PgPool) {
    let app = TestApp::new(db).await;
    with_endpoint(&app).await;
    let alice = cid(&app.customer("alice").await);
    let now = Utc::now();
    app.notify("apple", "n1", vec![apple(Some(alice), now, now)])
        .await;
    app.fakes
        .apple
        .fetch
        .lock()
        .unwrap()
        .insert("tx".into(), vec![apple(Some(alice), now, now)]);

    // The store says the purchase was made by alice's account: bob cannot take it.
    let (s, body) = app
        .sdk(
            Method::POST,
            "/customers/bob/sync",
            Some(json!({"store": "app_store", "id": "tx"})),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "transfer_refused");
    assert!(app.customer("alice").await["entitlements"]["pro"].is_object());
    assert_eq!(app.customer("bob").await["entitlements"], json!({}));
    assert_eq!(events(&app).await, ["initial_purchase"]);

    // Alice herself can still restore it.
    let (s, _) = app
        .sdk(
            Method::POST,
            "/customers/alice/sync",
            Some(json!({"store": "app_store", "id": "tx"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn restore_moves_a_purchase_away_from_an_anonymous_customer(db: PgPool) {
    let app = TestApp::new(db).await;
    with_endpoint(&app).await;
    // The app was reinstalled: the old anonymous id is gone, a new one restores.
    let old = format!("$entitle_anon:{}", uuid::Uuid::new_v4());
    let new = format!("$entitle_anon:{}", uuid::Uuid::new_v4());
    let old_id = cid(&app.customer(&old).await);
    let now = Utc::now();
    app.notify("apple", "n1", vec![apple(Some(old_id), now, now)])
        .await;
    app.fakes
        .apple
        .fetch
        .lock()
        .unwrap()
        .insert("tx".into(), vec![apple(Some(old_id), now, now)]);

    let (s, body) = app
        .sdk(
            Method::POST,
            &format!("/customers/{new}/sync"),
            Some(json!({"store": "app_store", "id": "tx"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(body["entitlements"]["pro"].is_object());
    assert_eq!(app.customer(&old).await["entitlements"], json!({}));
    assert_eq!(events(&app).await, ["initial_purchase", "transfer"]);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn creem_checkout_and_portal(db: PgPool) {
    let app = TestApp::new(db).await;
    let (s, _) = app
        .sdk(
            Method::POST,
            "/customers/alice/checkout",
            Some(json!({"product_id": "prod_unknown"})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let checkout = |success_url: &'static str| {
        let app = &app;
        async move {
            app.sdk(
                Method::POST,
                "/customers/alice/checkout",
                Some(json!({"product_id": "prod_pro", "success_url": success_url, "metadata": {"plan": "a"}})),
            )
            .await
        }
    };
    // No redirect origins configured: any success_url is rejected.
    let (s, body) = checkout("https://app.test/ok").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");
    let (s, project) = app
        .admin(
            Method::PATCH,
            &format!("/admin/v1/projects/{}", app.project),
            json!({"checkout_redirect_origins": ["https://APP.test:443/", "http://localhost:3000"]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{project}");
    assert_eq!(
        project["checkout_redirect_origins"],
        json!(["http://localhost:3000", "https://app.test"])
    );
    for bad in ["https://app.test/path", "ftp://app.test", "not a url"] {
        let (s, _) = app
            .admin(
                Method::PATCH,
                &format!("/admin/v1/projects/{}", app.project),
                json!({"checkout_redirect_origins": [bad]}),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }
    for bad in [
        "https://evil.test/ok",
        "http://app.test/ok",
        "https://app.test:8443/ok",
        "https://app.test.evil.test/ok",
        "javascript:alert(1)",
    ] {
        assert_eq!(checkout(bad).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    assert!(app.fakes.creem.checkouts.lock().unwrap().is_empty());
    let (s, body) = checkout("https://app.test/ok").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body,
        json!({"id": "ch_fake", "url": "https://creem.test/checkout/prod_pro"})
    );
    let alice = cid(&app.customer("alice").await);
    let req = app.fakes.creem.checkouts.lock().unwrap()[0].clone();
    assert_eq!(req.customer, alice, "checkout carries the account token");
    assert_eq!(req.success_url.as_deref(), Some("https://app.test/ok"));

    let (s, _) = app
        .secret(Method::POST, "/customers/alice/portal", None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "no creem purchase yet");
    let now = Utc::now();
    let mut s = sub(
        Store::Creem,
        "prod_pro",
        now,
        Some(now + Duration::days(30)),
    );
    s.customer_ref = Some(alice);
    s.store_customer_id = Some("cust_42".into());
    app.notify("creem", "evt_1", vec![s]).await;
    let (st, body) = app.sdk(Method::POST, "/customers/alice/portal", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "portal needs a secret key");
    assert_eq!(body["error"]["code"], "forbidden");
    let (st, body) = app
        .secret(Method::POST, "/customers/alice/portal", None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["url"], "https://creem.test/portal/cust_42");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn promotional_grants(db: PgPool) {
    let app = TestApp::new(db).await;
    let path = "/customers/alice/entitlements/pro/grant";
    assert_eq!(
        app.sdk(Method::POST, path, None).await.0,
        StatusCode::FORBIDDEN
    );
    let (s, _) = app
        .secret(
            Method::POST,
            "/customers/alice/entitlements/nope/grant",
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let expires = Utc::now() + Duration::days(7);
    let (s, info) = app
        .secret(Method::POST, path, Some(json!({"expires_at": expires})))
        .await;
    assert_eq!(s, StatusCode::OK, "{info}");
    assert_eq!(info["entitlements"]["pro"]["source"], "promotional");
    let (s, info) = app
        .secret(
            Method::POST,
            "/customers/alice/entitlements/pro/revoke",
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(info["entitlements"], json!({}));
    let (s, info) = app.secret(Method::POST, path, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        info["entitlements"]["pro"]["expires_at"],
        Value::Null,
        "lifetime grant"
    );
}

/// The real adapters are wired to every webhook route. An unsigned fake payload
/// must be rejected by the adapter itself (stub: 501; implemented: 400/401),
/// never accepted and never answered by the server with 404/500. No network needed.
#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn real_adapters_are_wired_to_webhooks(db: PgPool) {
    let fake = TestApp::new(db.clone()).await;
    let app = TestApp {
        router: router(AppState {
            db: db.clone(),
            cipher: Arc::new(common::cipher()),
            admin_token: common::ADMIN.into(),
            adapters: Adapters::real(),
            webhook_allow_private: true,
            trust_proxy: false,
            public_url: Some(common::PUBLIC_URL.into()),
        }),
        ..fake
    };
    for slug in ["apple", "google", "creem"] {
        let status = app.notify(slug, "e", vec![]).await;
        assert!(
            [
                StatusCode::NOT_IMPLEMENTED,
                StatusCode::UNAUTHORIZED,
                StatusCode::BAD_REQUEST
            ]
            .contains(&status),
            "{slug}: {status}"
        );
    }
    assert_eq!(app.count("SELECT count(*) FROM store_events").await, 0);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn concurrent_first_notifications_for_one_purchase_are_serialized(db: PgPool) {
    // Creem sends checkout.completed and subscription.active for a new purchase
    // at the same moment: the second must wait for the first, not fail on the
    // unique (project, store, original_transaction_id) index.
    let app = Arc::new(TestApp::new(db.clone()).await);
    let alice = cid(&app.customer("alice").await);
    let now = Utc::now();

    let mut tx = db.begin().await.unwrap();
    entitle_server::purchases::apply_snapshot(
        &mut tx,
        entitle_core::ProjectId(app.project),
        &apple(Some(alice), now, now),
        entitle_server::purchases::Source::Notification,
        now,
    )
    .await
    .unwrap();

    let second = tokio::spawn({
        let app = app.clone();
        async move {
            let mut renewed = apple(Some(alice), now, now + Duration::seconds(1));
            renewed.store_transaction_id = "1001".into();
            app.notify("apple", "n2", vec![renewed]).await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    tx.commit().await.unwrap();

    assert_eq!(second.await.unwrap(), StatusCode::OK);
    assert_eq!(app.count("SELECT count(*) FROM purchases").await, 1);
    let info = app.customer_secret("alice").await;
    assert_eq!(info["purchases"][0]["store_transaction_id"], "1001");
}
