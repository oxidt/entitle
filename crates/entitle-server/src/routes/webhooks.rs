//! `POST /webhooks/{creem|apple|google}/{project_id}`: inbound store notifications.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use chrono::Utc;
use entitle_core::{InboundNotification, ProjectId, Store};
use uuid::Uuid;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Path;
use crate::purchases::{Source, apply_snapshot};

pub fn router() -> Router<AppState> {
    Router::new().route("/{store}/{project_id}", post(inbound))
}

async fn inbound(
    State(state): State<AppState>,
    Path((slug, project_id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<StatusCode> {
    let body = body?;
    let store =
        Store::from_webhook_slug(&slug).ok_or_else(|| ApiError::not_found("unknown store"))?;
    let project = ProjectId(project_id);
    let notification = InboundNotification {
        headers: headers
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect(),
        body: body.to_vec(),
    };
    // Credentials lookup doubles as the project existence check.
    let parsed = state
        .adapters
        .parse_notification(&state.db, &state.cipher, project, store, &notification)
        .await?;

    let mut tx = state.db.begin().await?;
    let fresh = sqlx::query(
        "INSERT INTO store_events (project_id, store, event_id) VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(project.0)
    .bind(store.as_str())
    .bind(&parsed.event_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !fresh {
        tracing::info!(%project, %store, event_id = %parsed.event_id, "duplicate store notification");
        return Ok(StatusCode::OK);
    }
    let now = Utc::now();
    for snapshot in &parsed.snapshots {
        apply_snapshot(&mut tx, project, snapshot, Source::Notification, now).await?;
    }
    tx.commit().await?;
    tracing::info!(
        %project, %store, event_id = %parsed.event_id, purchases = parsed.snapshots.len(),
        "store notification applied"
    );
    Ok(StatusCode::OK)
}
