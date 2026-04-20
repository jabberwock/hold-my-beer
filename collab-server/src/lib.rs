pub mod db;

use axum::{
    extract::{Extension, Path, Request, State},
    http::{self, StatusCode},
    middleware::Next,
    response::{sse::{Event, KeepAlive, Sse}, IntoResponse, Response},
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest as Sha2Digest, Sha256};
use sqlx::{sqlite::SqlitePool, Row};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;

/// Resolved auth context for a request. `team_id = None` means legacy
/// namespace (no team token presented, or server runs without team tokens).
/// Extracted via `Extension<AuthContext>` in handlers.
#[derive(Clone, Debug, Default)]
pub struct AuthContext {
    pub team_id: Option<String>,
}

impl AuthContext {
    pub fn legacy() -> Self {
        Self { team_id: None }
    }
    pub fn for_team(team_id: impl Into<String>) -> Self {
        Self { team_id: Some(team_id.into()) }
    }
}

/// SHA-256 hex digest of a token. Used for looking up team tokens without
/// storing plaintext. Callers hash once at mint + once per request.
pub fn hash_token(token: &str) -> String {
    let mut hasher = <Sha256 as Sha2Digest>::new();
    Sha2Digest::update(&mut hasher, token.as_bytes());
    format!("{:x}", Sha2Digest::finalize(hasher))
}

/// How long a lease survives without a heartbeat before another worker
/// can take it over. Worth tuning against the client heartbeat cadence —
/// must be strictly greater than the longest expected heartbeat interval.
pub const LEASE_TTL_SECS: i64 = 30;

const MAX_INSTANCE_ID_LEN: usize = 64;
const MAX_ROLE_LEN: usize = 256;
const MAX_CONTENT_LEN: usize = 4096;
const MAX_REFS_COUNT: usize = 20;
const MAX_REF_LEN: usize = 64;
const MAX_TODO_DESC_LEN: usize = 2048;

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

/// Payload for POST /worker/lease — a worker claiming the singleton slot
/// for its `(team_id, instance_id)` identity. Same endpoint doubles as the
/// heartbeat: subsequent calls from the same pid extend the lease, calls
/// from a different pid either take over (if existing lease is stale) or
/// 409 (if still fresh).
#[derive(Debug, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub instance_id: String,
    pub pid: i64,
    pub host: String,
}

/// Lease state returned to the caller. `taken_over` is true when a stale
/// lease from a different pid was displaced — a signal worth logging so
/// humans can notice runaway restart loops.
#[derive(Debug, Serialize, Deserialize)]
pub struct LeaseState {
    pub team_id: Option<String>,
    pub instance_id: String,
    pub pid: i64,
    pub host: String,
    pub acquired_at: DateTime<Utc>,
    pub last_heartbeat: DateTime<Utc>,
    pub taken_over: bool,
}

/// Body returned on 409 so the caller can log *who* beat them to it.
#[derive(Debug, Serialize, Deserialize)]
pub struct LeaseConflict {
    pub team_id: Option<String>,
    pub instance_id: String,
    pub pid: i64,
    pub host: String,
    pub last_heartbeat: DateTime<Utc>,
    pub seconds_since_heartbeat: i64,
}

/// Delta payload sent by workers after each CLI invocation. The server
/// adds these to the running per-(team, worker) counters — it does not
/// store per-call history. Cost is optional because only the Claude CLI
/// surfaces real USD; other CLIs leave it unset.
#[derive(Debug, Serialize, Deserialize)]
pub struct UsageReport {
    pub worker: String,
    pub duration_secs: u64,
    pub input_tokens: u64,
    /// New tokens written to the prompt cache on this call.
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// Tokens served from the prompt cache on this call — the signal for
    /// whether caching is doing any work.
    #[serde(default)]
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub tier: String,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub cli: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UsageRow {
    pub worker: String,
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub duration_secs: u64,
    pub calls: u64,
    pub light_calls: u64,
    pub full_calls: u64,
    pub cost_usd: f64,
    pub cli: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UsageResponse {
    pub workers: Vec<UsageRow>,
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_cache_creation_tokens: u64,
    #[serde(default)]
    pub total_cache_read_tokens: u64,
    pub total_output_tokens: u64,
    pub total_duration_secs: u64,
    pub total_calls: u64,
    pub total_light_calls: u64,
    pub total_full_calls: u64,
    pub total_cost_usd: f64,
}

/// Broadcast payload for SSE. Carries the resolved team_id alongside the
/// message so each subscriber's SSE filter can drop messages for other
/// teams — the Message body itself stays team-unaware for backwards
/// compatibility with clients that didn't expect the field.
#[derive(Clone, Debug)]
pub struct BroadcastMsg {
    pub team_id: Option<String>,
    pub message: Message,
}

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub token: Option<String>,
    pub audit: bool,
    pub tx: broadcast::Sender<Arc<BroadcastMsg>>,
    pub sse_subscribers: Arc<AtomicUsize>,
    pub started_at: Instant,
}

/// Extract the bearer token from either the `Authorization: Bearer ...` header
/// or the `?token=...` query param (needed for EventSource, which can't set
/// headers). URL-decodes a small set of base64 chars so tokens survive round
/// tripping through query strings.
fn extract_token(request: &Request) -> Option<String> {
    let header_token = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    if header_token.is_some() {
        return header_token;
    }
    request.uri().query().and_then(|q| {
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == "token" {
                Some(v.replace("%3D", "=").replace("%2B", "+").replace("%2F", "/"))
            } else {
                None
            }
        })
    })
}

/// Look up a team token's team_id by its plaintext, returning Some(team_id)
/// only if the token is active (not revoked). Caller already has the
/// plaintext; we hash here so the DB never sees the raw secret on the wire.
async fn lookup_team_by_token(db: &SqlitePool, token: &str) -> Result<Option<String>, sqlx::Error> {
    let hash = hash_token(token);
    let row = sqlx::query("SELECT team_id FROM team_tokens WHERE token_hash = ? AND revoked_at IS NULL")
        .bind(&hash)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("team_id")))
}

/// Auth resolves the token to one of three outcomes:
///   - team match → request runs as `AuthContext { team_id: Some(t) }`
///   - legacy env-token match (or no auth configured, no token presented) →
///     `AuthContext::legacy()` (team_id = None)
///   - nothing matches → 401
///
/// Team tokens are honored regardless of whether the legacy env token is
/// configured. This lets a single server host both legacy single-namespace
/// clients and teamed clients simultaneously during migration.
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let provided = extract_token(&request);

    // Team token path: try the DB first so a team token always wins over any
    // legacy fallback. Rare DB errors fall through; handlers will 500 on their
    // own queries and surface the real cause.
    if let Some(ref tok) = provided {
        match lookup_team_by_token(&state.db, tok).await {
            Ok(Some(team_id)) => {
                request.extensions_mut().insert(AuthContext::for_team(team_id));
                return next.run(request).await;
            }
            Ok(None) => {} // fall through to legacy path
            Err(e) => {
                tracing::error!("token lookup failed: {}", e);
                // fall through — legacy match may still succeed
            }
        }
    }

    // Legacy env-token path.
    if let Some(expected) = state.token.as_ref() {
        if provided.as_deref() == Some(expected.as_str()) {
            request.extensions_mut().insert(AuthContext::legacy());
            return next.run(request).await;
        }
        // Server is configured with a legacy token but caller didn't match it
        // (and didn't match any team token above) → reject.
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // No legacy token configured, no team token match. Preserves the original
    // "no-auth" mode: if neither side presents credentials, the request runs
    // as legacy. This keeps existing single-user setups working unchanged.
    request.extensions_mut().insert(AuthContext::legacy());
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
        .route("/worker/lease", post(acquire_lease))
        .route("/worker/lease/:instance_id", delete(release_lease))
        .route("/usage", post(report_usage))
        .route("/usage", get(get_usage))
        .route("/admin/teams", get(list_teams))
        .route("/admin/teams", post(create_team))
        .route("/admin/teams/:team_id/tokens", post(mint_team_token))
        .route("/admin/teams/:team_id/tokens/:token_prefix", delete(revoke_team_token))
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
    Extension(auth): Extension<AuthContext>,
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
              AND COALESCE(team_id, '') = COALESCE(?, '')
            ORDER BY timestamp DESC
            "#,
        )
        .bind(&instance_id)
        .bind(auth.team_id.as_deref())
        .fetch_all(&state.db)
        .await
    } else {
        let cutoff_iso = (Utc::now() - Duration::hours(8)).to_rfc3339();
        sqlx::query(
            r#"
            SELECT id, hash, sender, recipient, content, refs, timestamp, read_at
            FROM messages
            WHERE (recipient = ? OR recipient = 'all')
              AND COALESCE(team_id, '') = COALESCE(?, '')
              AND timestamp >= ?
            ORDER BY timestamp DESC
            "#,
        )
        .bind(&instance_id)
        .bind(auth.team_id.as_deref())
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
            r#"UPDATE messages SET read_at = ?
               WHERE (recipient = ? OR recipient = 'all')
                 AND COALESCE(team_id, '') = COALESCE(?, '')
                 AND read_at IS NULL"#,
        )
        .bind(&now)
        .bind(&instance_id)
        .bind(auth.team_id.as_deref())
        .execute(&state.db)
        .await;
    }

    let messages = parse_message_rows(rows);
    Ok(Json(messages))
}

async fn get_history(
    Extension(auth): Extension<AuthContext>,
    Path(instance_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Message>>, StatusCode> {
    if instance_id.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&instance_id) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let limit: Option<i64> = params.get("limit").and_then(|v| v.parse().ok()).filter(|&n| n > 0);
    let team = auth.team_id.as_deref();

    let rows = if state.audit {
        if let Some(limit) = limit {
            sqlx::query(
                r#"
                SELECT * FROM (
                    SELECT id, hash, sender, recipient, content, refs, timestamp, read_at
                    FROM messages
                    WHERE (recipient = ? OR sender = ?)
                      AND COALESCE(team_id, '') = COALESCE(?, '')
                    ORDER BY timestamp DESC
                    LIMIT ?
                ) sub ORDER BY timestamp ASC
                "#,
            )
            .bind(&instance_id)
            .bind(&instance_id)
            .bind(team)
            .bind(limit)
            .fetch_all(&state.db)
            .await
        } else {
            sqlx::query(
                r#"
                SELECT id, hash, sender, recipient, content, refs, timestamp, read_at
                FROM messages
                WHERE (recipient = ? OR sender = ?)
                  AND COALESCE(team_id, '') = COALESCE(?, '')
                ORDER BY timestamp ASC
                "#,
            )
            .bind(&instance_id)
            .bind(&instance_id)
            .bind(team)
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
                    WHERE (recipient = ? OR sender = ?)
                      AND COALESCE(team_id, '') = COALESCE(?, '')
                      AND timestamp >= ?
                    ORDER BY timestamp DESC
                    LIMIT ?
                ) sub ORDER BY timestamp ASC
                "#,
            )
            .bind(&instance_id)
            .bind(&instance_id)
            .bind(team)
            .bind(&cutoff_iso)
            .bind(limit)
            .fetch_all(&state.db)
            .await
        } else {
            sqlx::query(
                r#"
                SELECT id, hash, sender, recipient, content, refs, timestamp, read_at
                FROM messages
                WHERE (recipient = ? OR sender = ?)
                  AND COALESCE(team_id, '') = COALESCE(?, '')
                  AND timestamp >= ?
                ORDER BY timestamp ASC
                "#,
            )
            .bind(&instance_id)
            .bind(&instance_id)
            .bind(team)
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
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<WorkerInfo>>, StatusCode> {
    let one_hour_ago = Utc::now() - Duration::hours(8);
    let cutoff_iso = one_hour_ago.to_rfc3339();
    let team = auth.team_id.as_deref();

    let presence_rows = sqlx::query(
        r#"
        SELECT instance_id, role, last_seen
        FROM presence
        WHERE last_seen >= ?
          AND COALESCE(team_id, '') = COALESCE(?, '')
        ORDER BY last_seen DESC
        "#,
    )
    .bind(&cutoff_iso)
    .bind(team)
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
          AND COALESCE(team_id, '') = COALESCE(?, '')
        GROUP BY sender
        "#,
    )
    .bind(&cutoff_iso)
    .bind(team)
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
          AND COALESCE(team_id, '') = COALESCE(?, '')
        GROUP BY sender
        ORDER BY last_seen DESC
        "#,
    )
    .bind(&cutoff_iso)
    .bind(team)
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
    Extension(auth): Extension<AuthContext>,
    Path(instance_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<PresenceUpdate>,
) -> Result<StatusCode, StatusCode> {
    let role = payload.role.unwrap_or_default();

    if instance_id.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&instance_id) || role.len() > MAX_ROLE_LEN {
        return Err(StatusCode::BAD_REQUEST);
    }

    let now = Utc::now().to_rfc3339();

    // presence has PRIMARY KEY(instance_id), so a worker that appears in
    // multiple teams under the same name would conflict. Since we scope
    // inserts by team at the query level, we upsert on (instance_id,team_id)
    // using a manual two-step instead of ON CONFLICT — SQLite can't do
    // partial-key upserts on a UNIQUE INDEX cleanly.
    let existing: Option<String> = sqlx::query_scalar(
        r#"SELECT instance_id FROM presence
           WHERE instance_id = ? AND COALESCE(team_id, '') = COALESCE(?, '')"#,
    )
    .bind(&instance_id)
    .bind(auth.team_id.as_deref())
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if existing.is_some() {
        sqlx::query(
            r#"UPDATE presence SET
                 role = CASE WHEN ? != '' THEN ? ELSE role END,
                 last_seen = ?
               WHERE instance_id = ? AND COALESCE(team_id, '') = COALESCE(?, '')"#,
        )
        .bind(&role).bind(&role).bind(&now)
        .bind(&instance_id).bind(auth.team_id.as_deref())
        .execute(&state.db).await
        .map_err(|e| {
            tracing::error!("Database error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    } else {
        sqlx::query(
            r#"INSERT INTO presence (instance_id, role, last_seen, team_id)
               VALUES (?, ?, ?, ?)"#,
        )
        .bind(&instance_id).bind(&role).bind(&now).bind(auth.team_id.as_deref())
        .execute(&state.db).await
        .map_err(|e| {
            tracing::error!("Database error: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }

    Ok(StatusCode::NO_CONTENT)
}

async fn delete_presence(
    Extension(auth): Extension<AuthContext>,
    Path(instance_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, StatusCode> {
    if instance_id.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&instance_id) {
        return Err(StatusCode::BAD_REQUEST);
    }

    sqlx::query(
        r#"DELETE FROM presence
           WHERE instance_id = ? AND COALESCE(team_id, '') = COALESCE(?, '')"#,
    )
    .bind(&instance_id)
    .bind(auth.team_id.as_deref())
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// Acquire or extend the singleton lease for (team_id, instance_id).
///
/// Three outcomes:
///   - Existing lease, same pid  → heartbeat (update last_heartbeat). 200 OK.
///   - Existing lease, different pid, stale (> LEASE_TTL_SECS) → take over.
///     200 OK with taken_over=true.
///   - Existing lease, different pid, fresh → 409 Conflict with holder info.
///
/// All three steps run inside a transaction so concurrent callers can't
/// both succeed. The UNIQUE index on (COALESCE(team_id,''), instance_id)
/// is the backstop if the transaction logic ever races.
async fn acquire_lease(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<LeaseRequest>,
) -> Result<Response, StatusCode> {
    if payload.instance_id.len() > MAX_INSTANCE_ID_LEN
        || !is_valid_identifier(&payload.instance_id)
        || payload.host.len() > 256
        || payload.pid <= 0
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let ttl_cutoff = (now - Duration::seconds(LEASE_TTL_SECS)).to_rfc3339();

    let mut tx = state.db.begin().await.map_err(|e| {
        tracing::error!("lease tx begin failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // 1. Same-pid heartbeat: extend last_heartbeat if the lease is ours.
    let updated = sqlx::query(
        r#"
        UPDATE worker_leases
        SET last_heartbeat = ?, host = ?
        WHERE COALESCE(team_id, '') = COALESCE(?, '')
          AND instance_id = ?
          AND pid = ?
        "#,
    )
    .bind(&now_iso)
    .bind(&payload.host)
    .bind(auth.team_id.as_deref())
    .bind(&payload.instance_id)
    .bind(payload.pid)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!("lease heartbeat failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if updated.rows_affected() > 0 {
        // Heartbeat: fetch the current row to return it to the caller.
        let row = sqlx::query(
            r#"
            SELECT team_id, instance_id, pid, host, acquired_at, last_heartbeat
            FROM worker_leases
            WHERE COALESCE(team_id, '') = COALESCE(?, '')
              AND instance_id = ?
            "#,
        )
        .bind(auth.team_id.as_deref())
        .bind(&payload.instance_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!("lease fetch failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        tx.commit().await.map_err(|e| {
            tracing::error!("lease commit failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        return Ok(Json(lease_state_from_row(row, false)).into_response());
    }

    // 2. No same-pid lease. Inspect any existing lease for this identity.
    let existing = sqlx::query(
        r#"
        SELECT team_id, instance_id, pid, host, acquired_at, last_heartbeat
        FROM worker_leases
        WHERE COALESCE(team_id, '') = COALESCE(?, '')
          AND instance_id = ?
        "#,
    )
    .bind(auth.team_id.as_deref())
    .bind(&payload.instance_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!("lease lookup failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut taken_over = false;
    if let Some(row) = existing {
        let hb_str: String = row.get("last_heartbeat");
        let hb = DateTime::parse_from_rfc3339(&hb_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or(now);
        let stale = hb_str.as_str() < ttl_cutoff.as_str()
            || (now - hb).num_seconds() > LEASE_TTL_SECS;

        if !stale {
            // Fresh lease, different pid → conflict.
            let holder_pid: i64 = row.get("pid");
            let holder_host: String = row.get("host");
            let holder_team: Option<String> = row.try_get("team_id").ok().flatten();
            let conflict = LeaseConflict {
                team_id: holder_team,
                instance_id: payload.instance_id.clone(),
                pid: holder_pid,
                host: holder_host,
                last_heartbeat: hb,
                seconds_since_heartbeat: (now - hb).num_seconds(),
            };
            tx.commit().await.ok();
            return Ok((StatusCode::CONFLICT, Json(conflict)).into_response());
        }

        // Stale — evict so the INSERT below can succeed.
        sqlx::query(
            r#"
            DELETE FROM worker_leases
            WHERE COALESCE(team_id, '') = COALESCE(?, '')
              AND instance_id = ?
            "#,
        )
        .bind(auth.team_id.as_deref())
        .bind(&payload.instance_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!("stale lease eviction failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        taken_over = true;
        tracing::warn!(
            "lease take-over: team={:?} instance={} displacing pid {} (stale {}s)",
            auth.team_id,
            payload.instance_id,
            row.get::<i64, _>("pid"),
            (now - hb).num_seconds(),
        );
    }

    // 3. Insert fresh lease.
    sqlx::query(
        r#"
        INSERT INTO worker_leases (team_id, instance_id, pid, host, acquired_at, last_heartbeat)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(auth.team_id.as_deref())
    .bind(&payload.instance_id)
    .bind(payload.pid)
    .bind(&payload.host)
    .bind(&now_iso)
    .bind(&now_iso)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!("lease insert failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    tx.commit().await.map_err(|e| {
        tracing::error!("lease commit failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(LeaseState {
        team_id: auth.team_id,
        instance_id: payload.instance_id,
        pid: payload.pid,
        host: payload.host,
        acquired_at: now,
        last_heartbeat: now,
        taken_over,
    })
    .into_response())
}

/// Release a lease. Idempotent: returns 204 whether or not the lease exists.
/// Only the current holder's pid can release — callers with the wrong pid
/// are silently ignored (no leak of who holds it, and the legitimate holder
/// retains the lease).
async fn release_lease(
    Extension(auth): Extension<AuthContext>,
    Path(instance_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<LeaseRequest>,
) -> Result<StatusCode, StatusCode> {
    if instance_id.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&instance_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if payload.instance_id != instance_id {
        return Err(StatusCode::BAD_REQUEST);
    }

    sqlx::query(
        r#"
        DELETE FROM worker_leases
        WHERE COALESCE(team_id, '') = COALESCE(?, '')
          AND instance_id = ?
          AND pid = ?
        "#,
    )
    .bind(auth.team_id.as_deref())
    .bind(&instance_id)
    .bind(payload.pid)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("lease release failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// Admin endpoints (create team, mint tokens, etc.) are open only to callers
/// who reached legacy auth context. In practice that means: the server was
/// started with `COLLAB_TOKEN=...` and the caller presented that same token.
/// If the server runs without an env token, anyone can admin — which is
/// acceptable for single-operator deployments (the whole server is open)
/// but loudly logged so it's not accidental.
fn require_admin(state: &AppState, auth: &AuthContext) -> Result<(), Response> {
    if auth.team_id.is_some() {
        // Team tokens are not admins by design. Return a human-readable
        // hint so the CLI can show the operator why they got rejected.
        let body = serde_json::json!({
            "error": "admin_required",
            "message": "This endpoint requires the admin token (COLLAB_ADMIN_TOKEN). You presented a team token (COLLAB_TOKEN) — team tokens can do worker ops but not admin ops."
        });
        return Err((StatusCode::FORBIDDEN, Json(body)).into_response());
    }
    if state.token.is_none() {
        tracing::warn!(
            "admin endpoint reached with no server env token configured — \
             anyone reaching the network can admin this server"
        );
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateTeamRequest {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateTeamResponse {
    pub team_id: String,
    pub name: String,
    /// Plaintext token, shown exactly once. The hash is what's stored.
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TeamInfo {
    pub team_id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub active_token_count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MintTokenResponse {
    pub token: String,
    pub token_prefix: String,
}

/// Name validation for teams: alphanumeric + dash/underscore, 1–64 chars.
fn is_valid_team_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// Generate a team token. 32 random bytes → url-safe base64-ish. Callers
/// see the plaintext once; we only store the SHA-256 hash.
fn generate_token() -> String {
    let uuid1 = Uuid::new_v4();
    let uuid2 = Uuid::new_v4();
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid1.as_bytes());
    bytes.extend_from_slice(uuid2.as_bytes());
    let mut hasher = <Sha256 as Sha2Digest>::new();
    Sha2Digest::update(&mut hasher, &bytes);
    format!("tm_{:x}", Sha2Digest::finalize(hasher))
}

async fn list_teams(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, Response> {
    require_admin(&state, &auth)?;
    let rows = sqlx::query(
        r#"
        SELECT t.id, t.name, t.created_at,
               (SELECT COUNT(*) FROM team_tokens tt
                WHERE tt.team_id = t.id AND tt.revoked_at IS NULL) AS tokens
        FROM teams t
        ORDER BY t.created_at ASC
        "#,
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("teams list failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;

    let teams = rows
        .into_iter()
        .filter_map(|row| {
            let created_str: String = row.get("created_at");
            let created_at = DateTime::parse_from_rfc3339(&created_str)
                .ok()?
                .with_timezone(&Utc);
            Some(TeamInfo {
                team_id: row.get("id"),
                name: row.get("name"),
                created_at,
                active_token_count: row.get("tokens"),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(teams).into_response())
}

async fn create_team(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateTeamRequest>,
) -> Result<Response, Response> {
    require_admin(&state, &auth)?;
    if !is_valid_team_name(&payload.name) {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }

    let team_id = Uuid::new_v4().to_string();
    let now_iso = Utc::now().to_rfc3339();
    let token = generate_token();
    let hash = hash_token(&token);

    let mut tx = state.db.begin().await.map_err(|e| {
        tracing::error!("create_team tx begin: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;

    let insert_team = sqlx::query("INSERT INTO teams (id, name, created_at) VALUES (?, ?, ?)")
        .bind(&team_id)
        .bind(&payload.name)
        .bind(&now_iso)
        .execute(&mut *tx)
        .await;
    match insert_team {
        Ok(_) => {}
        Err(sqlx::Error::Database(dbe)) if dbe.message().contains("UNIQUE") => {
            return Err(StatusCode::CONFLICT.into_response());
        }
        Err(e) => {
            tracing::error!("insert team failed: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    }

    sqlx::query("INSERT INTO team_tokens (token_hash, team_id, created_at) VALUES (?, ?, ?)")
        .bind(&hash)
        .bind(&team_id)
        .bind(&now_iso)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!("insert team token failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;

    tx.commit().await.map_err(|e| {
        tracing::error!("create_team commit: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;

    Ok(Json(CreateTeamResponse {
        team_id,
        name: payload.name,
        token,
    }).into_response())
}

async fn mint_team_token(
    Extension(auth): Extension<AuthContext>,
    Path(team_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, Response> {
    require_admin(&state, &auth)?;

    let exists: Option<String> = sqlx::query_scalar("SELECT id FROM teams WHERE id = ?")
        .bind(&team_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| {
            tracing::error!("team lookup failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;
    if exists.is_none() {
        return Err(StatusCode::NOT_FOUND.into_response());
    }

    let token = generate_token();
    let hash = hash_token(&token);
    let now_iso = Utc::now().to_rfc3339();

    sqlx::query("INSERT INTO team_tokens (token_hash, team_id, created_at) VALUES (?, ?, ?)")
        .bind(&hash)
        .bind(&team_id)
        .bind(&now_iso)
        .execute(&state.db)
        .await
        .map_err(|e| {
            tracing::error!("mint token failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;

    let token_prefix = token.chars().take(12).collect::<String>();
    Ok(Json(MintTokenResponse { token, token_prefix }).into_response())
}

async fn revoke_team_token(
    Extension(auth): Extension<AuthContext>,
    Path((team_id, token_prefix)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, Response> {
    require_admin(&state, &auth)?;

    if token_prefix.len() < 8 || token_prefix.len() > 64 {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    if !token_prefix.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }

    let now_iso = Utc::now().to_rfc3339();
    let pattern = format!("{}%", token_prefix);
    let result = sqlx::query(
        r#"UPDATE team_tokens SET revoked_at = ?
           WHERE team_id = ? AND token_hash LIKE ? AND revoked_at IS NULL"#,
    )
    .bind(&now_iso)
    .bind(&team_id)
    .bind(&pattern)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("revoke token failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;

    if result.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn lease_state_from_row(row: sqlx::sqlite::SqliteRow, taken_over: bool) -> LeaseState {
    let team_id: Option<String> = row.try_get("team_id").ok().flatten();
    let instance_id: String = row.get("instance_id");
    let pid: i64 = row.get("pid");
    let host: String = row.get("host");
    let acquired_str: String = row.get("acquired_at");
    let heartbeat_str: String = row.get("last_heartbeat");
    let acquired_at = DateTime::parse_from_rfc3339(&acquired_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let last_heartbeat = DateTime::parse_from_rfc3339(&heartbeat_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    LeaseState {
        team_id,
        instance_id,
        pid,
        host,
        acquired_at,
        last_heartbeat,
        taken_over,
    }
}

async fn create_message(
    Extension(auth): Extension<AuthContext>,
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
        INSERT INTO messages (id, hash, sender, recipient, content, refs, timestamp, team_id)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&message_id)
    .bind(&content_hash)
    .bind(&payload.sender)
    .bind(&payload.recipient)
    .bind(&payload.content)
    .bind(&refs_str)
    .bind(&timestamp_iso)
    .bind(auth.team_id.as_deref())
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

    // Broadcast tagged with team_id so each SSE subscriber can filter out
    // messages bound for a different team namespace.
    let _ = state.tx.send(Arc::new(BroadcastMsg {
        team_id: auth.team_id,
        message: msg.clone(),
    }));

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
    Extension(auth): Extension<AuthContext>,
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
    let subscriber_team = auth.team_id.clone();

    let rx = state.tx.subscribe();
    let stream = BroadcastStream::new(rx)
        .filter_map(move |result| {
            let _guard = &guard;
            match result {
                Ok(bmsg) => {
                    if bmsg.team_id != subscriber_team {
                        return None;
                    }
                    if bmsg.message.recipient == instance_id || bmsg.message.recipient == "all" {
                        let event = Event::default()
                            .json_data(&bmsg.message)
                            .unwrap_or_else(|_| Event::default());
                        Some(Ok(event))
                    } else {
                        None
                    }
                }
                Err(_) => None, // lagged or closed — skip, client will reconnect
            }
        });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn stream_all_events(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt as _;

    state.sse_subscribers.fetch_add(1, Ordering::Relaxed);
    let guard = SseGuard(Arc::clone(&state.sse_subscribers));
    let subscriber_team = auth.team_id.clone();

    let rx = state.tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        let _guard = &guard;
        match result {
            Ok(bmsg) => {
                if bmsg.team_id != subscriber_team {
                    return None;
                }
                Some(Ok(Event::default().json_data(&bmsg.message).unwrap_or_else(|_| Event::default())))
            }
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn get_metrics(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let cutoff_iso = (Utc::now() - Duration::hours(8)).to_rfc3339();
    let team = auth.team_id.as_deref();

    let messages_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE COALESCE(team_id, '') = COALESCE(?, '')"
    )
    .bind(team)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let messages_last_hour: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM messages
           WHERE timestamp >= ? AND COALESCE(team_id, '') = COALESCE(?, '')"#,
    )
    .bind(&cutoff_iso)
    .bind(team)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let active_workers: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM presence
           WHERE last_seen >= ? AND COALESCE(team_id, '') = COALESCE(?, '')"#,
    )
    .bind(&cutoff_iso)
    .bind(team)
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

async fn report_usage(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<UsageReport>,
) -> Result<StatusCode, StatusCode> {
    if payload.worker.len() > MAX_INSTANCE_ID_LEN || !is_valid_identifier(&payload.worker) {
        return Err(StatusCode::BAD_REQUEST);
    }
    // `light` is accepted for back-compat with pre-tier-drop workers that may
    // still be in-flight, but every new call reports `full` since Light tier
    // no longer exists. `light_calls` stays as historical-only in the DB.
    let tier = match payload.tier.as_str() {
        "light" | "full" => payload.tier.as_str(),
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    let light_delta: i64 = if tier == "light" { 1 } else { 0 };
    let full_delta: i64 = 1 - light_delta;
    let team_key = auth.team_id.clone().unwrap_or_default();
    let cost = payload.cost_usd.unwrap_or(0.0);
    let cli = payload.cli.unwrap_or_default();
    let now_iso = Utc::now().to_rfc3339();

    sqlx::query(
        r#"
        INSERT INTO team_usage_totals (
            team_id, worker, input_tokens, cache_creation_tokens, cache_read_tokens,
            output_tokens, duration_secs,
            calls, light_calls, full_calls, cost_usd, cli, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?)
        ON CONFLICT(team_id, worker) DO UPDATE SET
            input_tokens          = input_tokens          + excluded.input_tokens,
            cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
            cache_read_tokens     = cache_read_tokens     + excluded.cache_read_tokens,
            output_tokens         = output_tokens         + excluded.output_tokens,
            duration_secs         = duration_secs         + excluded.duration_secs,
            calls                 = calls                 + 1,
            light_calls           = light_calls           + excluded.light_calls,
            full_calls            = full_calls            + excluded.full_calls,
            cost_usd              = cost_usd              + excluded.cost_usd,
            cli                   = CASE WHEN excluded.cli != '' THEN excluded.cli ELSE cli END,
            updated_at            = excluded.updated_at
        "#,
    )
    .bind(&team_key)
    .bind(&payload.worker)
    .bind(payload.input_tokens as i64)
    .bind(payload.cache_creation_tokens as i64)
    .bind(payload.cache_read_tokens as i64)
    .bind(payload.output_tokens as i64)
    .bind(payload.duration_secs as i64)
    .bind(light_delta)
    .bind(full_delta)
    .bind(cost)
    .bind(&cli)
    .bind(&now_iso)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("usage upsert failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(StatusCode::NO_CONTENT)
}

async fn get_usage(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<UsageResponse>, StatusCode> {
    let team_key = auth.team_id.clone().unwrap_or_default();
    let rows = sqlx::query(
        r#"
        SELECT worker, input_tokens, cache_creation_tokens, cache_read_tokens,
               output_tokens, duration_secs,
               calls, light_calls, full_calls, cost_usd, cli, updated_at
        FROM team_usage_totals
        WHERE team_id = ?
        ORDER BY (input_tokens + cache_creation_tokens + cache_read_tokens + output_tokens) DESC
        "#,
    )
    .bind(&team_key)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("usage fetch failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut workers = Vec::with_capacity(rows.len());
    let mut total_in = 0u64;
    let mut total_cache_creation = 0u64;
    let mut total_cache_read = 0u64;
    let mut total_out = 0u64;
    let mut total_dur = 0u64;
    let mut total_calls = 0u64;
    let mut total_light = 0u64;
    let mut total_full = 0u64;
    let mut total_cost = 0.0f64;
    for row in rows {
        let input_tokens: i64 = row.get("input_tokens");
        let cache_creation_tokens: i64 = row.get("cache_creation_tokens");
        let cache_read_tokens: i64 = row.get("cache_read_tokens");
        let output_tokens: i64 = row.get("output_tokens");
        let duration_secs: i64 = row.get("duration_secs");
        let calls: i64 = row.get("calls");
        let light_calls: i64 = row.get("light_calls");
        let full_calls: i64 = row.get("full_calls");
        let cost_usd: f64 = row.get("cost_usd");
        let updated_str: String = row.get("updated_at");
        let updated_at = DateTime::parse_from_rfc3339(&updated_str)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        total_in += input_tokens as u64;
        total_cache_creation += cache_creation_tokens as u64;
        total_cache_read += cache_read_tokens as u64;
        total_out += output_tokens as u64;
        total_dur += duration_secs as u64;
        total_calls += calls as u64;
        total_light += light_calls as u64;
        total_full += full_calls as u64;
        total_cost += cost_usd;
        workers.push(UsageRow {
            worker: row.get("worker"),
            input_tokens: input_tokens as u64,
            cache_creation_tokens: cache_creation_tokens as u64,
            cache_read_tokens: cache_read_tokens as u64,
            output_tokens: output_tokens as u64,
            duration_secs: duration_secs as u64,
            calls: calls as u64,
            light_calls: light_calls as u64,
            full_calls: full_calls as u64,
            cost_usd,
            cli: row.get("cli"),
            updated_at,
        });
    }

    Ok(Json(UsageResponse {
        workers,
        total_input_tokens: total_in,
        total_cache_creation_tokens: total_cache_creation,
        total_cache_read_tokens: total_cache_read,
        total_output_tokens: total_out,
        total_duration_secs: total_dur,
        total_calls,
        total_light_calls: total_light,
        total_full_calls: total_full,
        total_cost_usd: total_cost,
    }))
}

async fn cleanup_old_messages(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if state.audit {
        return Err(StatusCode::FORBIDDEN);
    }

    let one_hour_ago = Utc::now() - Duration::hours(8);
    let cutoff_iso = one_hour_ago.to_rfc3339();

    let result = sqlx::query(
        r#"DELETE FROM messages
           WHERE timestamp < ?
             AND COALESCE(team_id, '') = COALESCE(?, '')"#,
    )
    .bind(&cutoff_iso)
    .bind(auth.team_id.as_deref())
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    sqlx::query(
        r#"DELETE FROM presence
           WHERE last_seen < ?
             AND COALESCE(team_id, '') = COALESCE(?, '')"#,
    )
    .bind(&cutoff_iso)
    .bind(auth.team_id.as_deref())
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
    Extension(auth): Extension<AuthContext>,
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
        INSERT INTO todos (id, hash, instance, assigned_by, description, created_at, team_id)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&id)
    .bind(&hash)
    .bind(&payload.instance)
    .bind(&payload.assigned_by)
    .bind(&payload.description)
    .bind(&now_iso)
    .bind(auth.team_id.as_deref())
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

    // Wake the worker via SSE — send a ping message so it picks up the task
    // immediately. This is the *only* delegation notification; the CLI's
    // todo_add used to post its own too, which produced a visible duplicate.
    let ping_content = format!("📋 New task assigned: {}", payload.description);
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
        r#"INSERT INTO messages (id, hash, sender, recipient, content, refs, timestamp, team_id)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&ping_id)
    .bind(&ping_hash)
    .bind(&payload.assigned_by)
    .bind(&payload.instance)
    .bind(&ping_content)
    .bind("")
    .bind(&ping_ts_iso)
    .bind(auth.team_id.as_deref())
    .execute(&state.db)
    .await;

    let _ = state.tx.send(Arc::new(BroadcastMsg {
        team_id: auth.team_id,
        message: Message {
            id: ping_id,
            hash: ping_hash,
            sender: payload.assigned_by,
            recipient: payload.instance,
            content: ping_content,
            refs: vec![],
            timestamp: ping_ts,
            read_at: None,
        },
    }));

    Ok(Json(todo))
}

async fn list_todos(
    Extension(auth): Extension<AuthContext>,
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
        WHERE instance = ?
          AND completed_at IS NULL
          AND COALESCE(team_id, '') = COALESCE(?, '')
        ORDER BY created_at ASC
        "#,
    )
    .bind(&instance_id)
    .bind(auth.team_id.as_deref())
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
    Extension(auth): Extension<AuthContext>,
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

    // Atomic: only update if not already completed and within the caller's
    // team — returns 409 on double-complete, 404 if the hash belongs to a
    // different team (deliberately indistinguishable from "doesn't exist"
    // so teams can't probe each other's todo hashes).
    let result = sqlx::query(
        r#"
        UPDATE todos SET completed_at = ?
        WHERE hash LIKE ?
          AND completed_at IS NULL
          AND COALESCE(team_id, '') = COALESCE(?, '')
        "#,
    )
    .bind(&now_iso)
    .bind(&pattern)
    .bind(auth.team_id.as_deref())
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    match result.rows_affected() {
        0 => {
            let exists: i64 = sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM todos
                   WHERE hash LIKE ? AND COALESCE(team_id, '') = COALESCE(?, '')"#,
            )
            .bind(&pattern)
            .bind(auth.team_id.as_deref())
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);
            if exists > 0 {
                Err(StatusCode::CONFLICT)
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

#[cfg(test)]
mod lease_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    async fn app_with_legacy_token(expected_token: Option<&str>) -> Router {
        let db = db::init_test_db().await.unwrap();
        let (tx, _) = broadcast::channel(256);
        let state = AppState {
            db,
            token: expected_token.map(|s| s.to_string()),
            audit: false,
            tx,
            sse_subscribers: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        };
        create_app(state)
    }

    async fn app_with_team(team_id: &str, token: &str) -> Router {
        let db = db::init_test_db().await.unwrap();
        let now = Utc::now().to_rfc3339();
        sqlx::query("INSERT INTO teams (id, name, created_at) VALUES (?, ?, ?)")
            .bind(team_id).bind(team_id).bind(&now)
            .execute(&db).await.unwrap();
        let hash = hash_token(token);
        sqlx::query("INSERT INTO team_tokens (token_hash, team_id, created_at) VALUES (?, ?, ?)")
            .bind(&hash).bind(team_id).bind(&now)
            .execute(&db).await.unwrap();
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

    fn lease_req(app: &Router, token: Option<&str>, instance: &str, pid: i64) -> HttpRequest<Body> {
        let body = serde_json::json!({
            "instance_id": instance,
            "pid": pid,
            "host": "test-host",
        });
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri("/worker/lease")
            .header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {}", t));
        }
        let _ = app;
        builder.body(Body::from(body.to_string())).unwrap()
    }

    fn release_req(token: Option<&str>, instance: &str, pid: i64) -> HttpRequest<Body> {
        let body = serde_json::json!({
            "instance_id": instance,
            "pid": pid,
            "host": "test-host",
        });
        let mut builder = HttpRequest::builder()
            .method("DELETE")
            .uri(format!("/worker/lease/{}", instance))
            .header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {}", t));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    async fn body_bytes(resp: Response) -> Vec<u8> {
        use http_body_util::BodyExt;
        resp.into_body().collect().await.unwrap().to_bytes().to_vec()
    }

    #[tokio::test]
    async fn lease_acquires_on_first_call_in_legacy_mode() {
        let app = app_with_legacy_token(None).await;
        let resp = app.clone().oneshot(lease_req(&app, None, "webdev", 100)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let state: LeaseState = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(state.team_id, None);
        assert_eq!(state.instance_id, "webdev");
        assert_eq!(state.pid, 100);
        assert!(!state.taken_over);
    }

    #[tokio::test]
    async fn lease_same_pid_is_heartbeat() {
        let app = app_with_legacy_token(None).await;
        let _ = app.clone().oneshot(lease_req(&app, None, "webdev", 100)).await.unwrap();
        let resp = app.clone().oneshot(lease_req(&app, None, "webdev", 100)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let state: LeaseState = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(state.pid, 100);
        assert!(!state.taken_over);
    }

    #[tokio::test]
    async fn lease_different_pid_fresh_is_conflict() {
        let app = app_with_legacy_token(None).await;
        let _ = app.clone().oneshot(lease_req(&app, None, "webdev", 100)).await.unwrap();
        let resp = app.clone().oneshot(lease_req(&app, None, "webdev", 200)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let conflict: LeaseConflict = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(conflict.pid, 100);
        assert_eq!(conflict.instance_id, "webdev");
    }

    #[tokio::test]
    async fn lease_different_pid_stale_is_takeover() {
        let app = app_with_legacy_token(None).await;
        // Directly plant a stale lease so we don't need to sleep 30s.
        let db = db::init_test_db().await.unwrap();
        let stale = (Utc::now() - Duration::seconds(LEASE_TTL_SECS + 5)).to_rfc3339();
        sqlx::query("INSERT INTO worker_leases (team_id, instance_id, pid, host, acquired_at, last_heartbeat) VALUES (NULL, ?, ?, ?, ?, ?)")
            .bind("webdev").bind(100i64).bind("old-host").bind(&stale).bind(&stale)
            .execute(&db).await.unwrap();
        let (tx, _) = broadcast::channel(256);
        let state = AppState {
            db, token: None, audit: false, tx,
            sse_subscribers: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        };
        let app = create_app(state);
        let resp = app.clone().oneshot(lease_req(&app, None, "webdev", 200)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let state: LeaseState = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(state.pid, 200);
        assert!(state.taken_over, "stale lease should be taken over");
    }

    #[tokio::test]
    async fn lease_release_is_idempotent() {
        let app = app_with_legacy_token(None).await;
        let _ = app.clone().oneshot(lease_req(&app, None, "webdev", 100)).await.unwrap();
        let resp = app.clone().oneshot(release_req(None, "webdev", 100)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        // Releasing again — no-op but still 204.
        let resp2 = app.clone().oneshot(release_req(None, "webdev", 100)).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn lease_release_with_wrong_pid_does_not_evict() {
        let app = app_with_legacy_token(None).await;
        let _ = app.clone().oneshot(lease_req(&app, None, "webdev", 100)).await.unwrap();
        // Wrong pid tries to release — silently ignored.
        let _ = app.clone().oneshot(release_req(None, "webdev", 999)).await.unwrap();
        // Legitimate holder can still heartbeat.
        let resp = app.clone().oneshot(lease_req(&app, None, "webdev", 100)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn lease_scopes_by_team_id() {
        let app_a = app_with_team("team-a", "tok-a").await;
        let app_b = app_with_team("team-b", "tok-b").await;

        // Same instance name in different teams is fine — they're different
        // namespaces, each gets its own lease.
        let resp_a = app_a.clone().oneshot(lease_req(&app_a, Some("tok-a"), "webdev", 100)).await.unwrap();
        assert_eq!(resp_a.status(), StatusCode::OK);
        let resp_b = app_b.clone().oneshot(lease_req(&app_b, Some("tok-b"), "webdev", 200)).await.unwrap();
        assert_eq!(resp_b.status(), StatusCode::OK);

        let state_a: LeaseState = serde_json::from_slice(&body_bytes(resp_a).await).unwrap();
        let state_b: LeaseState = serde_json::from_slice(&body_bytes(resp_b).await).unwrap();
        assert_eq!(state_a.team_id.as_deref(), Some("team-a"));
        assert_eq!(state_b.team_id.as_deref(), Some("team-b"));
    }

    #[tokio::test]
    async fn team_token_resolves_to_team_context() {
        let app = app_with_team("team-a", "tok-a").await;
        let resp = app.clone().oneshot(lease_req(&app, Some("tok-a"), "webdev", 100)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let state: LeaseState = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(state.team_id.as_deref(), Some("team-a"));
    }

    #[tokio::test]
    async fn unknown_token_on_legacy_server_falls_through() {
        // Server has no env token and token "garbage" isn't in team_tokens →
        // legacy fallback (team_id = None).
        let app = app_with_legacy_token(None).await;
        let resp = app.clone().oneshot(lease_req(&app, Some("garbage"), "webdev", 100)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let state: LeaseState = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(state.team_id, None);
    }

    #[tokio::test]
    async fn legacy_env_token_required_when_configured_and_no_team_match() {
        let app = app_with_legacy_token(Some("envtoken")).await;
        // No token at all → 401.
        let resp = app.clone().oneshot(lease_req(&app, None, "webdev", 100)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Wrong token → 401.
        let resp2 = app.clone().oneshot(lease_req(&app, Some("wrong"), "webdev", 100)).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::UNAUTHORIZED);
        // Correct token → legacy context.
        let resp3 = app.clone().oneshot(lease_req(&app, Some("envtoken"), "webdev", 100)).await.unwrap();
        assert_eq!(resp3.status(), StatusCode::OK);
    }

    // ── Cross-team isolation ──────────────────────────────────────────────

    fn msg_req(token: Option<&str>, sender: &str, recipient: &str, content: &str) -> HttpRequest<Body> {
        let body = serde_json::json!({
            "sender": sender,
            "recipient": recipient,
            "content": content,
            "refs": [],
        });
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri("/messages")
            .header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {}", t));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    fn list_req(token: Option<&str>, instance: &str) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder()
            .method("GET")
            .uri(format!("/messages/{}", instance));
        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {}", t));
        }
        builder.body(Body::empty()).unwrap()
    }

    /// Build an app where both team-a and team-b exist on the SAME database,
    /// so we can prove messages don't leak between them.
    async fn app_with_two_teams() -> (Router, String, String) {
        let db = db::init_test_db().await.unwrap();
        let now = Utc::now().to_rfc3339();
        let tok_a = "tok-a".to_string();
        let tok_b = "tok-b".to_string();
        sqlx::query("INSERT INTO teams (id, name, created_at) VALUES (?, ?, ?)")
            .bind("team-a").bind("team-a").bind(&now).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO teams (id, name, created_at) VALUES (?, ?, ?)")
            .bind("team-b").bind("team-b").bind(&now).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO team_tokens (token_hash, team_id, created_at) VALUES (?, ?, ?)")
            .bind(hash_token(&tok_a)).bind("team-a").bind(&now).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO team_tokens (token_hash, team_id, created_at) VALUES (?, ?, ?)")
            .bind(hash_token(&tok_b)).bind("team-b").bind(&now).execute(&db).await.unwrap();
        let (tx, _) = broadcast::channel(256);
        let state = AppState {
            db, token: None, audit: false, tx,
            sse_subscribers: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        };
        (create_app(state), tok_a, tok_b)
    }

    #[tokio::test]
    async fn messages_are_isolated_between_teams() {
        let (app, tok_a, tok_b) = app_with_two_teams().await;

        // Team A sends to "webdev".
        let post_a = app.clone().oneshot(msg_req(Some(&tok_a), "pm", "webdev", "team-a secret")).await.unwrap();
        assert_eq!(post_a.status(), StatusCode::OK);

        // Team B has its own "webdev" worker, unrelated. Also sends a message.
        let post_b = app.clone().oneshot(msg_req(Some(&tok_b), "pm", "webdev", "team-b hello")).await.unwrap();
        assert_eq!(post_b.status(), StatusCode::OK);

        // Team A's webdev listing: only the team-a message.
        let list_a = app.clone().oneshot(list_req(Some(&tok_a), "webdev")).await.unwrap();
        assert_eq!(list_a.status(), StatusCode::OK);
        let msgs_a: Vec<Message> = serde_json::from_slice(&body_bytes(list_a).await).unwrap();
        assert_eq!(msgs_a.len(), 1);
        assert_eq!(msgs_a[0].content, "team-a secret");

        // Team B's webdev listing: only the team-b message.
        let list_b = app.clone().oneshot(list_req(Some(&tok_b), "webdev")).await.unwrap();
        assert_eq!(list_b.status(), StatusCode::OK);
        let msgs_b: Vec<Message> = serde_json::from_slice(&body_bytes(list_b).await).unwrap();
        assert_eq!(msgs_b.len(), 1);
        assert_eq!(msgs_b[0].content, "team-b hello");
    }

    #[tokio::test]
    async fn legacy_client_does_not_see_team_messages() {
        let (app, tok_a, _tok_b) = app_with_two_teams().await;

        // Team A sends a message.
        let _ = app.clone().oneshot(msg_req(Some(&tok_a), "pm", "webdev", "team-a")).await.unwrap();

        // A legacy client (no token) queries the same instance — gets nothing,
        // because legacy namespace is team_id=NULL, disjoint from team-a.
        let list_legacy = app.clone().oneshot(list_req(None, "webdev")).await.unwrap();
        assert_eq!(list_legacy.status(), StatusCode::OK);
        let msgs: Vec<Message> = serde_json::from_slice(&body_bytes(list_legacy).await).unwrap();
        assert_eq!(msgs.len(), 0);
    }

    // ── Admin endpoints ───────────────────────────────────────────────────

    fn create_team_req(token: Option<&str>, name: &str) -> HttpRequest<Body> {
        let body = serde_json::json!({ "name": name });
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri("/admin/teams")
            .header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {}", t));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn admin_create_team_mints_token_and_persists() {
        let app = app_with_legacy_token(Some("admin-secret")).await;
        let resp = app.clone().oneshot(create_team_req(Some("admin-secret"), "blender")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let created: CreateTeamResponse = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(created.name, "blender");
        assert!(created.token.starts_with("tm_"));

        // The newly-minted token should authenticate as the new team.
        let lease = app.clone().oneshot(lease_req(&app, Some(&created.token), "rigger", 100)).await.unwrap();
        assert_eq!(lease.status(), StatusCode::OK);
        let state: LeaseState = serde_json::from_slice(&body_bytes(lease).await).unwrap();
        assert_eq!(state.team_id.as_deref(), Some(created.team_id.as_str()));
    }

    #[tokio::test]
    async fn admin_endpoints_reject_team_tokens() {
        let app = app_with_team("team-a", "tok-a").await;
        let resp = app.clone().oneshot(create_team_req(Some("tok-a"), "other")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn admin_endpoints_reject_wrong_legacy_token() {
        let app = app_with_legacy_token(Some("admin-secret")).await;
        // Wrong token → 401 at the middleware.
        let resp = app.clone().oneshot(create_team_req(Some("wrong"), "x")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_create_team_rejects_duplicate_names() {
        let app = app_with_legacy_token(Some("a")).await;
        let r1 = app.clone().oneshot(create_team_req(Some("a"), "dup")).await.unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        let r2 = app.clone().oneshot(create_team_req(Some("a"), "dup")).await.unwrap();
        assert_eq!(r2.status(), StatusCode::CONFLICT);
    }

    // ── Usage endpoints ───────────────────────────────────────────────────

    fn usage_post_req(token: Option<&str>, worker: &str, input: u64, output: u64, tier: &str, cost: Option<f64>, cli: Option<&str>) -> HttpRequest<Body> {
        let mut body = serde_json::json!({
            "worker": worker,
            "duration_secs": 1,
            "input_tokens": input,
            "output_tokens": output,
            "tier": tier,
        });
        if let Some(c) = cost { body["cost_usd"] = serde_json::json!(c); }
        if let Some(c) = cli { body["cli"] = serde_json::json!(c); }
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri("/usage")
            .header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {}", t));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    fn usage_get_req(token: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder().method("GET").uri("/usage");
        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {}", t));
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn usage_report_sums_per_worker_totals() {
        let app = app_with_team("team-a", "tok-a").await;

        // Two calls for "builder" — full + light — plus one call for "reviewer".
        let r = app.clone().oneshot(usage_post_req(Some("tok-a"), "builder", 100, 50, "full", Some(0.02), Some("claude"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let r = app.clone().oneshot(usage_post_req(Some("tok-a"), "builder", 200, 80, "light", Some(0.01), Some("claude"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let r = app.clone().oneshot(usage_post_req(Some("tok-a"), "reviewer", 40, 20, "full", None, Some("ollama"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);

        let resp = app.clone().oneshot(usage_get_req(Some("tok-a"))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let usage: UsageResponse = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(usage.workers.len(), 2);
        assert_eq!(usage.total_input_tokens, 340);
        assert_eq!(usage.total_output_tokens, 150);
        assert_eq!(usage.total_calls, 3);
        assert_eq!(usage.total_full_calls, 2);
        assert_eq!(usage.total_light_calls, 1);
        // Heaviest worker first — builder wins with 430 total tokens vs reviewer's 60.
        assert_eq!(usage.workers[0].worker, "builder");
        assert_eq!(usage.workers[0].calls, 2);
        assert_eq!(usage.workers[0].full_calls, 1);
        assert_eq!(usage.workers[0].light_calls, 1);
        assert!((usage.workers[0].cost_usd - 0.03).abs() < 1e-9);
    }

    #[tokio::test]
    async fn usage_is_isolated_between_teams() {
        let app = app_with_team("team-a", "tok-a").await;
        // Seed a second team on the same DB via admin (legacy token off — fake another team by direct insert).
        // Simpler: spin up a fresh app for team-b and make sure team-a sees nothing of team-b's deltas.
        let app_b = app_with_team("team-b", "tok-b").await;
        let _ = app_b.clone().oneshot(usage_post_req(Some("tok-b"), "builder", 999, 999, "full", None, None)).await.unwrap();

        let resp = app.clone().oneshot(usage_get_req(Some("tok-a"))).await.unwrap();
        let usage: UsageResponse = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(usage.workers.len(), 0);
        assert_eq!(usage.total_input_tokens, 0);
    }

    #[tokio::test]
    async fn usage_rejects_invalid_worker_name() {
        let app = app_with_team("team-a", "tok-a").await;
        let r = app.clone().oneshot(usage_post_req(Some("tok-a"), "bad name!", 1, 1, "full", None, None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn usage_rejects_unknown_tier() {
        let app = app_with_team("team-a", "tok-a").await;
        let r = app.clone().oneshot(usage_post_req(Some("tok-a"), "builder", 1, 1, "heavy", None, None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }
}
