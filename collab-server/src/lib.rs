pub mod db;

use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use sqlx::{sqlite::SqlitePool, Row};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tower_http::cors::CorsLayer;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;

const MAX_INSTANCE_ID_LEN: usize = 64;
const MAX_ROLE_LEN: usize = 256;
const MAX_CONTENT_LEN: usize = 4096;
const MAX_REFS_COUNT: usize = 20;
const MAX_REF_LEN: usize = 64;

#[derive(Debug, Serialize, Deserialize)]
pub struct MessageCreate {
    pub sender: String,
    pub recipient: String,
    pub content: String,
    #[serde(default)]
    pub refs: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub hash: String,
    pub sender: String,
    pub recipient: String,
    pub content: String,
    pub refs: Vec<String>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub instance_id: String,
    pub role: String,
    pub last_seen: DateTime<Utc>,
    pub message_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PresenceUpdate {
    pub role: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub token: Option<String>,
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.token {
        let provided = request
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));

        if provided != Some(expected.as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    next.run(request).await
}

pub fn create_app(state: AppState) -> Router {
    let shared_state = Arc::new(state);
    Router::new()
        .route("/", get(root))
        .route("/messages", post(create_message))
        .route("/messages/:instance_id", get(list_messages))
        .route("/history/:instance_id", get(get_history))
        .route("/roster", get(get_roster))
        .route("/presence/:instance_id", put(update_presence))
        .route("/messages/cleanup", delete(cleanup_old_messages))
        .layer(axum::middleware::from_fn_with_state(
            shared_state.clone(),
            auth_middleware,
        ))
        .layer(TimeoutLayer::new(StdDuration::from_secs(30)))
        .layer(CorsLayer::permissive())
        .with_state(shared_state)
}

#[cfg(test)]
pub async fn create_test_app() -> Router {
    let db = db::init_test_db().await.unwrap();
    let state = AppState { db, token: None };
    create_app(state)
}

async fn root() -> impl IntoResponse {
    Json(serde_json::json!({
        "service": "Claude IPC Server",
        "version": "0.1.0"
    }))
}

async fn list_messages(
    Path(instance_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Message>>, StatusCode> {
    if instance_id.len() > MAX_INSTANCE_ID_LEN {
        return Err(StatusCode::BAD_REQUEST);
    }

    let one_hour_ago = Utc::now() - Duration::hours(1);
    let cutoff_iso = one_hour_ago.to_rfc3339();

    let rows = sqlx::query(
        r#"
        SELECT id, hash, sender, recipient, content, refs, timestamp
        FROM messages
        WHERE recipient = ? AND timestamp >= ?
        ORDER BY timestamp DESC
        "#,
    )
    .bind(&instance_id)
    .bind(&cutoff_iso)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let messages = parse_message_rows(rows);
    Ok(Json(messages))
}

async fn get_history(
    Path(instance_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Message>>, StatusCode> {
    if instance_id.len() > MAX_INSTANCE_ID_LEN {
        return Err(StatusCode::BAD_REQUEST);
    }

    let one_hour_ago = Utc::now() - Duration::hours(1);
    let cutoff_iso = one_hour_ago.to_rfc3339();

    let rows = sqlx::query(
        r#"
        SELECT id, hash, sender, recipient, content, refs, timestamp
        FROM messages
        WHERE (recipient = ? OR sender = ?) AND timestamp >= ?
        ORDER BY timestamp ASC
        "#,
    )
    .bind(&instance_id)
    .bind(&instance_id)
    .bind(&cutoff_iso)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let messages = parse_message_rows(rows);
    Ok(Json(messages))
}

async fn get_roster(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<WorkerInfo>>, StatusCode> {
    let one_hour_ago = Utc::now() - Duration::hours(1);
    let cutoff_iso = one_hour_ago.to_rfc3339();

    let presence_rows = sqlx::query(
        r#"
        SELECT instance_id, role, last_seen
        FROM presence
        WHERE last_seen >= ?
        ORDER BY last_seen DESC
        "#,
    )
    .bind(&cutoff_iso)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let message_rows = sqlx::query(
        r#"
        SELECT sender, COUNT(*) as message_count
        FROM messages
        WHERE timestamp >= ?
        GROUP BY sender
        "#,
    )
    .bind(&cutoff_iso)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for row in message_rows {
        let sender: String = row.get("sender");
        let count: i64 = row.get("message_count");
        counts.insert(sender, count as usize);
    }

    let mut workers: Vec<WorkerInfo> = presence_rows
        .into_iter()
        .filter_map(|row| {
            let timestamp_str: String = row.get("last_seen");
            let last_seen = DateTime::parse_from_rfc3339(&timestamp_str)
                .ok()?
                .with_timezone(&Utc);
            let instance_id: String = row.get("instance_id");
            let message_count = counts.get(&instance_id).copied().unwrap_or(0);
            Some(WorkerInfo {
                instance_id,
                role: row.get("role"),
                last_seen,
                message_count,
            })
        })
        .collect();

    let present_ids: std::collections::HashSet<String> =
        workers.iter().map(|w| w.instance_id.clone()).collect();

    let sender_rows = sqlx::query(
        r#"
        SELECT sender as instance_id, MAX(timestamp) as last_seen, COUNT(*) as message_count
        FROM messages
        WHERE timestamp >= ?
        GROUP BY sender
        ORDER BY last_seen DESC
        "#,
    )
    .bind(&cutoff_iso)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    for row in sender_rows {
        let instance_id: String = row.get("instance_id");
        if present_ids.contains(&instance_id) {
            continue;
        }
        let timestamp_str: String = row.get("last_seen");
        if let Ok(last_seen) = DateTime::parse_from_rfc3339(&timestamp_str) {
            workers.push(WorkerInfo {
                instance_id,
                role: String::new(),
                last_seen: last_seen.with_timezone(&Utc),
                message_count: row.get::<i64, _>("message_count") as usize,
            });
        }
    }

    Ok(Json(workers))
}

async fn update_presence(
    Path(instance_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<PresenceUpdate>,
) -> Result<StatusCode, StatusCode> {
    let role = payload.role.unwrap_or_default();

    if instance_id.len() > MAX_INSTANCE_ID_LEN || role.len() > MAX_ROLE_LEN {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let now = Utc::now().to_rfc3339();

    sqlx::query(
        r#"
        INSERT INTO presence (instance_id, role, last_seen)
        VALUES (?, ?, ?)
        ON CONFLICT(instance_id) DO UPDATE SET
            role = CASE WHEN excluded.role != '' THEN excluded.role ELSE role END,
            last_seen = excluded.last_seen
        "#,
    )
    .bind(&instance_id)
    .bind(&role)
    .bind(&now)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(StatusCode::NO_CONTENT)
}

async fn create_message(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<MessageCreate>,
) -> Result<Json<Message>, StatusCode> {
    if payload.sender.len() > MAX_INSTANCE_ID_LEN
        || payload.recipient.len() > MAX_INSTANCE_ID_LEN
        || payload.content.len() > MAX_CONTENT_LEN
        || payload.refs.len() > MAX_REFS_COUNT
        || payload.refs.iter().any(|r| r.len() > MAX_REF_LEN)
    {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let timestamp = Utc::now();

    let mut hasher = Sha1::new();
    hasher.update(payload.sender.as_bytes());
    hasher.update(b"|");
    hasher.update(payload.recipient.as_bytes());
    hasher.update(b"|");
    hasher.update(payload.content.as_bytes());
    hasher.update(b"|");
    hasher.update(timestamp.to_rfc3339().as_bytes());
    let content_hash = format!("{:x}", hasher.finalize());

    let message_id = Uuid::new_v4().to_string();
    let timestamp_iso = timestamp.to_rfc3339();
    let refs_str = payload.refs.join(",");

    sqlx::query(
        r#"
        INSERT INTO messages (id, hash, sender, recipient, content, refs, timestamp)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&message_id)
    .bind(&content_hash)
    .bind(&payload.sender)
    .bind(&payload.recipient)
    .bind(&payload.content)
    .bind(&refs_str)
    .bind(&timestamp_iso)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(Message {
        id: message_id,
        hash: content_hash,
        sender: payload.sender,
        recipient: payload.recipient,
        content: payload.content,
        refs: payload.refs,
        timestamp,
    }))
}

async fn cleanup_old_messages(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let one_hour_ago = Utc::now() - Duration::hours(1);
    let cutoff_iso = one_hour_ago.to_rfc3339();

    let result = sqlx::query("DELETE FROM messages WHERE timestamp < ?")
        .bind(&cutoff_iso)
        .execute(&state.db)
        .await
        .map_err(|e| {
            tracing::error!("Database error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    sqlx::query("DELETE FROM presence WHERE last_seen < ?")
        .bind(&cutoff_iso)
        .execute(&state.db)
        .await
        .map_err(|e| {
            tracing::error!("Database error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(serde_json::json!({
        "deleted": result.rows_affected()
    })))
}

fn parse_message_rows(rows: Vec<sqlx::sqlite::SqliteRow>) -> Vec<Message> {
    rows.into_iter()
        .filter_map(|row| {
            let refs_str: String = row.get("refs");
            let refs = if refs_str.is_empty() {
                vec![]
            } else {
                refs_str.split(',').map(|s| s.to_string()).collect()
            };

            let timestamp_str: String = row.get("timestamp");
            let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                .ok()?
                .with_timezone(&Utc);

            Some(Message {
                id: row.get("id"),
                hash: row.get("hash"),
                sender: row.get("sender"),
                recipient: row.get("recipient"),
                content: row.get("content"),
                refs,
                timestamp,
            })
        })
        .collect()
}
