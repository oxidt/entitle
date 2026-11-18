mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use common::{ADMIN, TestApp};
use sqlx::PgPool;
use tower::ServiceExt;

const ORIGIN: &str = "https://app.example.com";

async fn send(app: &TestApp, req: Request<Body>) -> (StatusCode, HeaderMap) {
    let res = app.router.clone().oneshot(req).await.unwrap();
    (res.status(), res.headers().clone())
}

fn preflight(uri: &str, method: &str) -> Request<Body> {
    Request::builder()
        .method(Method::OPTIONS)
        .uri(uri)
        .header("origin", ORIGIN)
        .header("access-control-request-method", method)
        .header(
            "access-control-request-headers",
            "authorization,content-type",
        )
        .body(Body::empty())
        .unwrap()
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn sdk_routes_allow_any_origin(db: PgPool) {
    let app = TestApp::new(db).await;

    let (status, headers) = send(&app, preflight("/v1/customers/alice/login", "POST")).await;
    assert!(status.is_success(), "{status}");
    assert_eq!(headers["access-control-allow-origin"], "*");
    let allowed = headers["access-control-allow-headers"]
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(allowed.contains("authorization"), "{allowed}");
    assert!(allowed.contains("content-type"), "{allowed}");
    assert!(headers.contains_key("access-control-max-age"));
    assert!(!headers.contains_key("access-control-allow-credentials"));

    let req = Request::builder()
        .uri("/v1/customers/alice")
        .header("origin", ORIGIN)
        .header("authorization", format!("Bearer {}", app.pk))
        .body(Body::empty())
        .unwrap();
    let (status, headers) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["access-control-allow-origin"], "*");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn admin_routes_send_no_cors_headers(db: PgPool) {
    let app = TestApp::new(db).await;

    let (status, headers) = send(&app, preflight("/admin/v1/projects", "GET")).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(!headers.contains_key("access-control-allow-origin"));

    let req = Request::builder()
        .uri("/admin/v1/projects")
        .header("origin", ORIGIN)
        .header("authorization", format!("Bearer {ADMIN}"))
        .body(Body::empty())
        .unwrap();
    let (status, headers) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("access-control-allow-origin"));
}
