mod common;

use axum::http::{Method, StatusCode};
use common::TestApp;
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn test_connection_reports_the_store_verdict(db: PgPool) {
    let app = TestApp::new(db).await;
    let url = |store: &str| {
        format!(
            "/admin/v1/projects/{}/credentials/{store}/test",
            app.project
        )
    };

    let (s, body) = app
        .admin(Method::POST, &url("app_store"), json!(null))
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);

    *app.fakes.google.reject_credentials.lock().unwrap() = Some("no access".into());
    let (s, body) = app
        .admin(Method::POST, &url("play_store"), json!(null))
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], false);
    assert!(
        body["message"].as_str().unwrap().contains("no access"),
        "{body}"
    );

    let (s, _) = app
        .call(
            Method::DELETE,
            &format!("/admin/v1/projects/{}/credentials/creem", app.project),
            Some(common::ADMIN),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = app.admin(Method::POST, &url("creem"), json!(null)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app.admin(Method::POST, &url("steam"), json!(null)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
