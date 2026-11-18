mod common;

use axum::http::{Method, StatusCode};
use chrono::{Duration, Utc};
use common::TestApp;
use entitle_core::testing::sub;
use entitle_core::{CustomerId, Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

fn anon() -> String {
    format!("$entitle_anon:{}", Uuid::new_v4())
}

fn cid(info: &Value) -> CustomerId {
    CustomerId(info["customer_id"].as_str().unwrap().parse().unwrap())
}

/// Active App Store subscription linked to `owner` via the account token.
async fn buy(app: &TestApp, owner: CustomerId, store: Store, product: &str, event: &str) {
    let now = Utc::now();
    let mut s = sub(
        store,
        product,
        now - Duration::days(1),
        Some(now + Duration::days(30)),
    );
    s.original_transaction_id = format!("{event}-otid");
    s.customer_ref = Some(owner);
    let slug = match store {
        Store::AppStore => "apple",
        Store::PlayStore => "google",
        Store::Creem => "creem",
    };
    assert_eq!(app.notify(slug, event, vec![s]).await, StatusCode::OK);
}

async fn login(app: &TestApp, current: &str, new: &str) -> (StatusCode, Value) {
    app.sdk(
        Method::POST,
        &format!("/customers/{current}/login"),
        Some(json!({"new_app_user_id": new})),
    )
    .await
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn get_creates_customer_once(db: PgPool) {
    let app = TestApp::new(db).await;
    let a = anon();
    let first = app.customer(&a).await;
    let second = app.customer(&a).await;
    assert_eq!(first["customer_id"], second["customer_id"]);
    assert_eq!(app.customer_secret(&a).await["aliases"], json!([a]));
    assert_eq!(first["entitlements"], json!({}));
    let (s, _) = app
        .sdk(Method::GET, "/customers/$entitle_anon:not-a-uuid", None)
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn public_key_responses_hide_aliases_and_transaction_ids(db: PgPool) {
    let app = TestApp::new(db).await;
    let a = anon();
    let info = app.customer(&a).await;
    buy(&app, cid(&info), Store::AppStore, "pro_monthly", "a1").await;

    let (_, res) = login(&app, &a, "alice").await;
    for info in [app.customer("alice").await, res["customer_info"].clone()] {
        let purchase = &info["purchases"][0];
        assert!(info.get("aliases").is_none(), "{info}");
        assert!(purchase.get("original_transaction_id").is_none(), "{info}");
        assert!(purchase.get("store_transaction_id").is_none(), "{info}");
        assert!(info["entitlements"]["pro"].is_object(), "{info}");
    }

    let full = app.customer_secret("alice").await;
    assert_eq!(full["purchases"][0]["original_transaction_id"], "a1-otid");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn anonymous_login_to_new_user_attaches_alias(db: PgPool) {
    let app = TestApp::new(db).await;
    let a = anon();
    let before = app.customer(&a).await;
    buy(&app, cid(&before), Store::AppStore, "pro_monthly", "e1").await;

    let (s, res) = login(&app, &a, "alice").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(res["created"], true);
    let info = &res["customer_info"];
    assert_eq!(info["customer_id"], before["customer_id"]);
    assert_eq!(info["app_user_id"], "alice");
    assert_eq!(
        app.customer_secret("alice").await["aliases"],
        json!([a, "alice"])
    );
    assert!(info["entitlements"]["pro"].is_object());

    // Logging in again is a no-op.
    let (_, res) = login(&app, &a, "alice").await;
    assert_eq!(res["created"], false);
    assert_eq!(res["customer_info"]["customer_id"], before["customer_id"]);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn anonymous_login_to_existing_user_merges(db: PgPool) {
    let app = TestApp::new(db).await;
    let alice = app.customer("alice").await;
    buy(&app, cid(&alice), Store::PlayStore, "pro_yearly", "g1").await;

    let a = anon();
    let anon_info = app.customer(&a).await;
    buy(&app, cid(&anon_info), Store::AppStore, "pro_monthly", "a1").await;

    let (s, res) = login(&app, &a, "alice").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(res["created"], false);
    let info = &res["customer_info"];
    assert_eq!(info["customer_id"], alice["customer_id"]);
    assert_eq!(info["purchases"].as_array().unwrap().len(), 2);
    assert_eq!(
        app.customer_secret("alice").await["aliases"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // The anonymous id now resolves to alice; the anonymous customer is gone.
    assert_eq!(app.customer(&a).await["customer_id"], alice["customer_id"]);
    assert_eq!(app.count("SELECT count(*) FROM customers").await, 1);

    // A late store notification carrying the anonymous customer's token still lands on alice.
    buy(&app, cid(&anon_info), Store::Creem, "prod_pro", "c1").await;
    let info = app.customer("alice").await;
    assert_eq!(info["purchases"].as_array().unwrap().len(), 3);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn identified_users_are_never_merged(db: PgPool) {
    let app = TestApp::new(db).await;
    let alice = app.customer("alice").await;
    buy(&app, cid(&alice), Store::AppStore, "pro_monthly", "a1").await;

    let (s, res) = login(&app, "alice", "bob").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(res["created"], true);
    let bob = &res["customer_info"];
    assert_ne!(bob["customer_id"], alice["customer_id"]);
    assert_eq!(bob["entitlements"], json!({}));
    assert!(app.customer("alice").await["entitlements"]["pro"].is_object());

    // Switching back to an existing identified user does not merge either.
    let (_, res) = login(&app, "bob", "alice").await;
    assert_eq!(res["created"], false);
    assert_eq!(res["customer_info"]["customer_id"], alice["customer_id"]);
    assert_eq!(app.count("SELECT count(*) FROM customers").await, 2);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn cannot_log_in_to_anonymous_id(db: PgPool) {
    let app = TestApp::new(db).await;
    let (s, body) = login(&app, "alice", &anon()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn aliases_per_customer_are_bounded(db: PgPool) {
    let app = TestApp::new(db).await;
    let alice = app.customer("alice").await;
    sqlx::query(
        "INSERT INTO customer_aliases (project_id, app_user_id, customer_id)
         SELECT $1, 'old_' || n, $2 FROM generate_series(1, 98) n",
    )
    .bind(app.project)
    .bind(cid(&alice).0)
    .execute(&app.db)
    .await
    .unwrap();

    // 99 + one merged anonymous customer = the limit of 100.
    let a = anon();
    app.customer(&a).await;
    assert_eq!(login(&app, &a, "alice").await.0, StatusCode::OK);

    let b = anon();
    app.customer(&b).await;
    let (s, body) = login(&app, &b, "alice").await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
}
