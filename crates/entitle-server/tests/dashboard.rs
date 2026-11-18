mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::TestApp;
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;

async fn get(app: &TestApp, uri: &str) -> (StatusCode, Option<String>, String) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let ctype = res
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned());
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, ctype, String::from_utf8_lossy(&body).into_owned())
}

// Works with and without an asset bundle next to the test binary.
#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn dashboard_asks_for_login_or_shows_build_hint(db: PgPool) {
    let app = TestApp::new(db).await;

    for uri in ["/dashboard/", "/dashboard/customers"] {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let res = app.router.clone().oneshot(req).await.unwrap();
        match res.status() {
            // Signed out: every page leads to the login.
            StatusCode::TEMPORARY_REDIRECT => {
                assert_eq!(res.headers()[header::LOCATION], "/dashboard/login", "{uri}");
            }
            StatusCode::NOT_FOUND => {
                let (_, _, body) = get(&app, uri).await;
                assert!(body.contains("topcoat asset bundle"), "{uri}: {body}");
            }
            other => panic!("{uri}: unexpected {other}"),
        }
    }

    let req = Request::builder()
        .uri("/dashboard")
        .body(Body::empty())
        .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(res.headers()[header::LOCATION], "/dashboard/");

    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(res.headers()[header::LOCATION], "/dashboard/");
}
