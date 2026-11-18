mod common;

use axum::http::{Method, StatusCode};
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;

impl TestApp {
    async fn admin_customer(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.call(
            method,
            &format!("/admin/v1/projects/{}/customers/{path}", self.project),
            Some(common::ADMIN),
            body,
        )
        .await
    }
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn admin_looks_up_customers_without_creating_them(db: PgPool) {
    let app = TestApp::new(db).await;
    let (s, _) = app.admin_customer(Method::GET, "ghost", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(
        app.count("SELECT count(*) FROM customer_aliases WHERE app_user_id = 'ghost'")
            .await,
        0
    );

    let created = app.customer("alice").await;
    let (s, info) = app.admin_customer(Method::GET, "alice", None).await;
    assert_eq!(s, StatusCode::OK, "{info}");
    assert_eq!(info["customer_id"], created["customer_id"]);
    assert_eq!(info["app_user_id"], "alice");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn admin_grants_and_revokes_entitlements(db: PgPool) {
    let app = TestApp::new(db).await;

    // Granting to an unseen app user id creates the customer, as in the SDK API.
    let (s, info) = app
        .admin_customer(
            Method::POST,
            "carol/entitlements/pro/grant",
            Some(json!({})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{info}");
    assert_eq!(info["entitlements"]["pro"]["source"], "promotional");
    assert_eq!(info["entitlements"]["pro"]["expires_at"], Value::Null);

    let (s, info) = app
        .admin_customer(Method::POST, "carol/entitlements/pro/revoke", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{info}");
    assert!(info["entitlements"].get("pro").is_none(), "{info}");

    let (s, _) = app
        .admin_customer(Method::POST, "carol/entitlements/nope/grant", None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app
        .admin_customer(
            Method::POST,
            "carol/entitlements/pro/grant",
            Some(json!({"expires_at": "2000-01-01T00:00:00Z"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Admin token only.
    let (s, _) = app
        .call(
            Method::GET,
            &format!("/admin/v1/projects/{}/customers/carol", app.project),
            Some(&app.sk),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
