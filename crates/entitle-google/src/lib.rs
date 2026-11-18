//! Google Play adapter: Real-time developer notifications via Pub/Sub push and the Play Developer API
//! (`purchases.subscriptionsv2`, `purchases.productsv2`).
//!
//! The contract is described in `docs/adapters.md`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use entitle_core::currency::minor_from_units_nanos;
use entitle_core::{
    AdapterError, CustomerId, Environment, GoogleCredentials, InboundNotification, Money,
    ParsedNotification, PeriodType, PurchaseKind, PurchaseSnapshot, PurchaseStatus, Store,
    StoreAdapter,
};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use reqwest::{Method, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;

const SCOPE: &str = "https://www.googleapis.com/auth/androidpublisher";
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const OIDC_ISSUERS: [&str; 2] = ["accounts.google.com", "https://accounts.google.com"];
/// How long Google's signing keys are cached.
const JWKS_TTL: Duration = Duration::from_secs(3600);
/// Minimum time between JWKS refetches triggered by an unknown `kid`.
const JWKS_MIN_REFRESH: Duration = Duration::from_secs(60);
/// Access tokens are renewed this long before they expire.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);
const ACK_PENDING: &str = "ACKNOWLEDGEMENT_STATE_PENDING";
const CONSUMED: &str = "CONSUMPTION_STATE_CONSUMED";
const PRODUCT_TYPE_SUBSCRIPTION: i32 = 1;
const PRODUCT_TYPE_ONE_TIME: i32 = 2;
/// `voidedPurchaseNotification.refundType`: `REFUND_TYPE_QUANTITY_BASED_PARTIAL_REFUND`.
const REFUND_TYPE_PARTIAL: i32 = 2;

/// Google endpoints the adapter talks to. Tests point these at a mock server.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// Base URL of the Play Developer API (`androidpublisher`).
    pub api_base: String,
    /// OAuth token endpoint for the service-account JWT-bearer grant.
    pub token_url: String,
    /// Google's OIDC signing keys (JWKS), used to verify Pub/Sub push tokens.
    pub jwks_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            api_base: "https://androidpublisher.googleapis.com".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            jwks_url: "https://www.googleapis.com/oauth2/v3/certs".into(),
        }
    }
}

pub struct GoogleAdapter {
    http: reqwest::Client,
    endpoints: Endpoints,
    jwks: Mutex<Option<(JwkSet, Instant)>>,
    /// Access tokens by service account (`client_email/private_key_id`).
    tokens: Mutex<HashMap<String, (String, Instant)>>,
}

impl Default for GoogleAdapter {
    fn default() -> Self {
        Self::with_endpoints(Endpoints::default())
    }
}

impl GoogleAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_endpoints(endpoints: Endpoints) -> Self {
        GoogleAdapter {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("HTTP client"),
            endpoints,
            jwks: Mutex::new(None),
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Verifies the Pub/Sub push OIDC token (`Authorization: Bearer <JWT>`).
    /// Everything that can be checked offline is checked before the JWKS is fetched.
    async fn verify_push(
        &self,
        credentials: &GoogleCredentials,
        notification: &InboundNotification,
    ) -> Result<(), AdapterError> {
        let token = notification
            .header("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or_else(|| signature("missing bearer token"))?;
        let header = jsonwebtoken::decode_header(token).map_err(signature)?;
        if header.alg != Algorithm::RS256 {
            return Err(signature("unexpected algorithm"));
        }
        let kid = header.kid.ok_or_else(|| signature("missing kid"))?;
        if credentials.pubsub_audience.is_empty() {
            return Err(AdapterError::Credentials("pubsub_audience is empty".into()));
        }
        if credentials.pubsub_service_account_email.is_empty() {
            return Err(AdapterError::Credentials(
                "pubsub_service_account_email is empty".into(),
            ));
        }
        let key = self.signing_key(&kid).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&credentials.pubsub_audience]);
        validation.set_issuer(&OIDC_ISSUERS);
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        let claims = jsonwebtoken::decode::<PushClaims>(token, &key, &validation)
            .map_err(signature)?
            .claims;
        // Any service account can mint a token for our audience: only the one
        // configured on the Pub/Sub subscription may push.
        if claims.email.as_deref() != Some(credentials.pubsub_service_account_email.as_str())
            || !claims.email_verified
        {
            return Err(signature("unexpected service account"));
        }
        Ok(())
    }

    /// Google's OIDC key for `kid`, from the cache or a (rate-limited) JWKS fetch.
    async fn signing_key(&self, kid: &str) -> Result<DecodingKey, AdapterError> {
        let find = |set: &JwkSet| set.find(kid).map(DecodingKey::from_jwk);
        if let Some((set, fetched)) = &*self.jwks.lock().unwrap() {
            if fetched.elapsed() < JWKS_TTL
                && let Some(key) = find(set)
            {
                return key.map_err(signature);
            }
            if fetched.elapsed() < JWKS_MIN_REFRESH {
                return Err(signature("unknown key id"));
            }
        }
        let resp = self
            .http
            .get(&self.endpoints.jwks_url)
            .send()
            .await
            .map_err(upstream)?;
        if !resp.status().is_success() {
            return Err(AdapterError::Upstream(format!(
                "Google JWKS returned {}",
                resp.status()
            )));
        }
        let set: JwkSet = resp.json().await.map_err(upstream)?;
        let key = find(&set);
        *self.jwks.lock().unwrap() = Some((set, Instant::now()));
        key.ok_or_else(|| signature("unknown key id"))?
            .map_err(signature)
    }

    /// OAuth access token for the project's service account, cached until near expiry.
    async fn access_token(&self, credentials: &GoogleCredentials) -> Result<String, AdapterError> {
        #[derive(Deserialize)]
        struct ServiceAccount {
            client_email: String,
            private_key: String,
            private_key_id: Option<String>,
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: u64,
        }

        let sa: ServiceAccount =
            serde_json::from_value(credentials.service_account_json.clone())
                .map_err(|_| AdapterError::Credentials("invalid service account JSON".into()))?;
        let cache_key = format!(
            "{}/{}",
            sa.client_email,
            sa.private_key_id.as_deref().unwrap_or_default()
        );
        if let Some((token, expires)) = self.tokens.lock().unwrap().get(&cache_key)
            && *expires > Instant::now() + TOKEN_MARGIN
        {
            return Ok(token.clone());
        }

        let key = EncodingKey::from_rsa_pem(sa.private_key.as_bytes())
            .map_err(|_| AdapterError::Credentials("invalid service account private key".into()))?;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = sa.private_key_id;
        let now = Utc::now().timestamp();
        let claims = serde_json::json!({
            "iss": sa.client_email,
            "scope": SCOPE,
            "aud": self.endpoints.token_url,
            "iat": now,
            "exp": now + 3600,
        });
        let assertion = jsonwebtoken::encode(&header, &claims, &key)
            .map_err(|_| AdapterError::Credentials("invalid service account private key".into()))?;
        let resp = self
            .http
            .post(&self.endpoints.token_url)
            .form(&[("grant_type", JWT_BEARER_GRANT), ("assertion", &assertion)])
            .send()
            .await
            .map_err(upstream)?;
        let status = resp.status();
        if status.is_client_error() {
            return Err(AdapterError::Credentials(format!(
                "Google rejected the service account ({status})"
            )));
        }
        if !status.is_success() {
            return Err(AdapterError::Upstream(format!(
                "Google token endpoint returned {status}"
            )));
        }
        let token: TokenResponse = resp.json().await.map_err(upstream)?;
        let expires = Instant::now() + Duration::from_secs(token.expires_in);
        self.tokens
            .lock()
            .unwrap()
            .insert(cache_key, (token.access_token.clone(), expires));
        Ok(token.access_token)
    }

    /// Authenticated call to `…/androidpublisher/v3/applications/{package}/purchases/{path}`.
    async fn api(
        &self,
        credentials: &GoogleCredentials,
        method: Method,
        path: &str,
    ) -> Result<Response, AdapterError> {
        let token = self.access_token(credentials).await?;
        let url = format!(
            "{}/androidpublisher/v3/applications/{}/purchases/{path}",
            self.endpoints.api_base, credentials.package_name
        );
        let mut request = self.http.request(method.clone(), url).bearer_auth(token);
        if method == Method::POST {
            request = request.json(&serde_json::json!({}));
        }
        let resp = request.send().await.map_err(upstream)?;
        match resp.status() {
            s if s.is_success() => Ok(resp),
            // Google answers 400 for tokens it cannot parse and 410 for purchases it dropped.
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::GONE => {
                Err(AdapterError::NotFound)
            }
            s @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => Err(
                AdapterError::Credentials(format!("Google Play API rejected the request ({s})")),
            ),
            s => Err(AdapterError::Upstream(format!(
                "Google Play API returned {s}"
            ))),
        }
    }

    async fn api_get<T: DeserializeOwned>(
        &self,
        credentials: &GoogleCredentials,
        path: &str,
    ) -> Result<T, AdapterError> {
        let resp = self.api(credentials, Method::GET, path).await?;
        resp.json()
            .await
            .map_err(|e| AdapterError::Upstream(format!("unexpected Google Play response: {e}")))
    }

    /// Acknowledges a purchase. Failures are only logged: the state is still valid,
    /// and the next notification or restore tries again.
    async fn acknowledge(&self, credentials: &GoogleCredentials, path: &str, product_id: &str) {
        if let Err(e) = self.api(credentials, Method::POST, path).await {
            tracing::warn!(%product_id, error = %e, "failed to acknowledge Google Play purchase");
        }
    }

    /// The subscription behind `token`, plus the purchase it replaced (`linkedPurchaseToken`).
    async fn subscription_snapshots(
        &self,
        credentials: &GoogleCredentials,
        token: &str,
        as_of: DateTime<Utc>,
    ) -> Result<Vec<PurchaseSnapshot>, AdapterError> {
        let sub: SubscriptionV2 = self
            .api_get(credentials, &format!("subscriptionsv2/tokens/{token}"))
            .await?;
        let Some(snapshot) = subscription_snapshot(token, &sub, as_of)? else {
            return Ok(vec![]);
        };
        if sub.acknowledgement_state.as_deref() == Some(ACK_PENDING) && grants_access(&snapshot) {
            let path = format!(
                "subscriptions/{}/tokens/{token}:acknowledge",
                snapshot.product_id
            );
            self.acknowledge(credentials, &path, &snapshot.product_id)
                .await;
        }
        let mut snapshots = vec![snapshot];
        if let Some(linked) = &sub.linked_purchase_token {
            match self
                .api_get::<SubscriptionV2>(credentials, &format!("subscriptionsv2/tokens/{linked}"))
                .await
            {
                Ok(old) => snapshots.extend(subscription_snapshot(linked, &old, as_of)?),
                Err(AdapterError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(snapshots)
    }

    async fn product_snapshots(
        &self,
        credentials: &GoogleCredentials,
        token: &str,
        as_of: DateTime<Utc>,
    ) -> Result<Vec<PurchaseSnapshot>, AdapterError> {
        let product: ProductV2 = self
            .api_get(credentials, &format!("productsv2/tokens/{token}"))
            .await?;
        lifetime_product(&product)?;
        let Some(snapshot) = product_snapshot(token, &product, as_of)? else {
            return Ok(vec![]);
        };
        if product.acknowledgement_state.as_deref() == Some(ACK_PENDING) && grants_access(&snapshot)
        {
            let path = format!(
                "products/{}/tokens/{token}:acknowledge",
                snapshot.product_id
            );
            self.acknowledge(credentials, &path, &snapshot.product_id)
                .await;
        }
        Ok(vec![snapshot])
    }

    /// A voided purchase: the fetched state, marked refunded. A quantity-based partial
    /// refund leaves the purchase (and access) as it is.
    async fn voided_snapshots(
        &self,
        credentials: &GoogleCredentials,
        voided: &VoidedNotification,
        as_of: DateTime<Utc>,
    ) -> Result<Vec<PurchaseSnapshot>, AdapterError> {
        if voided.refund_type == Some(REFUND_TYPE_PARTIAL) {
            return Ok(vec![]);
        }
        let token = &voided.purchase_token;
        let snapshot = match voided.product_type {
            PRODUCT_TYPE_SUBSCRIPTION => {
                let sub: SubscriptionV2 = self
                    .api_get(credentials, &format!("subscriptionsv2/tokens/{token}"))
                    .await?;
                subscription_snapshot(token, &sub, as_of)?
            }
            PRODUCT_TYPE_ONE_TIME => {
                let product: ProductV2 = self
                    .api_get(credentials, &format!("productsv2/tokens/{token}"))
                    .await?;
                product_snapshot(token, &product, as_of)?
            }
            other => return Err(malformed(format!("unknown productType {other}"))),
        };
        Ok(snapshot
            .map(|s| PurchaseSnapshot {
                status: PurchaseStatus::Refunded,
                auto_renew: false,
                ..s
            })
            .into_iter()
            .collect())
    }
}

#[async_trait]
impl StoreAdapter for GoogleAdapter {
    type Credentials = GoogleCredentials;

    async fn parse_notification(
        &self,
        credentials: &GoogleCredentials,
        notification: &InboundNotification,
    ) -> Result<ParsedNotification, AdapterError> {
        self.verify_push(credentials, notification).await?;

        let push: PushBody = serde_json::from_slice(&notification.body).map_err(malformed)?;
        let data = STANDARD.decode(&push.message.data).map_err(malformed)?;
        let n: DeveloperNotification = serde_json::from_slice(&data).map_err(malformed)?;
        if n.package_name != credentials.package_name {
            return Err(malformed(format!(
                "notification for package {}",
                n.package_name
            )));
        }
        let as_of = n
            .event_time_millis
            .parse()
            .ok()
            .and_then(DateTime::from_timestamp_millis)
            .ok_or_else(|| malformed("invalid eventTimeMillis"))?;

        let snapshots = if let Some(s) = &n.subscription_notification {
            self.subscription_snapshots(credentials, &s.purchase_token, as_of)
                .await?
        } else if let Some(p) = &n.one_time_product_notification {
            match self
                .product_snapshots(credentials, &p.purchase_token, as_of)
                .await
            {
                // Consumed purchases, rentals: acknowledged, but nothing to store.
                Err(AdapterError::Unsupported(_)) => vec![],
                result => result?,
            }
        } else if let Some(v) = &n.voided_purchase_notification {
            self.voided_snapshots(credentials, v, as_of).await?
        } else {
            // testNotification, or a notification type without purchase state.
            vec![]
        };
        Ok(ParsedNotification {
            event_id: push.message.message_id,
            snapshots,
        })
    }

    async fn fetch_purchases(
        &self,
        credentials: &GoogleCredentials,
        id: &str,
    ) -> Result<Vec<PurchaseSnapshot>, AdapterError> {
        // Purchase tokens are URL-safe; anything else is not one (and must not reach the URL).
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(AdapterError::NotFound);
        }
        let now = Utc::now();
        match self.subscription_snapshots(credentials, id, now).await {
            Err(AdapterError::NotFound) => self.product_snapshots(credentials, id, now).await,
            result => result,
        }
    }

    async fn check_credentials(
        &self,
        credentials: &GoogleCredentials,
    ) -> Result<String, AdapterError> {
        // Needs the same permission as purchase verification (financial data).
        match self
            .api(credentials, Method::GET, "voidedpurchases?maxResults=1")
            .await
        {
            Ok(_) => Ok(format!(
                "The Google Play API accepted the service account for {}.",
                credentials.package_name
            )),
            Err(AdapterError::NotFound) => Err(AdapterError::Credentials(format!(
                "Google Play does not know the package {}",
                credentials.package_name
            ))),
            Err(e) => Err(e),
        }
    }
}

#[derive(Deserialize)]
struct PushBody {
    message: PushMessage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushMessage {
    data: String,
    message_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeveloperNotification {
    package_name: String,
    event_time_millis: String,
    subscription_notification: Option<TokenNotification>,
    one_time_product_notification: Option<TokenNotification>,
    voided_purchase_notification: Option<VoidedNotification>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenNotification {
    purchase_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoidedNotification {
    purchase_token: String,
    product_type: i32,
    /// `1` full refund, `2` quantity-based partial refund. Absent = full.
    refund_type: Option<i32>,
}

/// `purchases.subscriptionsv2` resource (the fields we use).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionV2 {
    #[serde(default)]
    line_items: Vec<SubscriptionLineItem>,
    start_time: Option<DateTime<Utc>>,
    subscription_state: String,
    latest_order_id: Option<String>,
    linked_purchase_token: Option<String>,
    test_purchase: Option<serde_json::Value>,
    acknowledgement_state: Option<String>,
    external_account_identifiers: Option<ExternalAccountIdentifiers>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionLineItem {
    product_id: String,
    expiry_time: DateTime<Utc>,
    latest_successful_order_id: Option<String>,
    auto_renewing_plan: Option<AutoRenewingPlan>,
    offer_phase: Option<OfferPhase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AutoRenewingPlan {
    #[serde(default)]
    auto_renew_enabled: bool,
    recurring_price: Option<GoogleMoney>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OfferPhase {
    free_trial: Option<serde_json::Value>,
    introductory_price: Option<serde_json::Value>,
}

/// Claims of the Pub/Sub push OIDC token that are checked beyond the standard ones.
#[derive(Deserialize)]
struct PushClaims {
    email: Option<String>,
    #[serde(default)]
    email_verified: bool,
}

/// `google.type.Money`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleMoney {
    currency_code: String,
    units: Option<String>,
    #[serde(default)]
    nanos: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExternalAccountIdentifiers {
    obfuscated_external_account_id: Option<String>,
}

/// `purchases.productsv2` resource (the fields we use).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProductV2 {
    #[serde(default)]
    product_line_item: Vec<ProductLineItem>,
    purchase_state_context: Option<PurchaseStateContext>,
    test_purchase_context: Option<serde_json::Value>,
    order_id: Option<String>,
    obfuscated_external_account_id: Option<String>,
    purchase_completion_time: Option<DateTime<Utc>>,
    acknowledgement_state: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProductLineItem {
    product_id: String,
    product_offer_details: Option<ProductOfferDetails>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProductOfferDetails {
    consumption_state: Option<String>,
    rent_offer_details: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PurchaseStateContext {
    purchase_state: Option<String>,
}

/// Maps a subscriptionsv2 resource. `None` for pending purchases (not paid yet).
fn subscription_snapshot(
    token: &str,
    sub: &SubscriptionV2,
    as_of: DateTime<Utc>,
) -> Result<Option<PurchaseSnapshot>, AdapterError> {
    // Bundles are not supported: the line item that runs longest describes the purchase.
    let item = sub
        .line_items
        .iter()
        .max_by_key(|i| i.expiry_time)
        .ok_or_else(|| malformed("subscription without line items"))?;
    let expires_at = item.expiry_time;
    let renewing = item
        .auto_renewing_plan
        .as_ref()
        .is_some_and(|p| p.auto_renew_enabled);
    let (status, auto_renew) = match sub.subscription_state.as_str() {
        "SUBSCRIPTION_STATE_ACTIVE" => (PurchaseStatus::Active, renewing),
        "SUBSCRIPTION_STATE_CANCELED" if expires_at > Utc::now() => {
            (PurchaseStatus::Cancelled, false)
        }
        "SUBSCRIPTION_STATE_CANCELED" | "SUBSCRIPTION_STATE_EXPIRED" => {
            (PurchaseStatus::Expired, false)
        }
        "SUBSCRIPTION_STATE_IN_GRACE_PERIOD" => (PurchaseStatus::GracePeriod, renewing),
        "SUBSCRIPTION_STATE_ON_HOLD" => (PurchaseStatus::BillingRetry, renewing),
        "SUBSCRIPTION_STATE_PAUSED" => (PurchaseStatus::Paused, renewing),
        "SUBSCRIPTION_STATE_PENDING" | "SUBSCRIPTION_STATE_PENDING_PURCHASE_CANCELED" => {
            return Ok(None);
        }
        other => return Err(malformed(format!("unknown subscriptionState {other}"))),
    };
    let period_type = match &item.offer_phase {
        Some(p) if p.free_trial.is_some() => PeriodType::Trial,
        Some(p) if p.introductory_price.is_some() => PeriodType::Intro,
        _ => PeriodType::Normal,
    };
    Ok(Some(PurchaseSnapshot {
        store: Store::PlayStore,
        environment: environment(sub.test_purchase.is_some()),
        original_transaction_id: token.to_owned(),
        store_transaction_id: item
            .latest_successful_order_id
            .clone()
            .or_else(|| sub.latest_order_id.clone())
            .unwrap_or_else(|| token.to_owned()),
        product_id: item.product_id.clone(),
        kind: PurchaseKind::Subscription,
        status,
        period_type,
        // subscriptionsv2 exposes no current-period start: `startTime` is when the
        // subscription was first granted (Apple/Creem report the current period start).
        purchased_at: sub
            .start_time
            .ok_or_else(|| malformed("subscription without startTime"))?,
        expires_at: Some(expires_at),
        grace_period_expires_at: (status == PurchaseStatus::GracePeriod).then_some(expires_at),
        auto_renew,
        price: item
            .auto_renewing_plan
            .as_ref()
            .and_then(|p| p.recurring_price.as_ref())
            .and_then(price),
        customer_ref: customer_ref(
            sub.external_account_identifiers
                .as_ref()
                .and_then(|e| e.obfuscated_external_account_id.as_deref()),
        ),
        store_customer_id: None,
        as_of,
    }))
}

/// One-time products are lifetime purchases. Refused: consumed purchases (Play has no
/// consumable product type, but a consumed purchase is used up) and rentals (Play does
/// not report the rental's end), since either would otherwise grant forever.
fn lifetime_product(product: &ProductV2) -> Result<(), AdapterError> {
    for item in &product.product_line_item {
        let Some(offer) = &item.product_offer_details else {
            continue;
        };
        if offer.consumption_state.as_deref() == Some(CONSUMED) {
            return Err(AdapterError::Unsupported(format!(
                "Google Play product {} was consumed: entitle supports subscriptions and \
                 non-consumed one-time products only",
                item.product_id
            )));
        }
        if offer.rent_offer_details.is_some() {
            return Err(AdapterError::Unsupported(format!(
                "Google Play product {} is a rental: entitle supports subscriptions and \
                 one-time purchases only",
                item.product_id
            )));
        }
    }
    Ok(())
}

/// Maps a productsv2 resource. `None` for pending purchases (not paid yet).
fn product_snapshot(
    token: &str,
    product: &ProductV2,
    as_of: DateTime<Utc>,
) -> Result<Option<PurchaseSnapshot>, AdapterError> {
    let state = product
        .purchase_state_context
        .as_ref()
        .and_then(|c| c.purchase_state.as_deref());
    let status = match state {
        Some("PURCHASED") => PurchaseStatus::Active,
        // Google does not say why; access ends either way.
        Some("CANCELLED") => PurchaseStatus::Revoked,
        Some("PENDING") => return Ok(None),
        other => return Err(malformed(format!("unknown purchaseState {other:?}"))),
    };
    let item = product
        .product_line_item
        .first()
        .ok_or_else(|| malformed("product purchase without line items"))?;
    Ok(Some(PurchaseSnapshot {
        store: Store::PlayStore,
        environment: environment(product.test_purchase_context.is_some()),
        original_transaction_id: token.to_owned(),
        store_transaction_id: product.order_id.clone().unwrap_or_else(|| token.to_owned()),
        product_id: item.product_id.clone(),
        kind: PurchaseKind::OneTime,
        status,
        period_type: PeriodType::Normal,
        purchased_at: product
            .purchase_completion_time
            .ok_or_else(|| malformed("product purchase without purchaseCompletionTime"))?,
        expires_at: None,
        grace_period_expires_at: None,
        auto_renew: false,
        price: None,
        customer_ref: customer_ref(product.obfuscated_external_account_id.as_deref()),
        store_customer_id: None,
        as_of,
    }))
}

/// Only purchases the customer currently owns can (and must) be acknowledged.
fn grants_access(s: &PurchaseSnapshot) -> bool {
    matches!(
        s.status,
        PurchaseStatus::Active | PurchaseStatus::Cancelled | PurchaseStatus::GracePeriod
    )
}

fn environment(test: bool) -> Environment {
    if test {
        Environment::Sandbox
    } else {
        Environment::Production
    }
}

/// `units` + `nanos` → the currency's minor units.
fn price(m: &GoogleMoney) -> Option<Money> {
    let units: i64 = m.units.as_deref().unwrap_or("0").parse().ok()?;
    Some(Money {
        amount_minor: minor_from_units_nanos(units, m.nanos, &m.currency_code)?,
        currency: m.currency_code.clone(),
    })
}

fn customer_ref(token: Option<&str>) -> Option<CustomerId> {
    token.and_then(|t| t.parse().ok()).map(CustomerId)
}

fn signature(e: impl ToString) -> AdapterError {
    AdapterError::InvalidSignature(e.to_string())
}

fn malformed(e: impl ToString) -> AdapterError {
    AdapterError::Malformed(e.to_string())
}

// The request URL carries the purchase token: keep it out of errors (and logs).
fn upstream(e: reqwest::Error) -> AdapterError {
    AdapterError::Upstream(e.without_url().to_string())
}
