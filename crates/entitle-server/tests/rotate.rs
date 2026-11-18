mod common;

use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::TestApp;
use entitle_core::{ProjectId, Store};
use entitle_server::Cipher;
use entitle_server::adapters::credentials_aad;
use entitle_server::identity_token::secret_aad;
use entitle_server::outbox::endpoint_secret_aad;
use entitle_server::rotate::{Rotated, rotate_encryption_key};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

fn key(byte: u8) -> Cipher {
    Cipher::from_base64(&STANDARD.encode([byte; 32])).unwrap()
}

/// TestApp (three store credentials) plus a webhook endpoint and an identity secret.
/// Returns the webhook id, its signing secret and the identity secret.
async fn setup(app: &TestApp) -> (Uuid, String, String) {
    let base = format!("/admin/v1/projects/{}", app.project);
    let (s, hook) = app
        .admin(
            Method::POST,
            &format!("{base}/webhooks"),
            json!({"url": "http://hooks.test/entitle"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{hook}");
    let (s, identity) = app
        .admin(Method::POST, &format!("{base}/identity-secret"), json!({}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{identity}");
    (
        hook["id"].as_str().unwrap().parse().unwrap(),
        hook["secret"].as_str().unwrap().into(),
        identity["identity_secret"].as_str().unwrap().into(),
    )
}

/// Every sealed value with its AAD, in a stable order.
async fn sealed(db: &PgPool) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut all = Vec::new();
    let creds: Vec<(Uuid, String, Vec<u8>)> =
        sqlx::query_as("SELECT project_id, store, sealed FROM store_credentials ORDER BY store")
            .fetch_all(db)
            .await
            .unwrap();
    for (project, store, sealed) in creds {
        let store: Store = serde_json::from_value(Value::String(store)).unwrap();
        all.push((sealed, credentials_aad(ProjectId(project), store)));
    }
    let hooks: Vec<(Uuid, Vec<u8>)> =
        sqlx::query_as("SELECT id, sealed_secret FROM webhook_endpoints ORDER BY id")
            .fetch_all(db)
            .await
            .unwrap();
    all.extend(
        hooks
            .into_iter()
            .map(|(id, s)| (s, endpoint_secret_aad(id))),
    );
    let projects: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT id, sealed_identity_secret FROM projects
         WHERE sealed_identity_secret IS NOT NULL ORDER BY id",
    )
    .fetch_all(db)
    .await
    .unwrap();
    all.extend(
        projects
            .into_iter()
            .map(|(id, s)| (s, secret_aad(ProjectId(id)))),
    );
    all
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn rotation_re_encrypts_every_secret(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (hook, hook_secret, identity_secret) = setup(&app).await;
    let (old, new) = (common::cipher(), key(7));
    let before: Vec<Vec<u8>> = sealed(&db)
        .await
        .iter()
        .map(|(s, aad)| old.open(s, aad).unwrap())
        .collect();
    assert_eq!(before.len(), 5);

    let rotated = rotate_encryption_key(&db, &old, &new).await.unwrap();
    assert_eq!(
        rotated,
        Rotated {
            store_credentials: 3,
            webhook_secrets: 1,
            identity_secrets: 1,
        }
    );
    let after = sealed(&db).await;
    for ((s, aad), plaintext) in after.iter().zip(&before) {
        assert_eq!(&new.open(s, aad).unwrap(), plaintext);
        assert!(old.open(s, aad).is_err(), "still readable with the old key");
    }
    let (s, _) = &after[3];
    assert_eq!(
        new.open(s, &endpoint_secret_aad(hook)).unwrap(),
        hook_secret.as_bytes()
    );
    let (s, aad) = &after[4];
    assert_eq!(new.open(s, aad).unwrap(), identity_secret.as_bytes());

    // Again with the old key: nothing decrypts, nothing changes.
    let err = rotate_encryption_key(&db, &old, &key(8)).await.unwrap_err();
    assert!(format!("{err:#}").contains("old key"), "{err:#}");
    assert_eq!(sealed(&db).await, after);
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn a_failure_midway_rolls_everything_back(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (hook, _, _) = setup(&app).await;
    // The webhook secret comes after the store credentials: seal it with another key.
    sqlx::query("UPDATE webhook_endpoints SET sealed_secret = $2 WHERE id = $1")
        .bind(hook)
        .bind(key(9).seal(b"whsec_other", &endpoint_secret_aad(hook)))
        .execute(&db)
        .await
        .unwrap();
    let before = sealed(&db).await;

    let err = rotate_encryption_key(&db, &common::cipher(), &key(7))
        .await
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains(&format!("webhook endpoint {hook}")),
        "{message}"
    );
    assert!(!message.contains(&STANDARD.encode([7u8; 32])), "{message}");
    assert_eq!(
        sealed(&db).await,
        before,
        "store credentials were rolled back"
    );
}
