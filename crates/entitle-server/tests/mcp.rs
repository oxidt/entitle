mod common;

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{ADMIN, TestApp};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower::ServiceExt;

const CALLBACK: &str = "http://localhost:9999/callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

async fn send(app: &TestApp, req: Request<Body>) -> (StatusCode, Response) {
    let response = app.router.clone().oneshot(req).await.unwrap();
    (response.status(), response)
}

async fn text(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn form(uri: &str, fields: &[(&str, &str)]) -> Request<Body> {
    let body = fields
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Registers a client and walks the consent page, allowing changes when `write`;
/// returns `(client_id, code)`.
async fn authorize(app: &TestApp, write: bool) -> (String, String) {
    let (status, response) = send(
        app,
        Request::post("/oauth/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"redirect_uris": [CALLBACK], "client_name": "Test <client>"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let client: Value = serde_json::from_str(&text(response).await).unwrap();
    let client_id = client["client_id"].as_str().unwrap().to_owned();

    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER));
    let params = [
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", CALLBACK),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", "xyz"),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let (status, response) = send(
        app,
        Request::get(format!("/oauth/authorize?{query}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page = text(response).await;
    assert!(
        page.contains("Connect Test &lt;client&gt; to entitle"),
        "{page}"
    );

    let mut fields = params.to_vec();
    fields.extend([("admin_token", "wrong"), ("decision", "approve")]);
    if write {
        fields.push(("write", "1"));
    }
    let (status, response) = send(app, form("/oauth/authorize", &fields)).await;
    assert_eq!(status, StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains("That token was rejected."));
    // The retry keeps "Allow changes" as submitted.
    assert_eq!(page.contains("value=\"1\" checked"), write, "{page}");

    fields.truncate(params.len());
    fields.extend([("admin_token", ADMIN), ("decision", "approve")]);
    if write {
        fields.push(("write", "1"));
    }
    let (status, response) = send(app, form("/oauth/authorize", &fields)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = response.headers()[header::LOCATION].to_str().unwrap();
    let rest = location.strip_prefix(&format!("{CALLBACK}?code=")).unwrap();
    let (code, state) = rest.split_once('&').unwrap();
    assert_eq!(state, "state=xyz");
    (client_id, code.to_owned())
}

async fn exchange(app: &TestApp, client_id: &str, code: &str) -> (StatusCode, Value) {
    let (status, response) = send(
        app,
        form(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", CALLBACK),
                ("client_id", client_id),
                ("code_verifier", VERIFIER),
            ],
        ),
    )
    .await;
    (status, serde_json::from_str(&text(response).await).unwrap())
}

fn mcp(token: Option<&str>, body: Value) -> Request<Body> {
    let mut req = Request::post("/mcp")
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream");
    if let Some(token) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    req.body(Body::from(body.to_string())).unwrap()
}

/// The JSON-RPC message in an SSE or JSON response body.
fn rpc(body: &str) -> Value {
    let json = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        .find(|d| !d.is_empty())
        .unwrap_or(body);
    serde_json::from_str(json).unwrap()
}

/// Initializes and calls `name` (stateless: no session id); returns the JSON-RPC response.
async fn call_tool(app: &TestApp, token: &str, name: &str, arguments: Value) -> Value {
    let (status, response) = send(
        app,
        mcp(
            Some(token),
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"},
            }}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(response.headers().get("mcp-session-id").is_none());
    let init = rpc(&text(response).await);
    assert_eq!(init["result"]["serverInfo"]["name"], "entitle");

    let (status, response) = send(
        app,
        mcp(
            Some(token),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    rpc(&text(response).await)
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn oauth_flow_grants_mcp_access(db: PgPool) {
    let app = TestApp::new(db).await;
    let (client_id, code) = authorize(&app, false).await;

    let (status, token) = exchange(&app, &client_id, &code).await;
    assert_eq!(status, StatusCode::OK, "{token}");
    let access = token["access_token"].as_str().unwrap();
    assert!(access.starts_with("mcp_"));
    assert_eq!(token["scope"], "mcp");

    // Codes are single use.
    let (status, replay) = exchange(&app, &client_id, &code).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(replay["error"], "invalid_grant");

    let result = call_tool(&app, access, "list_projects", json!({})).await["result"].clone();
    let projects: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(projects[0]["id"], app.project.to_string());

    let result = call_tool(
        &app,
        access,
        "get_customer",
        json!({"project_id": app.project, "app_user_id": "nobody/1"}),
    )
    .await;
    assert_eq!(result["result"]["isError"], true);

    // Approved without "Allow changes": write tools refuse.
    let refused = call_tool(
        &app,
        access,
        "grant_entitlement",
        json!({"project_id": app.project, "app_user_id": "u1", "identifier": "pro"}),
    )
    .await;
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("read-only"),
        "{refused}"
    );
    assert_eq!(
        app.count("SELECT count(*) FROM promotional_grants").await,
        0
    );
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn write_access_grants_and_revokes(db: PgPool) {
    let app = TestApp::new(db).await;
    // TestApp's project already has the `pro` entitlement.
    let (client_id, code) = authorize(&app, true).await;
    let (_, token) = exchange(&app, &client_id, &code).await;
    assert_eq!(token["scope"], "mcp mcp:write");
    let access = token["access_token"].as_str().unwrap();

    let args = json!({"project_id": app.project, "app_user_id": "u1", "identifier": "pro"});
    let granted = call_tool(&app, access, "grant_entitlement", args.clone()).await;
    let info: Value =
        serde_json::from_str(granted["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(info["entitlements"]["pro"].is_object(), "{info}");

    let revoked = call_tool(&app, access, "revoke_entitlement", args).await;
    let info: Value =
        serde_json::from_str(revoked["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(info["entitlements"].get("pro").is_none(), "{info}");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn oauth_endpoints_are_rate_limited(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut statuses = Vec::new();
    for _ in 0..31 {
        let (status, _) = send(&app, form("/oauth/token", &[])).await;
        statuses.push(status);
    }
    assert!(statuses[..30].iter().all(|s| *s == StatusCode::BAD_REQUEST));
    assert_eq!(statuses[30], StatusCode::TOO_MANY_REQUESTS);
    // Discovery is not limited.
    let (status, _) = send(
        &app,
        Request::get("/.well-known/oauth-authorization-server")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn mcp_without_a_valid_token_is_challenged(db: PgPool) {
    let app = TestApp::new(db).await;
    for token in [None, Some("mcp_unknown"), Some("sk_whatever")] {
        let (status, response) = send(&app, mcp(token, json!({}))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let challenge = response.headers()[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap();
        assert!(
            challenge.contains(
                "resource_metadata=\"http://localhost:8080/.well-known/oauth-protected-resource/mcp\""
            ),
            "{challenge}"
        );
    }
    // The admin token works directly, for clients with a static header.
    let result = call_tool(&app, ADMIN, "list_projects", json!({})).await;
    assert_eq!(result["result"]["isError"], false);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn consent_rejects_unregistered_redirects(db: PgPool) {
    let app = TestApp::new(db).await;
    let (status, response) = send(
        &app,
        Request::post("/oauth/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"redirect_uris": ["https://evil.example/cb"]}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", text(response).await);

    let long = format!("http://localhost:9999/{}", "a".repeat(600));
    let (status, response) = send(
        &app,
        Request::post("/oauth/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"redirect_uris": [long]}).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", text(response).await);

    let (client_id, _) = authorize(&app, false).await;
    let (status, _) = send(
        &app,
        Request::get(format!(
            "/oauth/authorize?response_type=code&client_id={client_id}\
             &redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fother&code_challenge=x&code_challenge_method=S256"
        ))
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn discovery_metadata(db: PgPool) {
    let app = TestApp::new(db).await;
    let (_, response) = send(
        &app,
        Request::get("/.well-known/oauth-authorization-server")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let meta: Value = serde_json::from_str(&text(response).await).unwrap();
    assert_eq!(meta["issuer"], "http://localhost:8080");
    assert_eq!(
        meta["authorization_endpoint"],
        "http://localhost:8080/oauth/authorize"
    );
    let (_, response) = send(
        &app,
        Request::get("/.well-known/oauth-protected-resource/mcp")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let meta: Value = serde_json::from_str(&text(response).await).unwrap();
    assert_eq!(meta["resource"], "http://localhost:8080/mcp");
}

/// Anonymous ids act as the customer with the public key and store transaction ids
/// authorize a restore: read tools must not hand them out.
#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn read_tools_hide_customer_credentials(db: PgPool) {
    use axum::http::Method;
    use chrono::{Duration, Utc};
    use entitle_core::Store;
    use entitle_core::testing::sub;

    let app = TestApp::new(db).await;
    let (client_id, code) = authorize(&app, false).await;
    let (_, token) = exchange(&app, &client_id, &code).await;
    let access = token["access_token"].as_str().unwrap();

    // One customer stays anonymous, one logs in from an anonymous id; both bought.
    let mut ids = Vec::new();
    for (n, login) in [(1, None), (2, Some("alice"))] {
        let anon = format!("$entitle_anon:{}", uuid::Uuid::new_v4());
        let customer = app.customer(&anon).await["customer_id"].clone();
        let now = Utc::now();
        let mut s = sub(
            Store::AppStore,
            "pro_monthly",
            now - Duration::days(1),
            Some(now + Duration::days(30)),
        );
        s.original_transaction_id = format!("otid-secret-{n}");
        s.store_transaction_id = format!("txid-secret-{n}");
        s.customer_ref = Some(entitle_core::CustomerId(
            customer.as_str().unwrap().parse().unwrap(),
        ));
        app.notify("apple", &format!("e{n}"), vec![s]).await;
        if let Some(login) = login {
            let (status, _) = app
                .sdk(
                    Method::POST,
                    &format!("/customers/{anon}/login"),
                    Some(json!({"new_app_user_id": login})),
                )
                .await;
            assert_eq!(status, StatusCode::OK);
        }
        ids.push(customer);
    }

    let tool_text = |name: &'static str, args: Value| {
        let app = &app;
        async move {
            let response = call_tool(app, access, name, args).await;
            assert_ne!(response["result"]["isError"], true, "{response}");
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let project = app.project;
    let texts = [
        tool_text("list_customers", json!({"project_id": project})).await,
        tool_text("list_activity", json!({"project_id": project})).await,
        tool_text(
            "list_activity",
            json!({"project_id": project, "customer_id": ids[0]}),
        )
        .await,
        tool_text(
            "get_customer",
            json!({"project_id": project, "customer_id": ids[0]}),
        )
        .await,
        tool_text(
            "get_customer",
            json!({"project_id": project, "app_user_id": "alice"}),
        )
        .await,
    ];
    for text in &texts {
        assert!(!text.contains("$entitle_anon:"), "{text}");
        assert!(!text.contains("-secret-"), "{text}");
    }
    let anonymous: Value = serde_json::from_str(&texts[3]).unwrap();
    assert_eq!(anonymous["customer_id"], ids[0]);
    assert!(anonymous["entitlements"]["pro"].is_object(), "{anonymous}");
    let alice: Value = serde_json::from_str(&texts[4]).unwrap();
    assert_eq!(alice["aliases"], json!(["alice"]));
    assert_eq!(alice["customer_id"], ids[1]);
    let customers: Value = serde_json::from_str(&texts[0]).unwrap();
    assert_eq!(customers.as_array().unwrap().len(), 2);
}

/// `POST /oauth/register` from the client at `peer`.
fn register_request(peer: &str) -> Request<Body> {
    let mut req = Request::post("/oauth/register")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"redirect_uris": [CALLBACK]}).to_string()))
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    req
}

async fn register_from(app: &TestApp, peer: &str) -> StatusCode {
    send(app, register_request(peer)).await.0
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn pending_registrations_are_capped_per_source(db: PgPool) {
    let app = TestApp::new(db).await;
    for _ in 0..10 {
        assert_eq!(register_from(&app, "10.0.0.1:1").await, StatusCode::CREATED);
    }
    assert_eq!(
        register_from(&app, "10.0.0.1:2").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    // IPv6 sources share an allocation per /64.
    for _ in 0..10 {
        assert_eq!(
            register_from(&app, "[2001:db8::1]:1").await,
            StatusCode::CREATED
        );
    }
    assert_eq!(
        register_from(&app, "[2001:db8::2]:1").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(register_from(&app, "10.0.0.2:1").await, StatusCode::CREATED);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn pending_registrations_expire_after_an_hour(db: PgPool) {
    let app = TestApp::new(db).await;
    sqlx::query(
        "INSERT INTO mcp_clients (client_id, redirect_uris, source, created_at)
         SELECT 'old_' || i, '{}', '10.0.0.1', now() - interval '61 minutes'
         FROM generate_series(1, 10) i",
    )
    .execute(&app.db)
    .await
    .unwrap();
    assert_eq!(register_from(&app, "10.0.0.1:1").await, StatusCode::CREATED);
    assert_eq!(
        app.count("SELECT count(*) FROM mcp_clients WHERE client_id LIKE 'old_%'")
            .await,
        0
    );
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn the_global_pending_cap_holds_under_concurrency(db: PgPool) {
    let app = TestApp::new(db).await;
    sqlx::query(
        "INSERT INTO mcp_clients (client_id, redirect_uris, source)
         SELECT 'pending_' || i, '{}', 'seeded' FROM generate_series(1, 998) i",
    )
    .execute(&app.db)
    .await
    .unwrap();
    let mut registrations = tokio::task::JoinSet::new();
    for i in 1..=10 {
        let router = app.router.clone();
        let req = register_request(&format!("10.0.1.{i}:1"));
        registrations.spawn(async move { router.oneshot(req).await.unwrap().status() });
    }
    let statuses = registrations.join_all().await;
    let created = statuses
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();
    assert_eq!(created, 2, "{statuses:?}");
    assert_eq!(app.count("SELECT count(*) FROM mcp_clients").await, 1000);
}
