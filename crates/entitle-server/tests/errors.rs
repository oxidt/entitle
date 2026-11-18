mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{ADMIN, TestApp};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

/// Sends a raw request and returns the status and the `error.code` of the JSON envelope
/// (panics if the body is not the envelope).
async fn raw(
    app: &TestApp,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
    content_type: Option<&str>,
    body: impl Into<Body>,
) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    let res = app
        .router
        .clone()
        .oneshot(req.body(body.into()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        panic!(
            "{uri}: not JSON: {:?}",
            String::from_utf8_lossy(&bytes).as_ref()
        )
    });
    let code = json["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("{uri}: no error envelope: {json}"))
        .to_owned();
    assert!(json["error"]["message"].is_string(), "{json}");
    (status, code)
}

const JSON: Option<&str> = Some("application/json");

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn extractor_rejections_use_the_error_envelope(db: PgPool) {
    let app = TestApp::new(db).await;
    let pk = Some(app.pk.as_str());
    let sk = Some(app.sk.as_str());
    let bad_request = (StatusCode::BAD_REQUEST, "bad_request".to_owned());
    let login = "/v1/customers/alice/login";

    // Bad JSON bodies.
    for (body, ct) in [
        ("{not json", JSON),
        (r#"{"wrong_field": "x"}"#, JSON),
        (r#"{"new_app_user_id": "bob"}"#, None),
        (r#"{"new_app_user_id": "bob"}"#, Some("text/plain")),
    ] {
        assert_eq!(
            raw(&app, Method::POST, login, pk, ct, body).await,
            bad_request,
            "{body} {ct:?}"
        );
    }
    // Unknown store on /sync.
    assert_eq!(
        raw(
            &app,
            Method::POST,
            "/v1/customers/alice/sync",
            pk,
            JSON,
            r#"{"store": "steam", "id": "1"}"#
        )
        .await,
        bad_request
    );
    // Optional body that is present but broken.
    assert_eq!(
        raw(
            &app,
            Method::POST,
            "/v1/customers/alice/entitlements/pro/grant",
            sk,
            JSON,
            "{"
        )
        .await,
        bad_request
    );
    // Non-UUID path parameters.
    assert_eq!(
        raw(
            &app,
            Method::GET,
            "/admin/v1/projects/not-a-uuid",
            Some(ADMIN),
            None,
            ""
        )
        .await,
        bad_request
    );
    assert_eq!(
        raw(
            &app,
            Method::POST,
            "/webhooks/apple/not-a-uuid",
            None,
            None,
            "{}"
        )
        .await,
        bad_request
    );
    // Too large.
    assert_eq!(
        raw(
            &app,
            Method::POST,
            login,
            pk,
            JSON,
            vec![b' '; 3 * 1024 * 1024]
        )
        .await,
        (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large".into())
    );
    // Unknown route and wrong method.
    assert_eq!(
        raw(&app, Method::GET, "/v1/nope", pk, None, "").await,
        (StatusCode::NOT_FOUND, "not_found".into())
    );
    assert_eq!(
        raw(&app, Method::PUT, "/v1/customers/alice", pk, None, "").await,
        (StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed".into())
    );
    assert_eq!(
        raw(&app, Method::GET, "/webhooks/apple/x", None, None, "").await,
        (StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed".into())
    );
}
