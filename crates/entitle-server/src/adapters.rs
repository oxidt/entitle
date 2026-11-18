//! Wiring between the server and the store adapter crates, plus encrypted
//! storage of per-project store credentials.

use std::sync::Arc;

use entitle_core::{
    AdapterError, AppleCredentials, CheckoutAdapter, CheckoutRequest, CheckoutSession,
    CreemCredentials, GoogleCredentials, InboundNotification, ParsedNotification, ProjectId,
    PurchaseSnapshot, Store, StoreAdapter,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::PgPool;

use crate::crypto::Cipher;
use crate::error::{ApiError, ApiResult};

pub type CreemAdapterRef = Arc<dyn CheckoutAdapter<Credentials = CreemCredentials>>;
pub type AppleAdapterRef = Arc<dyn StoreAdapter<Credentials = AppleCredentials>>;
pub type GoogleAdapterRef = Arc<dyn StoreAdapter<Credentials = GoogleCredentials>>;

/// One adapter per store. Tests swap in fakes.
#[derive(Clone)]
pub struct Adapters {
    pub creem: CreemAdapterRef,
    pub apple: AppleAdapterRef,
    pub google: GoogleAdapterRef,
}

impl Adapters {
    pub fn real() -> Self {
        Adapters {
            creem: Arc::new(entitle_creem::CreemAdapter::new()),
            apple: Arc::new(entitle_apple::AppleAdapter::new()),
            google: Arc::new(entitle_google::GoogleAdapter::new()),
        }
    }

    pub async fn parse_notification(
        &self,
        db: &PgPool,
        cipher: &Cipher,
        project: ProjectId,
        store: Store,
        notification: &InboundNotification,
    ) -> ApiResult<ParsedNotification> {
        let parsed = match store {
            Store::Creem => {
                let c = load_credentials(db, cipher, project, store).await?;
                self.creem.parse_notification(&c, notification).await?
            }
            Store::AppStore => {
                let c = load_credentials(db, cipher, project, store).await?;
                self.apple.parse_notification(&c, notification).await?
            }
            Store::PlayStore => {
                let c = load_credentials(db, cipher, project, store).await?;
                self.google.parse_notification(&c, notification).await?
            }
        };
        check_store(store, &parsed.snapshots)?;
        Ok(parsed)
    }

    pub async fn fetch_purchases(
        &self,
        db: &PgPool,
        cipher: &Cipher,
        project: ProjectId,
        store: Store,
        id: &str,
    ) -> ApiResult<Vec<PurchaseSnapshot>> {
        let snapshots = match store {
            Store::Creem => {
                let c = load_credentials(db, cipher, project, store).await?;
                self.creem.fetch_purchases(&c, id).await?
            }
            Store::AppStore => {
                let c = load_credentials(db, cipher, project, store).await?;
                self.apple.fetch_purchases(&c, id).await?
            }
            Store::PlayStore => {
                let c = load_credentials(db, cipher, project, store).await?;
                self.google.fetch_purchases(&c, id).await?
            }
        };
        check_store(store, &snapshots)?;
        Ok(snapshots)
    }

    /// One authenticated call to the store with the project's credentials. The outer
    /// error is ours (e.g. no credentials); the inner one is the store's verdict.
    pub async fn check_credentials(
        &self,
        db: &PgPool,
        cipher: &Cipher,
        project: ProjectId,
        store: Store,
    ) -> ApiResult<Result<String, AdapterError>> {
        Ok(match store {
            Store::Creem => {
                let c = load_credentials(db, cipher, project, store).await?;
                self.creem.check_credentials(&c).await
            }
            Store::AppStore => {
                let c = load_credentials(db, cipher, project, store).await?;
                self.apple.check_credentials(&c).await
            }
            Store::PlayStore => {
                let c = load_credentials(db, cipher, project, store).await?;
                self.google.check_credentials(&c).await
            }
        })
    }

    pub async fn create_checkout(
        &self,
        db: &PgPool,
        cipher: &Cipher,
        project: ProjectId,
        request: &CheckoutRequest,
    ) -> ApiResult<CheckoutSession> {
        let c = load_credentials(db, cipher, project, Store::Creem).await?;
        Ok(self.creem.create_checkout(&c, request).await?)
    }

    pub async fn portal_url(
        &self,
        db: &PgPool,
        cipher: &Cipher,
        project: ProjectId,
        store_customer_id: &str,
    ) -> ApiResult<String> {
        let c = load_credentials(db, cipher, project, Store::Creem).await?;
        Ok(self.creem.portal_url(&c, store_customer_id).await?)
    }
}

/// An adapter must only report purchases of its own store.
fn check_store(store: Store, snapshots: &[PurchaseSnapshot]) -> ApiResult<()> {
    match snapshots.iter().find(|s| s.store != store) {
        Some(s) => Err(ApiError::Internal(anyhow::anyhow!(
            "{store} adapter returned a {} purchase",
            s.store
        ))),
        None => Ok(()),
    }
}

pub fn credentials_aad(project: ProjectId, store: Store) -> Vec<u8> {
    format!("store_credentials:{project}:{store}").into_bytes()
}

/// Validates `json` as the credentials type of `store`, then stores it sealed.
pub async fn save_credentials(
    db: &PgPool,
    cipher: &Cipher,
    project: ProjectId,
    store: Store,
    json: serde_json::Value,
) -> ApiResult<()> {
    fn typed<T: DeserializeOwned + Serialize>(json: serde_json::Value) -> ApiResult<Vec<u8>> {
        let creds: T = serde_json::from_value(json)
            .map_err(|e| ApiError::BadRequest(format!("invalid credentials: {e}")))?;
        Ok(serde_json::to_vec(&creds).expect("credentials serialize"))
    }
    let plaintext = match store {
        Store::Creem => typed::<CreemCredentials>(json)?,
        Store::AppStore => typed::<AppleCredentials>(json)?,
        Store::PlayStore => typed::<GoogleCredentials>(json)?,
    };
    let sealed = cipher.seal(&plaintext, &credentials_aad(project, store));
    sqlx::query(
        "INSERT INTO store_credentials (project_id, store, sealed) VALUES ($1, $2, $3)
         ON CONFLICT (project_id, store) DO UPDATE SET sealed = EXCLUDED.sealed, updated_at = now()",
    )
    .bind(project.0)
    .bind(store.as_str())
    .bind(sealed)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn load_credentials<T: DeserializeOwned>(
    db: &PgPool,
    cipher: &Cipher,
    project: ProjectId,
    store: Store,
) -> ApiResult<T> {
    let sealed: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT sealed FROM store_credentials WHERE project_id = $1 AND store = $2")
            .bind(project.0)
            .bind(store.as_str())
            .fetch_optional(db)
            .await?;
    let Some((sealed,)) = sealed else {
        return Err(ApiError::not_found(format!(
            "{store} is not configured for this project"
        )));
    };
    let plaintext = cipher.open(&sealed, &credentials_aad(project, store))?;
    Ok(serde_json::from_slice(&plaintext).map_err(anyhow::Error::from)?)
}
