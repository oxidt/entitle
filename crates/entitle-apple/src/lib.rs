//! App Store adapter: App Store Server Notifications V2 (signed JWS) and the App Store Server API.
//!
//! JWS verification (x5c chain up to Apple Root CA G3, Apple's marker OIDs, certificate
//! validity at `signedDate`, ES256) and the API client (ES256 JWT) come from
//! `app-store-server-library`. This crate picks the environment, maps errors and turns
//! Apple's transactions into [`PurchaseSnapshot`]s. The contract is described in
//! `docs/adapters.md`.

use std::time::Duration;

use app_store_server_library::api_client::transport::{Transport, TransportError};
use app_store_server_library::app_store_server_api_client::{
    AppStoreServerApiClient, AppStoreServerApiClientError,
};
use app_store_server_library::crypto::CryptoProvider;
use app_store_server_library::crypto::jws;
use app_store_server_library::models::app_store_environment::Environment as AppleEnvironment;
use app_store_server_library::models::auto_renew_status::AutoRenewStatus;
use app_store_server_library::models::in_app_ownership_type::InAppOwnershipType;
use app_store_server_library::models::jws_renewal_info_decoded_payload::JWSRenewalInfoDecodedPayload;
use app_store_server_library::models::jws_transaction_decoded_payload::JWSTransactionDecodedPayload;
use app_store_server_library::models::notification_type_v2::NotificationTypeV2;
use app_store_server_library::models::offer_discount_type::OfferDiscountType;
use app_store_server_library::models::offer_type::OfferType;
use app_store_server_library::models::product_type::ProductType;
use app_store_server_library::models::response_body_v2_decoded_payload::ResponseBodyV2DecodedPayload;
use app_store_server_library::models::status::Status;
use app_store_server_library::signed_data_verifier::{SignedDataVerifier, SignedDataVerifierError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use entitle_core::currency::minor_from_milliunits;
use entitle_core::{
    AdapterError, AppleCredentials, CustomerId, Environment, InboundNotification, Money,
    ParsedNotification, PeriodType, PurchaseKind, PurchaseSnapshot, PurchaseStatus, StoreAdapter,
};
use serde::Deserialize;

/// Apple Root CA - G3 (DER), from <https://www.apple.com/certificateauthority/>.
/// SHA-256 fingerprint `63:34:3A:BF:…:3E:91:79`.
const APPLE_ROOT_CA_G3: &[u8] = include_bytes!("../certs/AppleRootCA-G3.cer");
const PRODUCTION_URL: &str = "https://api.storekit.apple.com";
const SANDBOX_URL: &str = "https://api.storekit-sandbox.apple.com";

#[derive(Debug, Clone)]
pub struct AppleAdapter {
    root_certificates: Vec<Vec<u8>>,
    production_url: String,
    sandbox_url: String,
    http: reqwest::Client,
}

impl Default for AppleAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleAdapter {
    pub fn new() -> Self {
        Self::with_config(vec![APPLE_ROOT_CA_G3.to_vec()], PRODUCTION_URL, SANDBOX_URL)
    }

    /// Trust other root certificates (DER) and talk to other API hosts. For tests.
    pub fn with_config(
        root_certificates: Vec<Vec<u8>>,
        production_url: impl Into<String>,
        sandbox_url: impl Into<String>,
    ) -> Self {
        AppleAdapter {
            root_certificates,
            production_url: production_url.into(),
            sandbox_url: sandbox_url.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("static reqwest configuration"),
        }
    }

    fn verifier(
        &self,
        credentials: &AppleCredentials,
        environment: AppleEnvironment,
    ) -> Result<SignedDataVerifier, AdapterError> {
        SignedDataVerifier::new(
            self.root_certificates.clone(),
            environment,
            credentials.bundle_id.clone(),
            credentials.app_apple_id,
            false, // check certificate validity at `signedDate`, as Apple recommends
        )
        .map_err(|_| {
            AdapterError::Credentials(
                "app_apple_id is required for the production App Store".into(),
            )
        })
    }

    fn client(
        &self,
        credentials: &AppleCredentials,
        environment: AppleEnvironment,
    ) -> AppStoreServerApiClient<HttpTransport> {
        let base_url = match environment {
            AppleEnvironment::Production => &self.production_url,
            _ => &self.sandbox_url,
        };
        AppStoreServerApiClient::new(
            credentials.private_key.as_bytes().to_vec(),
            &credentials.key_id,
            &credentials.issuer_id,
            &credentials.bundle_id,
            environment,
            HttpTransport {
                http: self.http.clone(),
                base_url: base_url.clone(),
            },
        )
        .expect("production and sandbox are API environments")
    }

    /// Current state of the purchase behind a verified transaction from `environment`.
    async fn purchase_state(
        &self,
        credentials: &AppleCredentials,
        environment: AppleEnvironment,
        signed_transaction: Option<String>,
    ) -> Result<PurchaseSnapshot, AdapterError> {
        let verifier = self.verifier(credentials, environment.clone())?;
        let tx = verify_response(signed_transaction, |s| {
            verifier.verify_and_decode_signed_transaction(s)
        })?;
        if purchase_kind(&tx)? == PurchaseKind::OneTime {
            let as_of = required(tx.signed_date, "signedDate")?;
            return snapshot(tx, None, None, as_of, false);
        }

        let original_id = required(tx.original_transaction_id.clone(), "originalTransactionId")?;
        let statuses = self
            .client(credentials, environment)
            .get_all_subscription_statuses(&original_id, None)
            .await
            .map_err(api_error)?;
        let item = statuses
            .data
            .into_iter()
            .flatten()
            .flat_map(|group| group.last_transactions.unwrap_or_default())
            .find(|item| item.original_transaction_id.as_deref() == Some(original_id.as_str()))
            .ok_or(AdapterError::NotFound)?;
        let tx = verify_response(item.signed_transaction_info, |s| {
            verifier.verify_and_decode_signed_transaction(s)
        })?;
        let renewal = item
            .signed_renewal_info
            .map(|s| verify_response(Some(s), |s| verifier.verify_and_decode_renewal_info(s)))
            .transpose()?;
        let as_of = required(tx.signed_date, "signedDate")?.max(
            renewal
                .as_ref()
                .and_then(|r| r.signed_date)
                .unwrap_or_default(),
        );
        snapshot(tx, renewal.as_ref(), item.status.as_ref(), as_of, false)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NotificationBody {
    signed_payload: String,
}

#[async_trait]
impl StoreAdapter for AppleAdapter {
    type Credentials = AppleCredentials;

    async fn parse_notification(
        &self,
        credentials: &AppleCredentials,
        notification: &InboundNotification,
    ) -> Result<ParsedNotification, AdapterError> {
        let body: NotificationBody = serde_json::from_slice(&notification.body)
            .map_err(|e| AdapterError::Malformed(format!("expected {{\"signedPayload\"}}: {e}")))?;
        // The environment decides which checks apply (app apple id in production), so it is read
        // before verification. Only Production or Sandbox can come out of this: the library skips
        // verification for Xcode/LocalTesting. The verifier re-checks it against the signed payload.
        let unverified: ResponseBodyV2DecodedPayload = jws::decode_payload(&body.signed_payload)
            .map_err(|e| AdapterError::InvalidSignature(format!("not a JWS: {e}")))?;
        let verifier = self.verifier(credentials, notification_environment(&unverified))?;
        let payload = verifier
            .verify_and_decode_notification(&body.signed_payload)
            .map_err(verify_error)?;

        let event_id = required(payload.notification_uuid, "notificationUUID")?;
        let empty = ParsedNotification {
            event_id: event_id.clone(),
            snapshots: vec![],
        };
        if payload.notification_type == Some(NotificationTypeV2::Test) {
            return Ok(empty);
        }
        let Some(data) = payload.data else {
            return Ok(empty);
        };
        let Some(signed_transaction) = data.signed_transaction_info else {
            return Ok(empty);
        };
        let tx = verifier
            .verify_and_decode_signed_transaction(&signed_transaction)
            .map_err(verify_error)?;
        // Consumables, non-renewing subscriptions: acknowledged, but nothing to store.
        if matches!(purchase_kind(&tx), Err(AdapterError::Unsupported(_))) {
            return Ok(empty);
        }
        let renewal = data
            .signed_renewal_info
            .map(|s| verifier.verify_and_decode_renewal_info(&s))
            .transpose()
            .map_err(verify_error)?;
        let as_of = required(payload.signed_date, "signedDate")?;
        // REVOKE: a Family Sharing entitlement was withdrawn, not refunded.
        let revoked = payload.notification_type == Some(NotificationTypeV2::Revoke);
        let snapshot = snapshot(tx, renewal.as_ref(), data.status.as_ref(), as_of, revoked)?;
        Ok(ParsedNotification {
            event_id,
            snapshots: vec![snapshot],
        })
    }

    async fn fetch_purchases(
        &self,
        credentials: &AppleCredentials,
        id: &str,
    ) -> Result<Vec<PurchaseSnapshot>, AdapterError> {
        // Transaction ids are numeric; anything else would end up in the request path.
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(AdapterError::NotFound);
        }
        check_private_key(credentials)?;

        for environment in [AppleEnvironment::Production, AppleEnvironment::Sandbox] {
            match self
                .client(credentials, environment.clone())
                .get_transaction_info(id)
                .await
            {
                Ok(response) => {
                    let snapshot = self
                        .purchase_state(credentials, environment, response.signed_transaction_info)
                        .await?;
                    return Ok(vec![snapshot]);
                }
                // Apple: a transaction unknown to production may be a sandbox one.
                Err(e) if e.status() == 404 => continue,
                // Apple: production answers 401 for apps that have no release yet.
                Err(e)
                    if e.status() == 401 && matches!(environment, AppleEnvironment::Production) =>
                {
                    continue;
                }
                Err(e) => return Err(api_error(e)),
            }
        }
        Err(AdapterError::NotFound)
    }

    async fn check_credentials(
        &self,
        credentials: &AppleCredentials,
    ) -> Result<String, AdapterError> {
        check_private_key(credentials)?;
        // Needs a valid key, issuer and bundle id; Apple then notifies the sandbox URL.
        self.client(credentials, AppleEnvironment::Sandbox)
            .request_test_notification()
            .await
            .map_err(api_error)?;
        Ok(
            "The App Store Server API accepted the key. Apple is sending a test \
            notification to the sandbox notification URL."
                .into(),
        )
    }
}

fn check_private_key(credentials: &AppleCredentials) -> Result<(), AdapterError> {
    CryptoProvider::default_provider()
        .p256_signing
        .private_key(&credentials.private_key)
        .map(|_| ())
        .map_err(|_| AdapterError::Credentials("private_key is not a valid .p8 key".into()))
}

fn notification_environment(payload: &ResponseBodyV2DecodedPayload) -> AppleEnvironment {
    let environment = payload
        .data
        .as_ref()
        .and_then(|d| d.environment.clone())
        .or_else(|| payload.summary.as_ref().and_then(|s| s.environment.clone()))
        .or_else(|| {
            payload
                .app_data
                .as_ref()
                .and_then(|a| a.environment.clone())
        });
    match environment {
        Some(AppleEnvironment::Sandbox) => AppleEnvironment::Sandbox,
        _ => AppleEnvironment::Production,
    }
}

/// Auto-renewable subscriptions and non-consumables (lifetime). Anything else is refused:
/// consumables are used up, and a non-renewing subscription's term is set by the app, not
/// reported by Apple, so either would otherwise grant forever.
fn purchase_kind(tx: &JWSTransactionDecodedPayload) -> Result<PurchaseKind, AdapterError> {
    match &tx.r#type {
        Some(ProductType::AutoRenewableSubscription) => Ok(PurchaseKind::Subscription),
        Some(ProductType::NonConsumable) => Ok(PurchaseKind::OneTime),
        other => Err(AdapterError::Unsupported(format!(
            "App Store product type {}: entitle supports auto-renewable subscriptions and \
             non-consumables only",
            other
                .as_ref()
                .map_or("missing".to_owned(), |t| serde_json::to_string(t)
                    .unwrap_or_default())
        ))),
    }
}

/// Normalize one verified transaction (+ renewal info and Apple's subscription status).
fn snapshot(
    tx: JWSTransactionDecodedPayload,
    renewal: Option<&JWSRenewalInfoDecodedPayload>,
    status: Option<&Status>,
    as_of: DateTime<Utc>,
    revoked: bool,
) -> Result<PurchaseSnapshot, AdapterError> {
    let kind = purchase_kind(&tx)?;
    let expires_at = match kind {
        PurchaseKind::Subscription => Some(required(tx.expires_date, "expiresDate")?),
        PurchaseKind::OneTime => None,
    };
    let auto_renew = kind == PurchaseKind::Subscription
        && renewal.and_then(|r| r.auto_renew_status.as_ref()) == Some(&AutoRenewStatus::On);
    let renewing = if auto_renew {
        PurchaseStatus::Active
    } else {
        PurchaseStatus::Cancelled
    };

    // A family member's access withdrawn (REVOKE, or the same state read from the API)
    // is a revocation, whatever refund reason the purchaser's transaction carries.
    let family_shared = tx.in_app_ownership_type == Some(InAppOwnershipType::FamilyShared);
    let status = if revoked {
        PurchaseStatus::Revoked
    } else if tx.revocation_date.is_some() {
        if tx.revocation_reason.is_some() && !family_shared {
            PurchaseStatus::Refunded
        } else {
            PurchaseStatus::Revoked
        }
    } else if kind == PurchaseKind::OneTime {
        PurchaseStatus::Active
    } else {
        match status {
            Some(Status::Active) => renewing,
            Some(Status::Expired) => PurchaseStatus::Expired,
            Some(Status::BillingRetry) => PurchaseStatus::BillingRetry,
            Some(Status::BillingGracePeriod) => PurchaseStatus::GracePeriod,
            Some(Status::Revoked) => PurchaseStatus::Revoked,
            // Apple always sends a status for subscriptions; fall back to the dates.
            Some(Status::NotSupported(_)) | None if expires_at <= Some(as_of) => {
                PurchaseStatus::Expired
            }
            Some(Status::NotSupported(_)) | None => renewing,
        }
    };
    let grace_period_expires_at = match status {
        PurchaseStatus::GracePeriod => renewal.and_then(|r| r.grace_period_expires_date),
        _ => None,
    };
    let period_type = if tx.offer_discount_type == Some(OfferDiscountType::FreeTrial) {
        PeriodType::Trial
    } else if tx.offer_type == Some(OfferType::IntroductoryOffer) {
        PeriodType::Intro
    } else {
        PeriodType::Normal
    };
    let environment = match tx.environment {
        Some(AppleEnvironment::Sandbox) => Environment::Sandbox,
        _ => Environment::Production,
    };

    Ok(PurchaseSnapshot {
        store: entitle_core::Store::AppStore,
        environment,
        original_transaction_id: required(tx.original_transaction_id, "originalTransactionId")?,
        store_transaction_id: required(tx.transaction_id, "transactionId")?,
        product_id: required(tx.product_id, "productId")?,
        kind,
        status,
        period_type,
        purchased_at: required(tx.purchase_date, "purchaseDate")?,
        expires_at,
        grace_period_expires_at,
        auto_renew,
        // Apple reports milliunits (4990 = 4.99).
        price: tx.price.zip(tx.currency).and_then(|(price, currency)| {
            Some(Money {
                amount_minor: minor_from_milliunits(price, &currency)?,
                currency,
            })
        }),
        customer_ref: tx.app_account_token.map(CustomerId),
        store_customer_id: None,
        as_of,
    })
}

fn required<T>(value: Option<T>, field: &str) -> Result<T, AdapterError> {
    value.ok_or_else(|| AdapterError::Malformed(format!("missing {field}")))
}

fn verify_error(e: SignedDataVerifierError) -> AdapterError {
    match e {
        SignedDataVerifierError::InternalDeserializationError(e) => {
            AdapterError::Malformed(e.to_string())
        }
        SignedDataVerifierError::InvalidAppIdentifier => AdapterError::InvalidSignature(
            "signed for another app (bundle id / app apple id)".into(),
        ),
        e => AdapterError::InvalidSignature(e.to_string()),
    }
}

/// Signed data returned by the App Store Server API must verify too; if it does not,
/// the store (or the path to it) is broken, which is not the SDK caller's fault.
fn verify_response<T>(
    signed: Option<String>,
    verify: impl FnOnce(&str) -> Result<T, SignedDataVerifierError>,
) -> Result<T, AdapterError> {
    let signed =
        signed.ok_or_else(|| AdapterError::Upstream("response without signed data".into()))?;
    verify(&signed)
        .map_err(|e| AdapterError::Upstream(format!("unverifiable App Store response: {e}")))
}

fn api_error(e: AppStoreServerApiClientError) -> AdapterError {
    match e.status() {
        404 => AdapterError::NotFound,
        401 | 403 => AdapterError::Credentials(format!("App Store rejected the API key: {e}")),
        _ => AdapterError::Upstream(e.to_string()),
    }
}

/// The library's HTTP seam: sends its requests via our reqwest client to the configured host.
struct HttpTransport {
    http: reqwest::Client,
    base_url: String,
}

impl Transport for HttpTransport {
    async fn send(
        &self,
        request: http::Request<Vec<u8>>,
    ) -> Result<http::Response<Vec<u8>>, TransportError> {
        let (parts, body) = request.into_parts();
        // The library builds URLs for Apple's hosts; keep the path, swap the host.
        let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        let network = |e: reqwest::Error| TransportError::NetworkError(e.to_string());
        let response = self
            .http
            .request(parts.method, format!("{}{path}", self.base_url))
            .headers(parts.headers)
            .body(body)
            .send()
            .await
            .map_err(network)?;
        let status = response.status();
        let body = response.bytes().await.map_err(network)?;
        http::Response::builder()
            .status(status)
            .body(body.to_vec())
            .map_err(|e| TransportError::InvalidResponse(e.to_string()))
    }
}
