//! Outgoing webhooks: a transactional outbox in Postgres, drained by a worker
//! that leases rows (`FOR UPDATE SKIP LOCKED` in a short transaction), sends
//! without holding locks, and retries with exponential backoff.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use entitle_core::{CustomerId, EventType, ProjectId, PurchaseSnapshot};
use sqlx::{FromRow, PgConnection, PgPool};
use tokio::sync::watch;
use uuid::Uuid;

use crate::crypto::{Cipher, signature_header};
use crate::customers::{self, PurchaseView};

pub const SIGNATURE_HEADER: &str = "entitle-signature";
pub const MAX_ATTEMPTS: i32 = 15;
const BATCH: i64 = 20;
const IDLE_POLL: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a claimed batch is reserved for its worker. Longer than a whole batch
/// of timed-out sends (`BATCH` × `REQUEST_TIMEOUT`); after it, a crashed worker's
/// rows are claimed again.
pub const LEASE: Duration = Duration::from_secs(5 * 60);

/// Delay before retry number `attempts` (1-based): 30s, 1m, 2m, … capped at 6h.
/// 15 attempts span roughly 1.4 days.
pub fn backoff(attempts: i32) -> chrono::Duration {
    let exp = attempts.clamp(1, 20) - 1;
    let secs = 30i64.saturating_mul(1i64 << exp).min(6 * 3600);
    chrono::Duration::seconds(secs)
}

/// How outgoing webhooks reach their endpoints.
pub enum Egress {
    /// Public https hosts only: every resolved address is checked and the request
    /// is pinned to those addresses, so DNS cannot point it into the private network.
    Public,
    /// Any host, for operators whose endpoints live on their private network.
    Any(reqwest::Client),
}

impl Egress {
    pub fn new(allow_private: bool) -> Self {
        if allow_private {
            Egress::Any(
                reqwest::Client::builder()
                    // A redirect must not lead the signed payload somewhere else.
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .expect("static reqwest config"),
            )
        } else {
            Egress::Public
        }
    }

    async fn client_for(&self, url: &reqwest::Url) -> Result<reqwest::Client, String> {
        match self {
            Egress::Any(http) => Ok(http.clone()),
            Egress::Public => {
                // A stalled resolver must not hold up the (sequential) worker.
                let pinned =
                    tokio::time::timeout(REQUEST_TIMEOUT, oxidt_egress::guard_host_async(url))
                        .await
                        .map_err(|_| "DNS lookup timed out".to_string())?
                        .map_err(|e| e.to_string())?;
                let timeouts = oxidt_egress::Timeouts {
                    connect: REQUEST_TIMEOUT,
                    total: REQUEST_TIMEOUT,
                };
                oxidt_egress::pinned_client(&pinned, timeouts).map_err(|e| e.to_string())
            }
        }
    }
}

/// Checks a webhook URL at registration; [`Egress::Public`] re-checks the
/// resolved addresses on every delivery.
pub fn check_endpoint_url(url: &str, allow_private: bool) -> Result<reqwest::Url, String> {
    if allow_private {
        let url = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("url must be http(s)".into());
        }
        return Ok(url);
    }
    let policy = oxidt_egress::UrlPolicy {
        allow_query: true,
        ..oxidt_egress::UrlPolicy::default()
    };
    let url = oxidt_egress::check_url(url, &policy).map_err(|e| e.to_string())?;
    if oxidt_egress::literal_ip_is_forbidden(&url) {
        return Err(
            "url must be a public host (set ENTITLE_WEBHOOK_ALLOW_PRIVATE for internal endpoints)"
                .into(),
        );
    }
    Ok(url)
}

pub fn endpoint_secret_aad(endpoint: Uuid) -> Vec<u8> {
    format!("webhook_endpoint:{endpoint}").into_bytes()
}

/// Queues one event for every webhook endpoint of the project, inside the
/// caller's transaction (so events exist iff the state change committed).
pub async fn enqueue(
    conn: &mut PgConnection,
    project: ProjectId,
    event: EventType,
    customer: CustomerId,
    purchase: &PurchaseSnapshot,
    transferred_from: Option<CustomerId>,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let endpoints: Vec<(Uuid,)> =
        sqlx::query_as("SELECT id FROM webhook_endpoints WHERE project_id = $1")
            .bind(project.0)
            .fetch_all(&mut *conn)
            .await?;
    if endpoints.is_empty() {
        return Ok(());
    }
    let event_id = Uuid::new_v4();
    let mut payload = serde_json::json!({
        "id": event_id,
        "type": event,
        "created_at": now,
        "project_id": project,
        "customer": {
            "id": customer,
            "app_user_ids": customers::aliases(conn, customer).await?,
        },
        "purchase": PurchaseView::from(purchase),
        "entitlements": customers::entitlements(conn, project, customer, now).await?,
    });
    if let Some(from) = transferred_from {
        payload["transferred_from"] = serde_json::json!({ "customer_id": from });
    }
    for (endpoint,) in endpoints {
        sqlx::query(
            "INSERT INTO outbox (id, endpoint_id, event_id, event_type, customer_id, payload, created_at, next_attempt_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $7)",
        )
        .bind(Uuid::new_v4())
        .bind(endpoint)
        .bind(event_id)
        .bind(event.as_str())
        .bind(customer.0)
        .bind(&payload)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    tracing::info!(%project, %customer, event = %event, "queued webhook event");
    Ok(())
}

#[derive(FromRow)]
struct Due {
    id: Uuid,
    payload: serde_json::Value,
    attempts: i32,
    endpoint_id: Uuid,
    url: String,
    sealed_secret: Vec<u8>,
}

/// Delivers one batch of due webhooks. Returns how many were attempted.
///
/// Rows are claimed with a lease in one short transaction and each result is
/// recorded in another, so no locks are held while sending.
pub async fn deliver_due(db: &PgPool, cipher: &Cipher, egress: &Egress) -> anyhow::Result<usize> {
    let due: Vec<Due> = sqlx::query_as(
        "WITH claimed AS (
            SELECT id FROM outbox
            WHERE status = 'pending' AND next_attempt_at <= now()
              AND (locked_until IS NULL OR locked_until <= now())
            ORDER BY next_attempt_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
         )
         UPDATE outbox o SET locked_until = now() + make_interval(secs => $2)
         FROM claimed, webhook_endpoints e
         WHERE o.id = claimed.id AND e.id = o.endpoint_id
         RETURNING o.id, o.payload, o.attempts, e.id AS endpoint_id, e.url, e.sealed_secret",
    )
    .bind(BATCH)
    .bind(LEASE.as_secs_f64())
    .fetch_all(db)
    .await?;

    for d in &due {
        let attempts = d.attempts + 1;
        match send(cipher, egress, d).await {
            Ok(()) => {
                sqlx::query(
                    "UPDATE outbox SET status = 'delivered', attempts = $2, delivered_at = now(),
                        last_error = NULL, locked_until = NULL
                     WHERE id = $1",
                )
                .bind(d.id)
                .bind(attempts)
                .execute(db)
                .await?;
            }
            Err(error) => {
                let status = if attempts >= MAX_ATTEMPTS {
                    "failed"
                } else {
                    "pending"
                };
                tracing::warn!(outbox_id = %d.id, endpoint_id = %d.endpoint_id, attempts, %error, "webhook delivery failed");
                sqlx::query(
                    "UPDATE outbox SET status = $2, attempts = $3, last_error = $4,
                        next_attempt_at = now() + make_interval(secs => $5), locked_until = NULL
                     WHERE id = $1",
                )
                .bind(d.id)
                .bind(status)
                .bind(attempts)
                .bind(error)
                .bind(backoff(attempts).num_seconds() as f64)
                .execute(db)
                .await?;
            }
        }
    }
    Ok(due.len())
}

async fn send(cipher: &Cipher, egress: &Egress, d: &Due) -> Result<(), String> {
    let secret = cipher
        .open(&d.sealed_secret, &endpoint_secret_aad(d.endpoint_id))
        .map_err(|e| e.to_string())?;
    let secret = String::from_utf8(secret).map_err(|e| e.to_string())?;
    let body = serde_json::to_vec(&d.payload).map_err(|e| e.to_string())?;
    let signature = signature_header(&secret, Utc::now().timestamp(), &body);
    let url = reqwest::Url::parse(&d.url).map_err(|e| e.to_string())?;
    let response = egress
        .client_for(&url)
        .await?
        .post(url)
        .timeout(REQUEST_TIMEOUT)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(SIGNATURE_HEADER, signature)
        .body(body)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("endpoint answered {}", response.status()))
    }
}

/// Drains the outbox until `shutdown` flips to `true`.
pub async fn run_worker(
    db: PgPool,
    cipher: Arc<Cipher>,
    allow_private: bool,
    mut shutdown: watch::Receiver<bool>,
) {
    let egress = Egress::new(allow_private);
    tracing::info!("webhook worker started");
    loop {
        let sleep = match deliver_due(&db, &cipher, &egress).await {
            Ok(0) => IDLE_POLL,
            Ok(_) => Duration::ZERO,
            Err(error) => {
                tracing::error!(?error, "webhook worker batch failed");
                IDLE_POLL
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(sleep) => {}
            _ = shutdown.changed() => break,
        }
        if *shutdown.borrow() {
            break;
        }
    }
    tracing::info!("webhook worker stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1).num_seconds(), 30);
        assert_eq!(backoff(2).num_seconds(), 60);
        assert_eq!(backoff(5).num_seconds(), 480);
        assert_eq!(backoff(14).num_seconds(), 6 * 3600);
        assert_eq!(backoff(100).num_seconds(), 6 * 3600);
    }

    #[test]
    fn endpoint_urls_are_public_https_unless_private_is_allowed() {
        assert!(check_endpoint_url("https://example.com/hook?token=x", false).is_ok());
        for url in [
            "http://example.com/hook",
            "https://user:pw@example.com/hook",
            "https://127.0.0.1/hook",
            "https://169.254.169.254/latest/meta-data/",
            "https://[::1]/hook",
        ] {
            assert!(check_endpoint_url(url, false).is_err(), "{url}");
        }
        assert!(check_endpoint_url("http://backend:8080/hook", true).is_ok());
        assert!(check_endpoint_url("ftp://example.com", true).is_err());
    }

    #[tokio::test]
    async fn public_egress_refuses_hosts_resolving_to_private_addresses() {
        let url = reqwest::Url::parse("https://localhost/hook").unwrap();
        assert!(Egress::Public.client_for(&url).await.is_err());
    }
}
