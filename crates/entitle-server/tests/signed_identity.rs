mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use common::TestApp;
use http_body_util::BodyExt;
use jsonwebtoken::{EncodingKey, Header, encode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const ANON: &str = "$entitle_anon:6f1d2c3b-4a5e-4f60-8a7b-9c0d1e2f3a4b";

async fn identity_secret(app: &TestApp) -> String {
    let (s, body) = app
        .admin(
            Method::POST,
            &format!("/admin/v1/projects/{}/identity-secret", app.project),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    body["identity_secret"].as_str().unwrap().into()
}

async fn require(app: &TestApp, on: bool) -> StatusCode {
    app.admin(
        Method::PATCH,
        &format!("/admin/v1/projects/{}", app.project),
        json!({ "require_signed_identity": on }),
    )
    .await
    .0
}

fn token(app: &TestApp, secret: &str, sub: &str) -> String {
    let claims = json!({
        "sub": sub,
        "aud": format!("entitle:{}", app.project),
        "exp": Utc::now().timestamp() + 600,
    });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

/// A public-key call with an optional `X-Entitle-Identity` header.
async fn call(
    app: &TestApp,
    method: Method,
    path: &str,
    identity: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/v1{path}"))
        .header("authorization", format!("Bearer {}", app.pk));
    if let Some(t) = identity {
        req = req.header("x-entitle-identity", t);
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn requiring_signed_identity_needs_a_secret_first(db: PgPool) {
    let app = TestApp::new(db).await;
    assert_eq!(require(&app, true).await, StatusCode::BAD_REQUEST);
    identity_secret(&app).await;
    assert_eq!(require(&app, true).await, StatusCode::OK);
    let (_, project) = app
        .admin(
            Method::GET,
            &format!("/admin/v1/projects/{}", app.project),
            json!({}),
        )
        .await;
    assert_eq!(project["require_signed_identity"], true);
    assert_eq!(project["has_identity_secret"], true);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn tokens_are_checked_when_sent_even_if_not_required(db: PgPool) {
    let app = TestApp::new(db).await;
    let secret = identity_secret(&app).await;
    let alice = token(&app, &secret, "alice");

    let (s, _) = call(&app, Method::GET, "/customers/alice", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&app, Method::GET, "/customers/alice", Some(&alice), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&app, Method::GET, "/customers/bob", Some(&alice), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(&app, Method::GET, "/customers/alice", Some("nope"), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn required_identity_guards_identified_ids_and_both_sides_of_login(db: PgPool) {
    let app = TestApp::new(db).await;
    let secret = identity_secret(&app).await;
    assert_eq!(require(&app, true).await, StatusCode::OK);
    let alice = token(&app, &secret, "alice");
    let bob = token(&app, &secret, "bob");

    // Identified ids need a token for exactly that id; anonymous ids and the
    // secret key need none.
    let (s, body) = call(&app, Method::GET, "/customers/alice", None, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "forbidden");
    let (s, _) = call(&app, Method::GET, "/customers/alice", Some(&bob), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(&app, Method::GET, "/customers/alice", Some(&alice), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&app, Method::GET, &format!("/customers/{ANON}"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = app.secret(Method::GET, "/customers/carol", None).await;
    assert_eq!(s, StatusCode::OK);

    for (path, body) in [
        (
            "/customers/alice/sync",
            json!({ "store": "app_store", "id": "1000" }),
        ),
        (
            "/customers/alice/checkout",
            json!({ "product_id": "prod_pro" }),
        ),
    ] {
        let (s, _) = call(&app, Method::POST, path, None, Some(body)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
    }

    // Login: the target needs its own token in the body...
    let login = format!("/customers/{ANON}/login");
    let (s, _) = call(
        &app,
        Method::POST,
        &login,
        None,
        Some(json!({ "new_app_user_id": "alice" })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &app,
        Method::POST,
        &login,
        None,
        Some(json!({ "new_app_user_id": "alice", "identity_token": bob })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, body) = call(
        &app,
        Method::POST,
        &login,
        None,
        Some(json!({ "new_app_user_id": "alice", "identity_token": alice })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // ...and an identified source needs the header.
    let (s, _) = call(
        &app,
        Method::POST,
        "/customers/alice/login",
        None,
        Some(json!({ "new_app_user_id": "bob", "identity_token": bob })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn an_old_anonymous_id_of_an_identified_customer_needs_a_token(db: PgPool) {
    let app = TestApp::new(db).await;
    let secret = identity_secret(&app).await;
    let alice = token(&app, &secret, "alice");
    let bob = token(&app, &secret, "bob");
    // Logged in while signed identity was still optional.
    let anon = format!("/customers/{ANON}");
    let (s, anon_info) = call(&app, Method::GET, &anon, None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        &app,
        Method::POST,
        &format!("/customers/{ANON}/login"),
        None,
        Some(json!({ "new_app_user_id": "alice" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(require(&app, true).await, StatusCode::OK);
    assert_eq!(
        app.customer_secret("alice").await["customer_id"],
        anon_info["customer_id"]
    );

    let (s, body) = call(&app, Method::GET, &anon, None, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    let (s, _) = call(&app, Method::GET, &anon, Some(&bob), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &app,
        Method::POST,
        &format!("{anon}/sync"),
        None,
        Some(json!({ "store": "app_store", "id": "1000" })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, body) = call(&app, Method::GET, &anon, Some(&alice), None).await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // A fresh anonymous id still needs nothing.
    let fresh = "/customers/$entitle_anon:00000000-0000-4000-8000-000000000001";
    let (s, _) = call(&app, Method::GET, fresh, None, None).await;
    assert_eq!(s, StatusCode::OK);
}
