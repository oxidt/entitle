//! Shared test harness: the real router with fake store adapters.
#![allow(dead_code)]

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use entitle_core::{
    AdapterError, AppleCredentials, CheckoutAdapter, CheckoutRequest, CheckoutSession,
    CreemCredentials, GoogleCredentials, InboundNotification, ParsedNotification, PurchaseSnapshot,
    StoreAdapter,
};
use entitle_server::rate_limit::SdkBudget;
use entitle_server::{Adapters, AppState, Cipher, router};
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub const ADMIN: &str = "test-admin-token-0123456789abcdef0123";
pub const PUBLIC_URL: &str = "http://localhost:8080";
pub const FAKE_SIGNATURE_HEADER: &str = "x-fake-signature";

pub fn cipher() -> Cipher {
    Cipher::from_base64(&base64::engine::general_purpose::STANDARD.encode([42u8; 32])).unwrap()
}

/// Body format understood by [`FakeAdapter::parse_notification`].
#[derive(Serialize, Deserialize)]
pub struct FakeNotification {
    pub event_id: String,
    pub snapshots: Vec<PurchaseSnapshot>,
}

/// A store adapter whose "store" is the test itself: notifications carry
/// snapshots as JSON, fetches answer from a map the test fills.
pub struct FakeAdapter<C> {
    pub fetch: Mutex<HashMap<String, Vec<PurchaseSnapshot>>>,
    pub checkouts: Mutex<Vec<CheckoutRequest>>,
    /// `check_credentials` fails with this message when set.
    pub reject_credentials: Mutex<Option<String>>,
    _creds: PhantomData<fn() -> C>,
}

impl<C> FakeAdapter<C> {
    fn new() -> Self {
        FakeAdapter {
            fetch: Mutex::default(),
            checkouts: Mutex::default(),
            reject_credentials: Mutex::default(),
            _creds: PhantomData,
        }
    }
}

#[async_trait]
impl<C: Send + Sync + 'static> StoreAdapter for FakeAdapter<C> {
    type Credentials = C;

    async fn parse_notification(
        &self,
        _: &C,
        n: &InboundNotification,
    ) -> Result<ParsedNotification, AdapterError> {
        if n.header(FAKE_SIGNATURE_HEADER) != Some("valid") {
            return Err(AdapterError::InvalidSignature("fake".into()));
        }
        let f: FakeNotification =
            serde_json::from_slice(&n.body).map_err(|e| AdapterError::Malformed(e.to_string()))?;
        Ok(ParsedNotification {
            event_id: f.event_id,
            snapshots: f.snapshots,
        })
    }

    async fn fetch_purchases(
        &self,
        _: &C,
        id: &str,
    ) -> Result<Vec<PurchaseSnapshot>, AdapterError> {
        self.fetch
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or(AdapterError::NotFound)
    }

    async fn check_credentials(&self, _: &C) -> Result<String, AdapterError> {
        match self.reject_credentials.lock().unwrap().clone() {
            Some(message) => Err(AdapterError::Credentials(message)),
            None => Ok("fake store accepted the credentials".into()),
        }
    }
}

#[async_trait]
impl<C: Send + Sync + 'static> CheckoutAdapter for FakeAdapter<C> {
    async fn create_checkout(
        &self,
        _: &C,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, AdapterError> {
        self.checkouts.lock().unwrap().push(request.clone());
        Ok(CheckoutSession {
            id: "ch_fake".into(),
            url: format!("https://creem.test/checkout/{}", request.product_id),
        })
    }

    async fn portal_url(&self, _: &C, store_customer_id: &str) -> Result<String, AdapterError> {
        Ok(format!("https://creem.test/portal/{store_customer_id}"))
    }
}

pub struct Fakes {
    pub creem: Arc<FakeAdapter<CreemCredentials>>,
    pub apple: Arc<FakeAdapter<AppleCredentials>>,
    pub google: Arc<FakeAdapter<GoogleCredentials>>,
}

pub struct TestApp {
    pub router: Router,
    pub db: PgPool,
    pub fakes: Fakes,
    pub project: Uuid,
    pub pk: String,
    pub sk: String,
}

impl TestApp {
    /// A project with public + secret keys, credentials for all three stores and
    /// entitlement `pro` unlocked by `pro_monthly` (App Store), `pro_yearly`
    /// (Play Store) and `prod_pro` (Creem).
    pub async fn new(db: PgPool) -> TestApp {
        Self::with_sdk_budget(db, SdkBudget::per_minute(600, 6000)).await
    }

    /// [`TestApp::new`] with other SDK request budgets.
    pub async fn with_sdk_budget(db: PgPool, sdk_budget: SdkBudget) -> TestApp {
        let fakes = Fakes {
            creem: Arc::new(FakeAdapter::new()),
            apple: Arc::new(FakeAdapter::new()),
            google: Arc::new(FakeAdapter::new()),
        };
        let state = AppState {
            db: db.clone(),
            cipher: Arc::new(cipher()),
            admin_token: ADMIN.into(),
            adapters: Adapters {
                creem: fakes.creem.clone(),
                apple: fakes.apple.clone(),
                google: fakes.google.clone(),
            },
            webhook_allow_private: true,
            trust_proxy: false,
            public_url: Some(PUBLIC_URL.into()),
            sdk_budget,
        };
        let mut app = TestApp {
            router: router(state),
            db,
            fakes,
            project: Uuid::nil(),
            pk: String::new(),
            sk: String::new(),
        };

        let (s, p) = app
            .admin(Method::POST, "/admin/v1/projects", json!({"name": "Test"}))
            .await;
        assert_eq!(s, StatusCode::CREATED);
        app.project = p["id"].as_str().unwrap().parse().unwrap();
        let base = format!("/admin/v1/projects/{}", app.project);

        app.pk = app
            .admin(
                Method::POST,
                &format!("{base}/api-keys"),
                json!({"kind": "public"}),
            )
            .await
            .1["key"]
            .as_str()
            .unwrap()
            .into();
        app.sk = app
            .admin(
                Method::POST,
                &format!("{base}/api-keys"),
                json!({"kind": "secret"}),
            )
            .await
            .1["key"]
            .as_str()
            .unwrap()
            .into();

        for (store, creds) in [
            (
                "creem",
                json!({"api_key": "creem_test", "webhook_secret": "whsec", "test_mode": true}),
            ),
            (
                "app_store",
                json!({"issuer_id": "i", "key_id": "k", "private_key": "pem", "bundle_id": "com.test", "app_apple_id": null}),
            ),
            (
                "play_store",
                json!({"service_account_json": {"type": "service_account"}, "package_name": "com.test", "pubsub_audience": "aud", "pubsub_service_account_email": "pubsub@test.iam.gserviceaccount.com"}),
            ),
        ] {
            let (s, _) = app
                .admin(Method::PUT, &format!("{base}/credentials/{store}"), creds)
                .await;
            assert_eq!(s, StatusCode::NO_CONTENT, "{store} credentials");
        }

        let (_, ent) = app
            .admin(
                Method::POST,
                &format!("{base}/entitlements"),
                json!({"identifier": "pro"}),
            )
            .await;
        let ent_id = ent["id"].as_str().unwrap().to_owned();
        for (store, product) in [
            ("app_store", "pro_monthly"),
            ("play_store", "pro_yearly"),
            ("creem", "prod_pro"),
        ] {
            let (s, prod) = app
                .admin(
                    Method::POST,
                    &format!("{base}/products"),
                    json!({"store": store, "store_product_id": product}),
                )
                .await;
            assert_eq!(s, StatusCode::CREATED);
            let prod_id = prod["id"].as_str().unwrap();
            let (s, _) = app
                .call(
                    Method::PUT,
                    &format!("{base}/entitlements/{ent_id}/products/{prod_id}"),
                    Some(ADMIN),
                    None,
                )
                .await;
            assert_eq!(s, StatusCode::NO_CONTENT);
        }
        app
    }

    pub async fn call(
        &self,
        method: Method,
        uri: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(token) = bearer {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        (status, json)
    }

    pub async fn admin(&self, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
        self.call(method, uri, Some(ADMIN), Some(body)).await
    }

    pub async fn sdk(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.call(method, &format!("/v1{path}"), Some(&self.pk), body)
            .await
    }

    pub async fn secret(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.call(method, &format!("/v1{path}"), Some(&self.sk), body)
            .await
    }

    pub async fn customer(&self, app_user_id: &str) -> Value {
        let (s, info) = self
            .sdk(Method::GET, &format!("/customers/{app_user_id}"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{info}");
        info
    }

    /// Customer info as the secret key sees it (aliases, transaction ids).
    pub async fn customer_secret(&self, app_user_id: &str) -> Value {
        let (s, info) = self
            .secret(Method::GET, &format!("/customers/{app_user_id}"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{info}");
        info
    }

    /// Delivers a fake store notification to `/webhooks/{slug}/{project}`.
    pub async fn notify(
        &self,
        slug: &str,
        event_id: &str,
        snapshots: Vec<PurchaseSnapshot>,
    ) -> StatusCode {
        let body = serde_json::to_vec(&FakeNotification {
            event_id: event_id.into(),
            snapshots,
        })
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/webhooks/{slug}/{}", self.project))
            .header(FAKE_SIGNATURE_HEADER, "valid")
            .body(Body::from(body))
            .unwrap();
        self.router.clone().oneshot(req).await.unwrap().status()
    }

    pub async fn count(&self, sql: &str) -> i64 {
        let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(&self.db)
            .await
            .unwrap();
        n
    }
}
