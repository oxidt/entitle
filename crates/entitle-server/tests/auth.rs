mod common;

use axum::http::{Method, StatusCode};
use common::{ADMIN, TestApp};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn healthz(db: PgPool) {
    let app = TestApp::new(db).await;
    let (s, body) = app.call(Method::GET, "/healthz", None, None).await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, Some("ok")));
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn version_names_the_build(db: PgPool) {
    let app = TestApp::new(db).await;
    let (s, body) = app.call(Method::GET, "/version", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body.get("commit").is_some(), "{body}");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn admin_requires_token(db: PgPool) {
    let app = TestApp::new(db).await;
    assert_eq!(
        app.call(Method::GET, "/admin/v1/projects", None, None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.call(Method::GET, "/admin/v1/projects", Some("wrong-token"), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // Project keys are not admin tokens.
    assert_eq!(
        app.call(Method::GET, "/admin/v1/projects", Some(&app.sk), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.call(Method::GET, "/admin/v1/projects", Some(ADMIN), None)
            .await
            .0,
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn sdk_key_rules(db: PgPool) {
    let app = TestApp::new(db).await;
    assert!(app.pk.starts_with("pk_") && app.sk.starts_with("sk_"));
    let path = "/v1/customers/alice";
    assert_eq!(
        app.call(Method::GET, path, None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.call(Method::GET, path, Some("pk_nope"), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.call(Method::GET, path, Some(ADMIN), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.call(Method::GET, path, Some(&app.pk), None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        app.call(Method::GET, path, Some(&app.sk), None).await.0,
        StatusCode::OK
    );
    // Secret-only endpoints reject public keys.
    assert_eq!(
        app.call(
            Method::GET,
            "/v1/customers/alice/export",
            Some(&app.pk),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.call(Method::DELETE, path, Some(&app.pk), None).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.call(
            Method::GET,
            "/v1/customers/alice/export",
            Some(&app.sk),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn revoked_keys_stop_working(db: PgPool) {
    let app = TestApp::new(db).await;
    let base = format!("/admin/v1/projects/{}/api-keys", app.project);
    let (_, keys) = app.call(Method::GET, &base, Some(ADMIN), None).await;
    let keys = keys.as_array().unwrap();
    assert_eq!(keys.len(), 2);
    assert!(
        keys.iter().all(|k| k.get("key").is_none()),
        "full keys are never listed"
    );
    let pk_id = keys.iter().find(|k| k["kind"] == "public").unwrap()["id"]
        .as_str()
        .unwrap();
    let (s, _) = app
        .call(
            Method::DELETE,
            &format!("{base}/{pk_id}"),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(
        app.sdk(Method::GET, "/customers/alice", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    // Only hashes are stored.
    assert_eq!(
        app.count(&format!(
            "SELECT count(*) FROM api_keys WHERE key_prefix = '{}'",
            &app.pk[..11]
        ))
        .await,
        1
    );
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn projects_are_isolated(db: PgPool) {
    let a = TestApp::new(db.clone()).await;
    let b = TestApp::new(db).await;
    let ca = a.customer("alice").await;
    let cb = b.customer("alice").await;
    assert_ne!(ca["customer_id"], cb["customer_id"]);
    // Admin routes for one project cannot touch another project's keys.
    let (_, keys) = a
        .call(
            Method::GET,
            &format!("/admin/v1/projects/{}/api-keys", a.project),
            Some(ADMIN),
            None,
        )
        .await;
    let key_id = keys[0]["id"].as_str().unwrap();
    let (s, _) = b
        .call(
            Method::DELETE,
            &format!("/admin/v1/projects/{}/api-keys/{key_id}", b.project),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn credentials_are_validated_encrypted_and_never_returned(db: PgPool) {
    let app = TestApp::new(db).await;
    let base = format!("/admin/v1/projects/{}/credentials", app.project);
    let (s, _) = app
        .admin(
            Method::PUT,
            &format!("{base}/creem"),
            json!({"api_key": "x"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app
        .admin(Method::PUT, &format!("{base}/paypal"), json!({}))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // The Pub/Sub push service account is required.
    let (s, _) = app
        .admin(
            Method::PUT,
            &format!("{base}/play_store"),
            json!({"service_account_json": {}, "package_name": "com.test", "pubsub_audience": "aud"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, list) = app.call(Method::GET, &base, Some(ADMIN), None).await;
    assert_eq!(s, StatusCode::OK);
    let stores: Vec<_> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["store"].as_str().unwrap())
        .collect();
    assert_eq!(stores, ["app_store", "creem", "play_store"]);
    assert!(!list.to_string().contains("creem_test"));

    let (sealed,): (Vec<u8>,) =
        sqlx::query_as("SELECT sealed FROM store_credentials WHERE store = 'creem'")
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert!(!String::from_utf8_lossy(&sealed).contains("creem_test"));
}

// The "already attached" answer must not leak an attachment from another project.
#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn attach_product_is_scoped_to_the_project(db: PgPool) {
    let app = TestApp::new(db).await;
    let base = format!("/admin/v1/projects/{}", app.project);
    let (_, ents) = app
        .call(
            Method::GET,
            &format!("{base}/entitlements"),
            Some(ADMIN),
            None,
        )
        .await;
    let eid = ents[0]["id"].as_str().unwrap();
    let pid = ents[0]["product_ids"][0].as_str().unwrap();
    let attach =
        |project: String| format!("/admin/v1/projects/{project}/entitlements/{eid}/products/{pid}");

    // Already attached in its own project: idempotent success.
    let (s, _) = app
        .call(
            Method::PUT,
            &attach(app.project.to_string()),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (s, other) = app
        .admin(Method::POST, "/admin/v1/projects", json!({"name": "Other"}))
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = app
        .call(
            Method::PUT,
            &attach(other["id"].as_str().unwrap().into()),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn admin_catalog_crud(db: PgPool) {
    let app = TestApp::new(db).await;
    let base = format!("/admin/v1/projects/{}", app.project);
    let (s, _) = app
        .admin(
            Method::POST,
            &format!("{base}/products"),
            json!({"store": "creem", "store_product_id": "prod_pro"}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (_, ents) = app
        .call(
            Method::GET,
            &format!("{base}/entitlements"),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(ents[0]["identifier"], "pro");
    assert_eq!(ents[0]["product_ids"].as_array().unwrap().len(), 3);
    let (_, products) = app
        .call(Method::GET, &format!("{base}/products"), Some(ADMIN), None)
        .await;
    let pid = products[0]["id"].as_str().unwrap();
    let eid = ents[0]["id"].as_str().unwrap();
    let (s, _) = app
        .call(
            Method::DELETE,
            &format!("{base}/entitlements/{eid}/products/{pid}"),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, ents) = app
        .call(
            Method::GET,
            &format!("{base}/entitlements"),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(ents[0]["product_ids"].as_array().unwrap().len(), 2);
    let (s, _) = app
        .call(
            Method::DELETE,
            &format!("{base}/products/{pid}"),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = app
        .call(
            Method::DELETE,
            &format!("{base}/products/{pid}"),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app
        .admin(
            Method::POST,
            &format!("{base}/webhooks"),
            json!({"url": "ftp://x"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, hook) = app
        .admin(
            Method::POST,
            &format!("{base}/webhooks"),
            json!({"url": "https://example.com/hook"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(hook["secret"].as_str().unwrap().starts_with("whsec_"));
    let (_, hooks) = app
        .call(Method::GET, &format!("{base}/webhooks"), Some(ADMIN), None)
        .await;
    assert!(hooks[0].get("secret").is_none());
    let (s, _) = app.call(Method::DELETE, &base, Some(ADMIN), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(app.count("SELECT count(*) FROM products").await, 0);
}
