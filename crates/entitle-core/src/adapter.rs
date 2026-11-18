//! The contract between the server and the store adapter crates
//! (`entitle-creem`, `entitle-apple`, `entitle-google`).
//!
//! Adapters are stateless translators: they verify and parse store payloads and
//! call store APIs. They never touch the database; the server persists what they return.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{CustomerId, PurchaseSnapshot};

/// Metadata key under which the server's customer id travels through Creem checkouts.
pub const CREEM_CUSTOMER_METADATA_KEY: &str = "entitle_customer_id";

/// A raw inbound store webhook request.
#[derive(Debug, Clone, Default)]
pub struct InboundNotification {
    /// Header names are lowercase.
    pub headers: BTreeMap<String, String>,
    /// The exact bytes received (signatures are computed over these).
    pub body: Vec<u8>,
}

impl InboundNotification {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// A verified, parsed store notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNotification {
    /// The store's unique id for this delivery, used for deduplication:
    /// Creem event `id` (`evt_…`), Apple `notificationUUID`, Pub/Sub `message.messageId`.
    pub event_id: String,
    /// Current state of every purchase the notification is about. Empty for
    /// notifications that carry no purchase state (test pings, unrelated events).
    pub snapshots: Vec<PurchaseSnapshot>,
}

#[derive(Debug, Error)]
pub enum AdapterError {
    /// Signature / JWS / OIDC token did not verify. Server answers 401.
    #[error("invalid signature: {0}")]
    InvalidSignature(String),
    /// Payload could not be parsed. Server answers 400.
    #[error("malformed payload: {0}")]
    Malformed(String),
    /// The store does not know the identifier. Server answers 404.
    #[error("not found in store")]
    NotFound,
    /// Stored credentials are incomplete or were rejected by the store. Server answers 422.
    #[error("credentials: {0}")]
    Credentials(String),
    /// Store API failed or is unreachable; the store should retry. Server answers 502.
    #[error("store API error: {0}")]
    Upstream(String),
    /// A purchase type entitle cannot represent (e.g. a consumable). Server answers 422.
    #[error("unsupported purchase: {0}")]
    Unsupported(String),
    /// Phase-1 stub. Server answers 501.
    #[error("not implemented")]
    NotImplemented,
}

/// Every store adapter implements this.
#[async_trait]
pub trait StoreAdapter: Send + Sync {
    /// The project's credentials for this store (decrypted by the server).
    type Credentials: Send + Sync;

    /// Verify an inbound notification (signature / JWS chain / OIDC token) and
    /// turn it into normalized snapshots. May call the store API when the
    /// notification only carries an identifier (Google RTDN).
    async fn parse_notification(
        &self,
        credentials: &Self::Credentials,
        notification: &InboundNotification,
    ) -> Result<ParsedNotification, AdapterError>;

    /// Fetch the authoritative current state for an identifier the app submitted:
    /// Apple transaction id, Google purchase token, Creem subscription (`sub_…`)
    /// or checkout (`ch_…`) id. Returns [`AdapterError::NotFound`] if unknown.
    async fn fetch_purchases(
        &self,
        credentials: &Self::Credentials,
        id: &str,
    ) -> Result<Vec<PurchaseSnapshot>, AdapterError>;

    /// One authenticated store API call that proves the credentials work (for the
    /// admin "test connection"). Returns a human-readable summary of what was checked.
    async fn check_credentials(
        &self,
        credentials: &Self::Credentials,
    ) -> Result<String, AdapterError>;
}

/// Hosted-checkout stores (Creem only).
#[async_trait]
pub trait CheckoutAdapter: StoreAdapter {
    async fn create_checkout(
        &self,
        credentials: &Self::Credentials,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, AdapterError>;

    /// A customer-portal link for the store's customer id (`cust_…`).
    async fn portal_url(
        &self,
        credentials: &Self::Credentials,
        store_customer_id: &str,
    ) -> Result<String, AdapterError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutRequest {
    /// Store product id (`prod_…`).
    pub product_id: String,
    /// Must round-trip: put it into checkout metadata under
    /// [`CREEM_CUSTOMER_METADATA_KEY`] and return it as `PurchaseSnapshot::customer_ref`.
    pub customer: CustomerId,
    pub success_url: Option<String>,
    /// Extra developer metadata to attach to the checkout.
    pub metadata: BTreeMap<String, String>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutSession {
    pub id: String,
    pub url: String,
}

/// Creem API credentials of one project.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreemCredentials {
    pub api_key: String,
    pub webhook_secret: String,
    /// `true` = test mode (`test-api.creem.io`), `false` = live.
    pub test_mode: bool,
}

/// App Store Server API credentials of one project.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppleCredentials {
    pub issuer_id: String,
    pub key_id: String,
    /// Contents of the `.p8` private key (PEM).
    pub private_key: String,
    pub bundle_id: String,
    /// Required to verify production notifications.
    pub app_apple_id: Option<i64>,
}

/// Google Play Developer API credentials of one project.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleCredentials {
    /// The service account key file, as JSON.
    pub service_account_json: serde_json::Value,
    pub package_name: String,
    /// Expected `aud` claim of the Pub/Sub push OIDC token.
    pub pubsub_audience: String,
    /// Expected `email` claim of the Pub/Sub push OIDC token: the service account
    /// the push subscription authenticates as.
    pub pubsub_service_account_email: String,
}

// Secrets must never end up in logs.
impl fmt::Debug for CreemCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreemCredentials")
            .field("test_mode", &self.test_mode)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for AppleCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppleCredentials")
            .field("issuer_id", &self.issuer_id)
            .field("key_id", &self.key_id)
            .field("bundle_id", &self.bundle_id)
            .field("app_apple_id", &self.app_apple_id)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for GoogleCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleCredentials")
            .field("package_name", &self.package_name)
            .field("pubsub_audience", &self.pubsub_audience)
            .field(
                "pubsub_service_account_email",
                &self.pubsub_service_account_email,
            )
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive_on_query() {
        let mut n = InboundNotification::default();
        n.headers.insert("creem-signature".into(), "abc".into());
        assert_eq!(n.header("Creem-Signature"), Some("abc"));
        assert_eq!(n.header("x"), None);
    }

    #[test]
    fn debug_redacts_secrets() {
        let c = CreemCredentials {
            api_key: "creem_secret".into(),
            webhook_secret: "whsec".into(),
            test_mode: true,
        };
        let s = format!("{c:?}");
        assert!(!s.contains("creem_secret") && !s.contains("whsec"));
    }
}
