//! `entitle rotate-encryption-key`: re-encrypts every secret at rest with a new key.
//!
//! Runs against the database while the server is stopped: one transaction with the
//! sealed tables locked against writes. Each value is opened with the old key, sealed
//! with the new one under the same row-bound AAD, written, read back and opened with the
//! new key before commit. Any failure rolls everything back.

use anyhow::Context;
use entitle_core::{ProjectId, Store};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::adapters::credentials_aad;
use crate::crypto::Cipher;
use crate::db::parse_enum;
use crate::identity_token;
use crate::outbox::endpoint_secret_aad;

/// Number of re-encrypted values per table.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Rotated {
    pub store_credentials: usize,
    pub webhook_secrets: usize,
    pub identity_secrets: usize,
}

pub async fn rotate_encryption_key(
    db: &PgPool,
    old: &Cipher,
    new: &Cipher,
) -> anyhow::Result<Rotated> {
    let mut tx = db.begin().await?;
    // Readers may continue; writers (a server still running with the old key) wait.
    sqlx::query("LOCK TABLE projects, store_credentials, webhook_endpoints IN EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let mut rotated = Rotated::default();

    let rows: Vec<(Uuid, String, Vec<u8>)> =
        sqlx::query_as("SELECT project_id, store, sealed FROM store_credentials")
            .fetch_all(&mut *tx)
            .await?;
    for (project, store, sealed) in rows {
        let what = || format!("{store} credentials of project {project}");
        let aad = credentials_aad(ProjectId(project), parse_enum::<Store>(&store)?);
        let (resealed, plaintext) = reseal(old, new, &sealed, &aad).with_context(what)?;
        let (stored,): (Vec<u8>,) = sqlx::query_as(
            "UPDATE store_credentials SET sealed = $3 WHERE project_id = $1 AND store = $2
             RETURNING sealed",
        )
        .bind(project)
        .bind(&store)
        .bind(resealed)
        .fetch_one(&mut *tx)
        .await?;
        verify(new, &stored, &aad, &plaintext).with_context(what)?;
        rotated.store_credentials += 1;
    }

    let rows: Vec<(Uuid, Vec<u8>)> =
        sqlx::query_as("SELECT id, sealed_secret FROM webhook_endpoints")
            .fetch_all(&mut *tx)
            .await?;
    for (id, sealed) in rows {
        let what = || format!("signing secret of webhook endpoint {id}");
        let aad = endpoint_secret_aad(id);
        let (resealed, plaintext) = reseal(old, new, &sealed, &aad).with_context(what)?;
        let (stored,): (Vec<u8>,) = sqlx::query_as(
            "UPDATE webhook_endpoints SET sealed_secret = $2 WHERE id = $1 RETURNING sealed_secret",
        )
        .bind(id)
        .bind(resealed)
        .fetch_one(&mut *tx)
        .await?;
        verify(new, &stored, &aad, &plaintext).with_context(what)?;
        rotated.webhook_secrets += 1;
    }

    let rows: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT id, sealed_identity_secret FROM projects WHERE sealed_identity_secret IS NOT NULL",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (id, sealed) in rows {
        let what = || format!("identity secret of project {id}");
        let aad = identity_token::secret_aad(ProjectId(id));
        let (resealed, plaintext) = reseal(old, new, &sealed, &aad).with_context(what)?;
        let (stored,): (Vec<u8>,) = sqlx::query_as(
            "UPDATE projects SET sealed_identity_secret = $2 WHERE id = $1
             RETURNING sealed_identity_secret",
        )
        .bind(id)
        .bind(resealed)
        .fetch_one(&mut *tx)
        .await?;
        verify(new, &stored, &aad, &plaintext).with_context(what)?;
        rotated.identity_secrets += 1;
    }

    tx.commit().await?;
    Ok(rotated)
}

/// `(sealed with new, plaintext)`.
fn reseal(
    old: &Cipher,
    new: &Cipher,
    sealed: &[u8],
    aad: &[u8],
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let plaintext = old
        .open(sealed, aad)
        .context("cannot decrypt with the old key")?;
    Ok((new.seal(&plaintext, aad), plaintext))
}

/// The stored value must open with the new key to the same plaintext.
fn verify(new: &Cipher, stored: &[u8], aad: &[u8], plaintext: &[u8]) -> anyhow::Result<()> {
    let check = new
        .open(stored, aad)
        .context("re-encrypted value does not decrypt with the new key")?;
    anyhow::ensure!(
        check == plaintext,
        "re-encrypted value decrypts to different data"
    );
    Ok(())
}
