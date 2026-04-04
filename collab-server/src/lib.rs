pub mod db;

use axum::{
    extract::{Path, Request, State},
    http::{self, StatusCode},
    middleware::Next,
    response::{sse::{Event, KeepAlive, Sse}, IntoResponse, Response},
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use sqlx::{sqlite::SqlitePool, Row};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;

const MAX_INSTANCE_ID_LEN: usize = 64;
const MAX_ROLE_LEN: usize = 256;
const MAX_CONTENT_LEN: usize = 4096;
const MAX_REFS_COUNT: usize = 20;
const MAX_REF_LEN: usize = 64;
const MAX_TODO_DESC_LEN: usize = 512;

fn is_valid_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// "all" is a reserved broadcast channel — any worker can send to it and everyone receives it.
fn is_valid_recipient(s: &str) -> bool {
    s == "all" || is_valid_identifier(s)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MessageCreate {
    pub sender: String,
    pub recipient: String,
    pub content: String,
    #[serde(default)]
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub hash: String,
    pub sender: String,
    pub recipient: String,
    pub content: String,
    pub refs: Vec<String>,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TodoCreate {
    pub assigned_by: String,
    pub instance: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub hash: String,
    pub instance: String,
    pub assigned_by: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
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
    pub audit: bool,
    pub tx: broadcast::Sender<Arc<Message>>,
    pub sse_subscribers: Arc<AtomicUsize>,
    pub started_at: Instant,
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.token {
        // Check Authorization header first
        let header_token = request
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));

        // Fall back to ?token= query param (needed for EventSource which can't set headers)
        let query_token: Option<String> = request.uri().query().and_then(|q| {
            q.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                if k == "token" {
                    // URL-decode base64 chars (%3D → =, %2B → +, %2F → /)
                    Some(v.replace("%3D", "=").replace("%2B", "+").replace("%2F", "/"))
                } else {
                    None
                }
            })
        });

        let provided = header_token.map(|s| s.to_string()).or(query_token);
        if provided.as_deref() != Some(expected.as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    next.run(request).await
}

pub fn create_app(state: AppState) -> Router {
    let shared_state = Arc::new(state);

    // Standard routes with 30s timeout
    let timed = Router::new()
        .route("/", get(root))
        .route("/messages", post(create_message))
        .route("/messages/:instance_id", get(list_messages))
        .route("/history/:instance_id", get(get_history))
        .route("/roster", get(get_roster))
        .route("/presence/:instance_id", put(update_presence))
        .route("/presence/:instance_id", delete(delete_presence))
        .route("/messages/cleanup", delete(cleanup_old_messages))
        .route("/metrics", get(get_metrics))
        .route("/todos", post(create_todo))
        .route("/todos/:instance_id", get(list_todos))
        .route("/todos/:hash/done", patch(complete_todo))
        .layer(TimeoutLayer::with_status_code(http::StatusCode::REQUEST_TIMEOUT, StdDuration::from_secs(30)));

    // SSE routes — no timeout, connections stay open indefinitely
    let sse = Router::new()
        .route("/events/:instance_id", get(stream_events))
        .route("/events", get(stream_all_events));

    Router::new()
        .merge(timed)
        .merge(sse)
        .layer(axum::middleware::from_fn_with_state(
            shared_state.clone(),
            auth_middleware,
        ))
        .layer(CorsLayer::permissive())
        .with_state(shared_state)
}

#[cfg(test)]
pub async fn create_test_app() -> Router {
    let db = db::init_test_db().await.unwrap();
    let (tx, _) = broadcast::channel(256);
    let state = AppState {
        db,
        token: None,
        audit: false,
        tx,
        sse_subscribers: Arc::new(AtomicUsize::new(0)),
        started_at: Instant::now(),
    };
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
    if instance_id.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&instance_id) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let rows = if state.audit {
        sqlx::query(
            r#"
            SELECT id, hash, sender, recipient, content, refs, timestamp, read_at
            FROM messages
            WHERE (recipient = ? OR recipient = 'all')
            ORDER BY timestamp DESC
            "#,
        )
        .bind(&instance_id)
        .fetch_all(&state.db)
        .await
    } else {
        let cutoff_iso = (Utc::now() - Duration::hours(8)).to_rfc3339();
        sqlx::query(
            r#"
            SELECT id, hash, sender, recipient, content, refs, timestamp, read_at
            FROM messages
            WHERE (recipient = ? OR recipient = 'all') AND timestamp >= ?
            ORDER BY timestamp DESC
            "#,
        )
        .bind(&instance_id)
        .bind(&cutoff_iso)
        .fetch_all(&state.db)
        .await
    }
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // In audit mode stamp read_at on messages being seen for the first time.
    if state.audit {
        let now = Utc::now().to_rfc3339();
        let _ = sqlx::query(
            "UPDATE messages SET read_at = ? WHERE (recipient = ? OR recipient = 'all') AND read_at IS NULL",
        )
        .bind(&now)
        .bind(&instance_id)
        .execute(&state.db)
        .await;
    }

    let messages = parse_message_rows(rows);
    Ok(Json(messages))
}

async fn get_history(
    Path(instance_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Message>>, StatusCode> {
    if instance_id.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&instance_id) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let limit: Option<i64> = params.get("limit").and_then(|v| v.parse().ok()).filter(|&n| n > 0);

    let rows = if state.audit {
        if let Some(limit) = limit {
            sqlx::query(
                r#"
                SELECT * FROM (
                    SELECT id, hash, sender, recipient, content, refs, timestamp, read_at
                    FROM messages
                    WHERE (recipient = ? OR sender = ?)
                    ORDER BY timestamp DESC
                    LIMIT ?
                ) sub ORDER BY timestamp ASC
                "#,
            )
            .bind(&instance_id)
            .bind(&instance_id)
            .bind(limit)
            .fetch_all(&state.db)
            .await
        } else {
            sqlx::query(
                r#"
                SELECT id, hash, sender, recipient, content, refs, timestamp, read_at
                FROM messages
                WHERE (recipient = ? OR sender = ?)
                ORDER BY timestamp ASC
                "#,
            )
            .bind(&instance_id)
            .bind(&instance_id)
            .fetch_all(&state.db)
            .await
        }
    } else {
        let cutoff_iso = (Utc::now() - Duration::hours(8)).to_rfc3339();
        if let Some(limit) = limit {
            sqlx::query(
                r#"
                SELECT * FROM (
                    SELECT id, hash, sender, recipient, content, refs, timestamp, read_at
                    FROM messages
                    WHERE (recipient = ? OR sender = ?) AND timestamp >= ?
                    ORDER BY timestamp DESC
                    LIMIT ?
                ) sub ORDER BY timestamp ASC
                "#,
            )
            .bind(&instance_id)
            .bind(&instance_id)
            .bind(&cutoff_iso)
            .bind(limit)
            .fetch_all(&state.db)
            .await
        } else {
            sqlx::query(
                r#"
                SELECT id, hash, sender, recipient, content, refs, timestamp, read_at
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
        }
    }
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
    let one_hour_ago = Utc::now() - Duration::hours(8);
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
        counts.insert(sender, usize::try_from(count).unwrap_or(0));
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
                message_count: usize::try_from(row.get::<i64, _>("message_count")).unwrap_or(0),
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

    if instance_id.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&instance_id) || role.len() > MAX_ROLE_LEN {
        return Err(StatusCode::BAD_REQUEST);
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

async fn delete_presence(
    Path(instance_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, StatusCode> {
    if instance_id.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&instance_id) {
        return Err(StatusCode::BAD_REQUEST);
    }

    sqlx::query("DELETE FROM presence WHERE instance_id = ?")
        .bind(&instance_id)
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
        || !is_valid_identifier(&payload.sender)
        || !is_valid_recipient(&payload.recipient)
        || payload.content.len() > MAX_CONTENT_LEN
        || payload.refs.len() > MAX_REFS_COUNT
        || payload.refs.iter().any(|r| r.len() > MAX_REF_LEN)
    {
        return Err(StatusCode::BAD_REQUEST);
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

    let msg = Message {
        id: message_id,
        hash: content_hash,
        sender: payload.sender,
        recipient: payload.recipient,
        content: payload.content,
        refs: payload.refs,
        timestamp,
        read_at: None,
    };

    // Notify SSE subscribers — ignore errors (no subscribers is fine)
    let _ = state.tx.send(Arc::new(msg.clone()));

    Ok(Json(msg))
}

/// RAII guard that decrements the SSE subscriber counter when dropped.
struct SseGuard(Arc<AtomicUsize>);

impl Drop for SseGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn stream_events(
    Path(instance_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>>, StatusCode> {
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt as _;

    if instance_id.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&instance_id) {
        return Err(StatusCode::BAD_REQUEST);
    }

    state.sse_subscribers.fetch_add(1, Ordering::Relaxed);
    let guard = SseGuard(Arc::clone(&state.sse_subscribers));

    let rx = state.tx.subscribe();
    let stream = BroadcastStream::new(rx)
        .filter_map(move |result| {
            // Keep guard alive for the lifetime of the stream.
            let _guard = &guard;
            match result {
                Ok(msg) if msg.recipient == instance_id || msg.recipient == "all" => {
                    let event = Event::default()
                        .json_data(&*msg)
                        .unwrap_or_else(|_| Event::default());
                    Some(Ok(event))
                }
                Ok(_) => None, // not for this subscriber
                Err(_) => None, // lagged or closed — skip, client will reconnect
            }
        });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn stream_all_events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt as _;

    state.sse_subscribers.fetch_add(1, Ordering::Relaxed);
    let guard = SseGuard(Arc::clone(&state.sse_subscribers));

    let rx = state.tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        let _guard = &guard;
        match result {
            Ok(msg) => Some(Ok(Event::default().json_data(&*msg).unwrap_or_else(|_| Event::default()))),
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn get_metrics(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let cutoff_iso = (Utc::now() - Duration::hours(8)).to_rfc3339();

    let messages_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(&state.db)
        .await
        .map_err(|e| {
            tracing::error!("Database error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let messages_last_hour: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE timestamp >= ?")
            .bind(&cutoff_iso)
            .fetch_one(&state.db)
            .await
            .map_err(|e| {
                tracing::error!("Database error: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

    let active_workers: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM presence WHERE last_seen >= ?")
            .bind(&cutoff_iso)
            .fetch_one(&state.db)
            .await
            .map_err(|e| {
                tracing::error!("Database error: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

    let sse_subscribers = state.sse_subscribers.load(Ordering::Relaxed);
    let uptime_secs = state.started_at.elapsed().as_secs();

    Ok(Json(serde_json::json!({
        "messages_total": messages_total,
        "messages_last_hour": messages_last_hour,
        "active_workers": active_workers,
        "sse_subscribers": sse_subscribers,
        "uptime_secs": uptime_secs,
    })))
}

async fn cleanup_old_messages(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if state.audit {
        return Err(StatusCode::FORBIDDEN);
    }

    let one_hour_ago = Utc::now() - Duration::hours(8);
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

async fn create_todo(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<TodoCreate>,
) -> Result<Json<Todo>, StatusCode> {
    if payload.assigned_by.len() > MAX_INSTANCE_ID_LEN
        || payload.instance.len() > MAX_INSTANCE_ID_LEN
        || !is_valid_identifier(&payload.assigned_by)
        || !is_valid_identifier(&payload.instance)
        || payload.description.is_empty()
        || payload.description.len() > MAX_TODO_DESC_LEN
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let now = Utc::now();
    let now_iso = now.to_rfc3339();

    let mut hasher = Sha1::new();
    hasher.update(payload.instance.as_bytes());
    hasher.update(b"|");
    hasher.update(payload.assigned_by.as_bytes());
    hasher.update(b"|");
    hasher.update(payload.description.as_bytes());
    hasher.update(b"|");
    hasher.update(now_iso.as_bytes());
    let hash = format!("{:x}", hasher.finalize());

    let id = Uuid::new_v4().to_string();

    sqlx::query(
        r#"
        INSERT INTO todos (id, hash, instance, assigned_by, description, created_at)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&id)
    .bind(&hash)
    .bind(&payload.instance)
    .bind(&payload.assigned_by)
    .bind(&payload.description)
    .bind(&now_iso)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let todo = Todo {
        id,
        hash: hash.clone(),
        instance: payload.instance.clone(),
        assigned_by: payload.assigned_by.clone(),
        description: payload.description.clone(),
        created_at: now,
        completed_at: None,
    };

    // Wake the worker via SSE — send a ping message so it picks up the task immediately
    let ping_content = format!("New task assigned: {}", payload.description);
    let ping_id = Uuid::new_v4().to_string();
    let ping_ts = Utc::now();
    let mut ping_hasher = Sha1::new();
    ping_hasher.update(payload.assigned_by.as_bytes());
    ping_hasher.update(b"|");
    ping_hasher.update(payload.instance.as_bytes());
    ping_hasher.update(b"|");
    ping_hasher.update(ping_content.as_bytes());
    ping_hasher.update(b"|");
    ping_hasher.update(ping_ts.to_rfc3339().as_bytes());
    let ping_hash = format!("{:x}", ping_hasher.finalize());

    let ping_ts_iso = ping_ts.to_rfc3339();
    let _ = sqlx::query(
        r#"INSERT INTO messages (id, hash, sender, recipient, content, refs, timestamp)
           VALUES (?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&ping_id)
    .bind(&ping_hash)
    .bind(&payload.assigned_by)
    .bind(&payload.instance)
    .bind(&ping_content)
    .bind("")
    .bind(&ping_ts_iso)
    .execute(&state.db)
    .await;

    let _ = state.tx.send(Arc::new(Message {
        id: ping_id,
        hash: ping_hash,
        sender: payload.assigned_by,
        recipient: payload.instance,
        content: ping_content,
        refs: vec![],
        timestamp: ping_ts,
        read_at: None,
    }));

    Ok(Json(todo))
}

async fn list_todos(
    Path(instance_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Todo>>, StatusCode> {
    if instance_id.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&instance_id) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let rows = sqlx::query(
        r#"
        SELECT id, hash, instance, assigned_by, description, created_at, completed_at
        FROM todos
        WHERE instance = ? AND completed_at IS NULL
        ORDER BY created_at ASC
        "#,
    )
    .bind(&instance_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let todos = parse_todo_rows(rows);
    Ok(Json(todos))
}

async fn complete_todo(
    Path(hash_prefix): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, StatusCode> {
    if hash_prefix.len() < 4 || hash_prefix.len() > 40 {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Ensure only hex chars
    if !hash_prefix.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let pattern = format!("{}%", hash_prefix);
    let now_iso = Utc::now().to_rfc3339();

    // Atomic: only update if not already completed — returns 409 on double-complete
    let result = sqlx::query(
        r#"
        UPDATE todos SET completed_at = ?
        WHERE hash LIKE ? AND completed_at IS NULL
        "#,
    )
    .bind(&now_iso)
    .bind(&pattern)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    match result.rows_affected() {
        0 => {
            // Check if it exists at all (already completed vs not found)
            let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM todos WHERE hash LIKE ?")
                .bind(&pattern)
                .fetch_one(&state.db)
                .await
                .unwrap_or(0);
            if exists > 0 {
                Err(StatusCode::CONFLICT) // 409 — already completed
            } else {
                Err(StatusCode::NOT_FOUND)
            }
        }
        _ => Ok(StatusCode::NO_CONTENT),
    }
}

fn parse_todo_rows(rows: Vec<sqlx::sqlite::SqliteRow>) -> Vec<Todo> {
    rows.into_iter()
        .filter_map(|row| {
            let created_str: String = row.get("created_at");
            let created_at = DateTime::parse_from_rfc3339(&created_str)
                .ok()?
                .with_timezone(&Utc);

            let completed_at = row.try_get::<Option<String>, _>("completed_at")
                .ok()
                .flatten()
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            Some(Todo {
                id: row.get("id"),
                hash: row.get("hash"),
                instance: row.get("instance"),
                assigned_by: row.get("assigned_by"),
                description: row.get("description"),
                created_at,
                completed_at,
            })
        })
        .collect()
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

            let read_at = row.try_get::<Option<String>, _>("read_at")
                .ok()
                .flatten()
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            Some(Message {
                id: row.get("id"),
                hash: row.get("hash"),
                sender: row.get("sender"),
                recipient: row.get("recipient"),
                content: row.get("content"),
                refs,
                timestamp,
                read_at,
            })
        })
        .collect()
}
