use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

/// Sentinel content broadcast to signal all `collab watch` instances to exit.
pub const STOP_WATCH_SIGNAL: &str = "__COLLAB_STOP_WATCH__";

// ── Terminal hyperlinks (OSC 8) ───────────────────────────────────────────────

/// Return the repo base URL for building commit links.
/// Checks COLLAB_REPO env var first (checked on every call, not cached, so tests can override it).
/// Falls back to auto-detecting from `git remote get-url origin`; that result is cached.
/// Converts SSH remotes (git@github.com:user/repo.git) to HTTPS.
pub fn repo_url() -> Option<String> {
    // Always check env var first so callers can override at runtime.
    if let Ok(v) = std::env::var("COLLAB_REPO") {
        let v = v.trim().trim_end_matches('/').to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    // Fall back to cached git remote detection.
    use std::sync::OnceLock;
    static GIT_REMOTE: OnceLock<Option<String>> = OnceLock::new();
    GIT_REMOTE.get_or_init(|| {
        let out = std::process::Command::new("git")
            .args(["remote", "get-url", "origin"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let raw = String::from_utf8(out.stdout).ok()?;
        let raw = raw.trim();
        let url = if let Some(rest) = raw.strip_prefix("git@") {
            // git@github.com:user/repo.git  →  https://github.com/user/repo
            let rest = rest.trim_end_matches(".git");
            format!("https://{}", rest.replacen(':', "/", 1))
        } else {
            // https://github.com/user/repo.git  →  https://github.com/user/repo
            raw.trim_end_matches(".git").to_string()
        };
        Some(url)
    }).clone()
}

/// Return a hash formatted as an OSC 8 terminal hyperlink when stdout is a tty.
/// Auto-detects repo URL from COLLAB_REPO env var or git remote. Falls back to plain text.
fn link_hash(hash: &str) -> String {
    if let (true, Some(repo)) = (is_stdout_tty(), repo_url()) {
        let url = format!("{}/commit/{}", repo, hash);
        format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", url, hash)
    } else {
        hash.to_string()
    }
}

#[cfg(unix)]
fn is_stdout_tty() -> bool {
    use std::os::unix::io::AsRawFd;
    (unsafe { libc::isatty(std::io::stdout().as_raw_fd()) }) == 1
}

#[cfg(not(unix))]
fn is_stdout_tty() -> bool {
    false
}

// ── Stream singleton guard ────────────────────────────────────────────────────

fn stream_lock_path(instance_id: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var("USERPROFILE").ok();
    #[cfg(not(windows))]
    let home = std::env::var("HOME").ok();
    home.map(|h| PathBuf::from(h).join(format!(".collab_stream_{}.pid", instance_id)))
}

/// RAII guard that removes the stream lockfile on drop.
struct LockGuard(Option<PathBuf>);
impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(ref path) = self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ── Read-state persistence ────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ReadState {
    /// instance_id → timestamp of the newest message seen in the last `list` run
    last_read: HashMap<String, DateTime<Utc>>,
    /// instance_id → last role set via `collab watch --role`
    roles: HashMap<String, String>,
    /// set of message hashes we have replied to (via `reply` or `add --refs`)
    #[serde(default)]
    replied: HashSet<String>,
    /// last recipient used in compose modal, per instance_id
    #[serde(default)]
    pub last_compose_recipient: HashMap<String, String>,
}

fn state_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var("USERPROFILE").ok().map(PathBuf::from);
    #[cfg(not(windows))]
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    home.map(|h| h.join(".collab_state.toml"))
}

pub fn load_read_state() -> ReadState {
    state_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_read_state(state: &ReadState) {
    if let Some(path) = state_path() {
        if let Ok(s) = toml::to_string(state) {
            let _ = std::fs::write(path, s);
        }
    }
}

// ── Domain types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub hash: String,
    pub sender: String,
    pub recipient: String,
    pub content: String,
    pub refs: Vec<String>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub instance_id: String,
    pub role: String,
    pub last_seen: DateTime<Utc>,
    pub message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub hash: String,
    pub instance: String,
    pub assigned_by: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

/// The slice of CollabClient that the worker harness depends on. Defined as a
/// trait so tests can substitute a recording fake without touching the network.
/// Methods here intentionally cover only what `process_messages` and the SSE
/// loop need — not the entire CLI surface.
/// Outcome of a lease acquire/heartbeat call. Callers inspect
/// `.conflict` to decide whether to refuse to start.
#[derive(Debug)]
pub enum LeaseOutcome {
    /// Lease is held (either freshly acquired, heartbeat-extended, or
    /// taken over from a stale holder).
    Held { taken_over: bool },
    /// Another process holds a fresh lease for this identity.
    Conflict {
        holder_pid: i64,
        holder_host: String,
        seconds_since_heartbeat: i64,
    },
}

/// Delta report a worker sends to the server after each CLI invocation.
/// Matches `collab_server::UsageReport`.
///
/// `input_tokens` / `cache_creation_tokens` / `cache_read_tokens` are the
/// three disjoint buckets claude's API returns for the prompt side — adding
/// them gives total input tokens. Reporting them separately lets the server
/// (and any /usage consumer) compute cache hit rate as
/// `cache_read / (input + cache_creation + cache_read)` — the signal for
/// whether prompt caching is earning its keep.
#[derive(Debug, Serialize)]
pub struct UsageReport<'a> {
    pub worker: &'a str,
    pub duration_secs: u64,
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub tier: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli: Option<&'a str>,
}

/// Running totals returned by GET /usage for the caller's team. Mirrors
/// `collab_server::UsageResponse` and what the old local usage.log
/// aggregate produced.
#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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

#[async_trait]
pub trait CollabApi: Send + Sync {
    async fn add_message(&self, recipient: &str, content: &str, refs: Option<Vec<String>>) -> Result<()>;
    async fn todo_add(&self, instance: &str, description: &str) -> Result<()>;
    async fn todo_done(&self, hash_prefix: &str) -> Result<()>;
    async fn fetch_pending_messages(&self) -> Result<Vec<Message>>;
    async fn fetch_history_pub(&self, instance_id: &str) -> Result<Vec<Message>>;
    async fn fetch_todos(&self, instance: &str) -> Result<Vec<Todo>>;
    async fn heartbeat(&self, role: Option<&str>) -> Result<()>;
    /// Post a per-call usage delta to the server. Best-effort — the worker
    /// keeps running if the server is unreachable.
    async fn report_usage(&self, report: &UsageReport<'_>) -> Result<()>;

    /// Acquire or extend the singleton worker lease for this instance.
    /// Server-side uniqueness is enforced on (team_id, instance_id) — two
    /// workers with the same name across different teams both succeed,
    /// same-team duplicates get a Conflict.
    async fn acquire_lease(&self, pid: i64, host: &str) -> Result<LeaseOutcome>;
    /// Release the lease. Idempotent; safe to call even when we don't hold it.
    async fn release_lease(&self, pid: i64) -> Result<()>;

    /// Base URL — used by the SSE loop to construct event-stream URLs.
    fn base_url(&self) -> &str;
    /// Bearer token if auth is configured — used by the SSE loop.
    fn bearer_token(&self) -> Option<&str>;
    /// Reqwest client — shared so SSE reuses the connection pool.
    fn http_client(&self) -> &reqwest::Client;
}

#[derive(Clone)]
pub struct CollabClient {
    pub base_url: String,
    pub instance_id: String,
    pub token: Option<String>,
    pub client: reqwest::Client,
}

#[async_trait]
impl CollabApi for CollabClient {
    async fn add_message(&self, recipient: &str, content: &str, refs: Option<Vec<String>>) -> Result<()> {
        CollabClient::add_message(self, recipient, content, refs).await
    }
    async fn todo_add(&self, instance: &str, description: &str) -> Result<()> {
        CollabClient::todo_add(self, instance, description).await
    }
    async fn todo_done(&self, hash_prefix: &str) -> Result<()> {
        CollabClient::todo_done(self, hash_prefix).await
    }
    async fn fetch_pending_messages(&self) -> Result<Vec<Message>> {
        CollabClient::fetch_pending_messages(self).await
    }
    async fn fetch_history_pub(&self, instance_id: &str) -> Result<Vec<Message>> {
        CollabClient::fetch_history_pub(self, instance_id).await
    }
    async fn fetch_todos(&self, instance: &str) -> Result<Vec<Todo>> {
        CollabClient::fetch_todos(self, instance).await
    }
    async fn heartbeat(&self, role: Option<&str>) -> Result<()> {
        CollabClient::heartbeat(self, role).await
    }
    async fn acquire_lease(&self, pid: i64, host: &str) -> Result<LeaseOutcome> {
        CollabClient::acquire_lease(self, pid, host).await
    }
    async fn release_lease(&self, pid: i64) -> Result<()> {
        CollabClient::release_lease(self, pid).await
    }
    async fn report_usage(&self, report: &UsageReport<'_>) -> Result<()> {
        CollabClient::report_usage(self, report).await
    }

    fn base_url(&self) -> &str { &self.base_url }
    fn bearer_token(&self) -> Option<&str> { self.token.as_deref() }
    fn http_client(&self) -> &reqwest::Client { &self.client }
}

impl CollabClient {
    pub fn new(base_url: &str, instance_id: &str, token: Option<&str>) -> Self {
        Self {
            base_url: base_url.to_string(),
            instance_id: instance_id.to_string(),
            token: token.map(|t| t.to_string()),
            client: reqwest::Client::new(),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            req.header("Authorization", format!("Bearer {}", token))
        } else {
            req
        }
    }

    pub async fn heartbeat(&self, role: Option<&str>) -> Result<()> {
        #[derive(Serialize)]
        struct PresenceUpdate {
            role: Option<String>,
        }

        let url = format!("{}/presence/{}", self.base_url, self.instance_id);
        self.auth(self.client.put(&url))
            .json(&PresenceUpdate { role: role.map(|r| r.to_string()) })
            .send()
            .await?;
        Ok(())
    }

    /// Append a per-call usage delta to the server's running totals. The
    /// server upserts on (team_id, worker) — no per-call rows kept.
    pub async fn report_usage(&self, report: &UsageReport<'_>) -> Result<()> {
        let url = format!("{}/usage", self.base_url);
        let resp = self.auth(self.client.post(&url))
            .json(report)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("usage report failed: HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// Fetch team-scoped running totals. Scoping is by the bearer token.
    pub async fn fetch_usage(&self) -> Result<UsageResponse> {
        let url = format!("{}/usage", self.base_url);
        let resp = self.auth(self.client.get(&url)).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("fetch usage failed: HTTP {}", resp.status()));
        }
        Ok(resp.json::<UsageResponse>().await?)
    }

    /// Acquire or heartbeat the singleton worker lease. Callers pass their
    /// own pid; we stamp it into the lease row so a second process claiming
    /// the same identity can be rejected with 409 before it burns CLI
    /// invocations. See `LeaseOutcome` for outcomes.
    pub async fn acquire_lease(&self, pid: i64, host: &str) -> Result<LeaseOutcome> {
        #[derive(Serialize)]
        struct LeaseRequest<'a> {
            instance_id: &'a str,
            pid: i64,
            host: &'a str,
        }
        #[derive(Deserialize)]
        struct LeaseState {
            #[serde(default)]
            taken_over: bool,
        }
        #[derive(Deserialize)]
        struct LeaseConflict {
            pid: i64,
            host: String,
            seconds_since_heartbeat: i64,
        }

        let url = format!("{}/worker/lease", self.base_url);
        let resp = self
            .auth(self.client.post(&url))
            .json(&LeaseRequest {
                instance_id: &self.instance_id,
                pid,
                host,
            })
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() {
            let state: LeaseState = resp.json().await?;
            Ok(LeaseOutcome::Held { taken_over: state.taken_over })
        } else if status.as_u16() == 409 {
            let conflict: LeaseConflict = resp.json().await?;
            Ok(LeaseOutcome::Conflict {
                holder_pid: conflict.pid,
                holder_host: conflict.host,
                seconds_since_heartbeat: conflict.seconds_since_heartbeat,
            })
        } else {
            anyhow::bail!("lease acquire failed: HTTP {}", status);
        }
    }

    pub async fn release_lease(&self, pid: i64) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            instance_id: &'a str,
            pid: i64,
            host: &'a str,
        }
        let url = format!("{}/worker/lease/{}", self.base_url, self.instance_id);
        // Host isn't needed for release, but the endpoint shares the
        // LeaseRequest schema — we send a placeholder.
        self.auth(self.client.delete(&url))
            .json(&Body {
                instance_id: &self.instance_id,
                pid,
                host: "release",
            })
            .send()
            .await?;
        Ok(())
    }

    pub async fn list_messages(&self, unread_only: bool, from_filter: Option<&str>, since_hash: Option<&str>) -> Result<()> {
        let url = format!("{}/messages/{}", self.base_url, self.instance_id);

        let response = self.auth(self.client.get(&url)).send().await?;

        if !response.status().is_success() {
            // Include the URL so the user (and the config-resolution test
            // suite) can see which server we tried — a bare `401 Unauthorized`
            // is useless when you're debugging which config got loaded.
            anyhow::bail!("Failed to fetch messages from {}: {}", url, response.status());
        }

        let mut messages: Vec<Message> = response.json().await?;

        // Filter out control signals — they're internal plumbing, not human messages.
        messages.retain(|m| m.content.trim() != STOP_WATCH_SIGNAL);

        // Filter out self-broadcasts — you sent them, you don't need to read them back.
        messages.retain(|m| !(m.sender == self.instance_id && m.recipient == "all"));

        let mut state = load_read_state();
        let last_read = state.last_read.get(&self.instance_id).copied();

        // --since <hash>: show messages after the message with this hash prefix
        if let Some(prefix) = since_hash {
            let prefix = prefix.trim_start_matches('@').to_lowercase();
            // Find the timestamp of the referenced message (search history for it)
            let history_url = format!("{}/history/{}", self.base_url, self.instance_id);
            if let Ok(resp) = self.auth(self.client.get(&history_url)).send().await {
                if let Ok(history) = resp.json::<Vec<Message>>().await {
                    if let Some(anchor) = history.iter().find(|m| m.hash.starts_with(&prefix)) {
                        let anchor_ts = anchor.timestamp;
                        messages.retain(|m| m.timestamp > anchor_ts);
                    } else {
                        anyhow::bail!("No message found with hash starting '{}'", prefix);
                    }
                }
            }
        } else if unread_only {
            if let Some(since) = last_read {
                messages.retain(|m| m.timestamp > since);
            }
        }

        if let Some(sender) = from_filter {
            let sender = sender.trim_start_matches('@');
            messages.retain(|m| m.sender == sender);
        }

        // Update last_read based on all messages (before --from filter narrows the set)
        // We already have all_messages before the from_filter retain, but we applied retain in-place.
        // Use the filtered set for unread tracking — seeing messages from @kali marks them read too.
        if let Some(newest) = messages.iter().map(|m| m.timestamp).max() {
            let current = last_read.unwrap_or(DateTime::<Utc>::MIN_UTC);
            if newest > current {
                state.last_read.insert(self.instance_id.clone(), newest);
                save_read_state(&state);
            }
        }

        if messages.is_empty() {
            return Ok(());
        }

        println!("Messages for @{}:\n", self.instance_id);
        for msg in &messages {
            let replied = state.replied.contains(&msg.hash);
            let short_hash = link_hash(&msg.hash[..7]);
            let tag = if replied {
                " [replied]"
            } else if msg.recipient == "all" {
                " [broadcast]"
            } else {
                ""
            };
            println!("─────────────────────────────────────");
            println!("← @{}  {}{}",  msg.sender, short_hash, tag);
            println!("Time: {}", msg.timestamp.format("%Y-%m-%d %H:%M:%S UTC"));
            if !msg.refs.is_empty() {
                let short_refs: Vec<String> = msg.refs.iter()
                    .map(|r| link_hash(&r.chars().take(7).collect::<String>()))
                    .collect();
                println!("Refs: {}", short_refs.join(", "));
            }
            println!("\n{}\n", msg.content);
        }
        println!("─────────────────────────────────────");

        Ok(())
    }

    pub async fn reply_to_latest(&self, sender: &str, content: &str) -> Result<()> {
        let sender = sender.trim_start_matches('@');
        let url = format!("{}/history/{}", self.base_url, self.instance_id);
        let response = self.auth(self.client.get(&url)).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch history: {}", response.status());
        }
        let messages: Vec<Message> = response.json().await?;
        let latest = messages.iter()
            .filter(|m| m.sender == sender)
            .max_by_key(|m| m.timestamp);
        match latest {
            None => anyhow::bail!("No messages found from @{}", sender),
            Some(msg) => {
                println!("Replying to {} [{}] from @{}", msg.timestamp.format("%H:%M:%S UTC"), link_hash(&msg.hash[..7]), sender);
                self.add_message(sender, content, Some(vec![msg.hash.clone()])).await
            }
        }
    }

    pub async fn show_message(&self, hash_prefix: &str) -> Result<()> {
        let url = format!("{}/history/{}", self.base_url, self.instance_id);
        let response = self.auth(self.client.get(&url)).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch history: {}", response.status());
        }
        let messages: Vec<Message> = response.json().await?;
        let prefix = hash_prefix.trim_start_matches('@').to_lowercase();
        let matches: Vec<&Message> = messages.iter()
            .filter(|m| m.hash.starts_with(&prefix))
            .collect();
        match matches.len() {
            0 => anyhow::bail!("No message found with hash starting '{}'", prefix),
            n if n > 1 => anyhow::bail!("Ambiguous: {} messages match '{}', use more characters", n, prefix),
            _ => {
                let msg = matches[0];
                let direction = if msg.sender == self.instance_id {
                    format!("you → @{}", msg.recipient)
                } else {
                    format!("@{} → you", msg.sender)
                };
                println!("─────────────────────────────────────");
                println!("Hash: {}", link_hash(&msg.hash[..7]));
                println!("From: @{}  To: @{}", msg.sender, msg.recipient);
                println!("Dir:  {}", direction);
                println!("Time: {}", msg.timestamp.format("%Y-%m-%d %H:%M:%S UTC"));
                if !msg.refs.is_empty() {
                    let short_refs: Vec<String> = msg.refs.iter()
                        .map(|r| link_hash(&r.chars().take(7).collect::<String>()))
                        .collect();
                    println!("Refs: {}", short_refs.join(", "));
                }
                println!("\n{}", msg.content);
                println!("─────────────────────────────────────");
            }
        }
        Ok(())
    }

    pub async fn show_status(&self) -> Result<()> {
        let todos_url = format!("{}/todos/{}", self.base_url, self.instance_id);
        let (roster_r, messages_r, todos_r) = tokio::join!(
            self.fetch_roster_pub(),
            async {
                let url = format!("{}/messages/{}", self.base_url, self.instance_id);
                let resp = self.auth(self.client.get(&url)).send().await?;
                resp.json::<Vec<Message>>().await.map_err(anyhow::Error::from)
            },
            async {
                let resp = self.auth(self.client.get(&todos_url)).send().await?;
                resp.json::<Vec<Todo>>().await.map_err(anyhow::Error::from)
            }
        );

        // Roster
        match roster_r {
            Ok(workers) => {
                if workers.is_empty() {
                    println!("No active workers.\n");
                } else {
                    println!("Active workers:\n");
                    for worker in &workers {
                        let you = if worker.instance_id == self.instance_id { " ◀ you" } else { "" };
                        print!("  @{}{}", worker.instance_id, you);
                        if !worker.role.is_empty() {
                            print!("  —  {}", worker.role);
                        }
                        println!();
                        println!("    Last seen: {}", worker.last_seen.format("%H:%M:%S UTC"));
                        println!();
                    }
                }
            }
            Err(e) => eprintln!("Warning: could not fetch roster: {}", e),
        }

        // Unread messages
        let mut state = load_read_state();
        let last_read = state.last_read.get(&self.instance_id).copied();

        match messages_r {
            Ok(all_messages) => {
                // Update last_read from the first fetch before filtering, so we don't
                // make a second request (which could mark new messages as read before showing them)
                if let Some(newest) = all_messages.iter().map(|m| m.timestamp).max() {
                    let current = last_read.unwrap_or(DateTime::<Utc>::MIN_UTC);
                    if newest > current {
                        state.last_read.insert(self.instance_id.clone(), newest);
                        save_read_state(&state);
                    }
                }
                let mut messages = all_messages;
                messages.retain(|m| m.content.trim() != STOP_WATCH_SIGNAL);
                messages.retain(|m| !(m.sender == self.instance_id && m.recipient == "all"));
                if let Some(since) = last_read {
                    messages.retain(|m| m.timestamp > since);
                }
                if messages.is_empty() {
                } else {
                    println!("Unread messages for @{}:\n", self.instance_id);
                    for msg in &messages {
                        let short_hash = link_hash(&msg.hash[..7]);
                        let tag = if msg.recipient == "all" { " [broadcast]" } else { "" };
                        println!("─────────────────────────────────────");
                        println!("← @{}  {}{}", msg.sender, short_hash, tag);
                        println!("Time: {}", msg.timestamp.format("%Y-%m-%d %H:%M:%S UTC"));
                        if !msg.refs.is_empty() {
                            let short_refs: Vec<String> = msg.refs.iter()
                                .map(|r| link_hash(&r.chars().take(7).collect::<String>()))
                                .collect();
                            println!("Refs: {}", short_refs.join(", "));
                        }
                        println!("\n{}\n", msg.content);
                    }
                    println!("─────────────────────────────────────");
                }
            }
            Err(e) => eprintln!("Warning: could not fetch messages: {}", e),
        }

        // Pending todos
        match todos_r {
            Ok(todos) if !todos.is_empty() => {
                println!("Pending tasks for @{}:\n", self.instance_id);
                for todo in &todos {
                    println!("  {}  {}", &todo.hash[..7], todo.description);
                    println!("  from @{}  —  {}", todo.assigned_by, todo.created_at.format("%Y-%m-%d %H:%M UTC"));
                    println!();
                }
            }
            Ok(_) => {} // no todos — silent
            Err(_) => {} // server may not support todos yet — silent
        }

        Ok(())
    }

    /// Send a message and return the resulting Message object (no stdout output).
    /// Fetch pending messages for this instance from the REST API.
    /// Used by the worker on startup to pick up messages that arrived while it was offline.
    pub async fn fetch_pending_messages(&self) -> Result<Vec<Message>> {
        let url = format!("{}/messages/{}", self.base_url, self.instance_id);
        let response = self.auth(self.client.get(&url)).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch messages: {}", response.status());
        }
        let messages: Vec<Message> = response.json().await?;
        Ok(messages)
    }

    pub async fn send_message_raw(
        &self,
        recipient: &str,
        content: &str,
        refs: Vec<String>,
    ) -> Result<Message> {
        #[derive(Serialize)]
        struct CreateMessage {
            sender: String,
            recipient: String,
            content: String,
            refs: Vec<String>,
        }

        let payload = CreateMessage {
            sender: self.instance_id.clone(),
            recipient: recipient.to_string(),
            content: content.to_string(),
            refs: refs.clone(),
        };

        let url = format!("{}/messages", self.base_url);
        let response = self.auth(self.client.post(&url))
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to send message: {}", response.status());
        }

        let msg: Message = response.json().await?;

        // Mark any referenced messages as replied
        if !refs.is_empty() {
            let mut state = load_read_state();
            for h in &refs {
                state.replied.insert(h.clone());
            }
            save_read_state(&state);
        }

        Ok(msg)
    }

    pub async fn add_message(
        &self,
        recipient: &str,
        content: &str,
        refs: Option<Vec<String>>,
    ) -> Result<()> {
        let msg = self.send_message_raw(recipient, content, refs.unwrap_or_default()).await?;
        println!("→ @{}  {}", recipient, link_hash(&msg.hash[..7]));
        println!("  Time: {}", msg.timestamp.format("%Y-%m-%d %H:%M:%S UTC"));
        Ok(())
    }

    pub async fn delete_presence(&self) -> Result<()> {
        let url = format!("{}/presence/{}", self.base_url, self.instance_id);
        self.auth(self.client.delete(&url)).send().await?;
        Ok(())
    }

    /// Broadcast the stop signal and clear all roster presence entries.
    pub async fn stop_all(&self) -> Result<()> {
        // 1. Broadcast the stop signal so stream instances exit.
        let msg = self.send_message_raw("all", STOP_WATCH_SIGNAL, vec![]).await?;
        println!("⛔ Stop signal broadcast to @all  [{}]", link_hash(&msg.hash[..7]));

        // 2. Clear presence for all workers currently in the roster.
        match self.fetch_roster_pub().await {
            Ok(workers) => {
                for worker in &workers {
                    let url = format!("{}/presence/{}", self.base_url, worker.instance_id);
                    let _ = self.auth(self.client.delete(&url)).send().await;
                }
                println!("  Cleared presence for {} worker(s) — roster is now empty.", workers.len());
            }
            Err(e) => eprintln!("  Warning: could not clear roster: {}", e),
        }

        println!("  Running `collab stream` instances will exit.");
        Ok(())
    }

    pub async fn broadcast(&self, content: &str, refs: Option<Vec<String>>) -> Result<()> {
        let ref_hashes = refs.unwrap_or_default();
        let msg = self.send_message_raw("all", content, ref_hashes).await?;
        println!("→ @all  {}  [broadcast]", link_hash(&msg.hash[..7]));
        println!("  Time: {}", msg.timestamp.format("%Y-%m-%d %H:%M:%S UTC"));
        println!("  (visible to all workers on their next `collab list`)");
        Ok(())
    }

    pub async fn stream_messages(&self, role: Option<String>) -> Result<()> {
        use tokio::time::{sleep, Duration};

        // Singleton guard: prevent multiple stream processes for the same instance.
        let _lock_guard = if let Some(path) = stream_lock_path(&self.instance_id) {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Ok(pid) = contents.trim().parse::<u32>() {
                    #[cfg(unix)]
                    let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
                    #[cfg(not(unix))]
                    let alive = false;
                    if alive {
                        anyhow::bail!(
                            "collab stream is already running for @{} (PID {})\n\
                             Kill it first:  kill {}",
                            self.instance_id, pid, pid
                        );
                    }
                }
            }
            let _ = std::fs::write(&path, std::process::id().to_string());
            LockGuard(Some(path))
        } else {
            LockGuard(None)
        };

        // Ignore SIGHUP so the stream survives being backgrounded or the
        // controlling terminal closing (e.g. `collab stream &`, nohup-less).
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            if let Ok(mut sighup) = signal(SignalKind::hangup()) {
                tokio::spawn(async move {
                    loop { sighup.recv().await; }
                });
            }
        }

        // Persist role
        let mut state = load_read_state();
        let effective_role = role.clone().or_else(|| state.roles.get(&self.instance_id).cloned());
        if let Some(ref r) = role {
            state.roles.insert(self.instance_id.clone(), r.clone());
            save_read_state(&state);
        }
        let role_str = effective_role.clone();

        println!("Streaming messages for @{} (SSE — zero polling)", self.instance_id);
        println!("Press Ctrl+C to stop\n");

        // Heartbeat presence in background
        let hb_client = self.clone();
        let hb_role = role_str.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let _ = hb_client.heartbeat(hb_role.as_deref()).await;
            }
        });

        // Initial heartbeat
        let _ = self.heartbeat(role_str.as_deref()).await;

        let mut backoff_secs = 1u64;

        loop {
            let url = format!("{}/events/{}", self.base_url, self.instance_id);
            let mut req = self.client.get(&url).header("Accept", "text/event-stream");
            if let Some(token) = &self.token {
                req = req.header("Authorization", format!("Bearer {}", token));
            }

            match req.send().await {
                Ok(response) if response.status().is_success() => {
                    backoff_secs = 1; // reset on successful connect
                    println!("── connected ──");

                    let mut buffer = String::new();
                    let mut response = response;

                    loop {
                        match response.chunk().await {
                            Ok(Some(chunk)) => {
                                buffer.push_str(&String::from_utf8_lossy(&chunk));
                                // Process complete SSE events (delimited by \n\n)
                                while let Some(end) = buffer.find("\n\n") {
                                    let event_str = buffer[..end].to_string();
                                    buffer.drain(..end + 2);
                                    for line in event_str.lines() {
                                        if let Some(data) = line.strip_prefix("data: ") {
                                            if let Ok(msg) = serde_json::from_str::<Message>(data) {
                                                if msg.content.trim() == STOP_WATCH_SIGNAL {
                                                    println!("⛔ Stop signal received from @{} — clearing presence and exiting.", msg.sender);
                                                    let _ = self.delete_presence().await;
                                                    return Ok(());
                                                }
                                                let tag = if msg.recipient == "all" { " [broadcast]" } else { "" };
                                                println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                                                println!("← @{}{}  {}", msg.sender, tag, msg.timestamp.format("%H:%M:%S UTC"));
                                                println!("Hash: {}", link_hash(&msg.hash[..7]));
                                                if !msg.refs.is_empty() {
                                                    let short_refs: Vec<String> = msg.refs.iter()
                                                        .map(|r| link_hash(&r.chars().take(7).collect::<String>()))
                                                        .collect();
                                                    println!("Refs: {}", short_refs.join(", "));
                                                }
                                                println!("\n{}\n", msg.content);
                                                println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(None) => {
                                println!("── connection closed, reconnecting in {}s ──", backoff_secs);
                                break;
                            }
                            Err(e) => {
                                eprintln!("── stream error: {} — reconnecting in {}s ──", e, backoff_secs);
                                break;
                            }
                        }
                    }
                }
                Ok(response) => {
                    eprintln!("── server error: {} — reconnecting in {}s ──", response.status(), backoff_secs);
                }
                Err(e) => {
                    eprintln!("── connection error: {} — reconnecting in {}s ──", e, backoff_secs);
                }
            }

            sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);
        }
    }

    async fn fetch_roster(&self) -> Result<Vec<WorkerInfo>> {
        self.fetch_roster_pub().await
    }

    pub async fn fetch_roster_pub(&self) -> Result<Vec<WorkerInfo>> {
        let url = format!("{}/roster", self.base_url);
        let response = self.auth(self.client.get(&url)).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("Server error: {}", response.status());
        }
        Ok(response.json::<Vec<WorkerInfo>>().await?)
    }

    pub async fn fetch_history_pub(&self, instance_id: &str) -> Result<Vec<Message>> {
        let url = format!("{}/history/{}", self.base_url, instance_id);
        let response = self.auth(self.client.get(&url)).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("Server error: {}", response.status());
        }
        Ok(response.json::<Vec<Message>>().await?)
    }

    pub async fn show_history(&self, filter_instance: Option<&str>) -> Result<()> {
        let url = format!("{}/history/{}", self.base_url, self.instance_id);

        let response = self.auth(self.client.get(&url)).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch history: {}", response.status());
        }

        let mut messages: Vec<Message> = response.json().await?;

        if let Some(filter_id) = filter_instance {
            messages.retain(|msg| msg.sender == filter_id || msg.recipient == filter_id);
        }

        if messages.is_empty() {
            println!("No message history in the last hour.");
            if let Some(filter_id) = filter_instance {
                println!("(filtered to conversations with @{})", filter_id);
            }
            return Ok(());
        }

        println!("Message History for @{}:\n", self.instance_id);
        if let Some(filter_id) = filter_instance {
            println!("(showing only conversations with @{})\n", filter_id);
        }

        for msg in messages {
            let direction = if msg.sender == self.instance_id {
                format!("→ @{}", msg.recipient)
            } else {
                format!("← @{}", msg.sender)
            };

            println!("─────────────────────────────────────");
            println!("{} [{}]", direction, link_hash(&msg.hash[..7]));
            println!("Time: {}", msg.timestamp.format("%Y-%m-%d %H:%M:%S UTC"));
            if !msg.refs.is_empty() {
                let short_refs: Vec<String> = msg.refs.iter()
                    .map(|r| link_hash(&r.chars().take(7).collect::<String>()))
                    .collect();
                println!("Refs: {}", short_refs.join(", "));
            }
            println!("\n{}\n", msg.content);
        }
        println!("─────────────────────────────────────");

        Ok(())
    }

    pub async fn todo_add(&self, instance: &str, description: &str) -> Result<()> {
        #[derive(Serialize)]
        struct TodoCreate {
            assigned_by: String,
            instance: String,
            description: String,
        }

        let payload = TodoCreate {
            assigned_by: self.instance_id.clone(),
            instance: instance.to_string(),
            description: description.to_string(),
        };

        let url = format!("{}/todos", self.base_url);
        let resp = self.auth(self.client.post(&url)).json(&payload).send().await?;

        if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            anyhow::bail!("Bad request — check instance ID and description length");
        }
        if !resp.status().is_success() {
            anyhow::bail!("Server error: {}", resp.status());
        }

        let todo: Todo = resp.json().await?;
        println!("→ @{}  {}", todo.instance, &todo.hash[..7]);
        println!("  {}", todo.description);

        // Do NOT post a "new task assigned" notification here. The server
        // inserts one atomically with the todo (see collab-server:create_todo)
        // and broadcasts it via SSE — doing it here too produced a visible
        // duplicate, once without the emoji (server) and once with (client).
        Ok(())
    }

    pub async fn fetch_todos(&self, instance: &str) -> Result<Vec<Todo>> {
        let url = format!("{}/todos/{}", self.base_url, instance);
        let resp = self.auth(self.client.get(&url)).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("Server error: {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    pub async fn todo_list(&self, instance: Option<&str>) -> Result<()> {
        let target = instance.unwrap_or(&self.instance_id);
        let url = format!("{}/todos/{}", self.base_url, target);
        let resp = self.auth(self.client.get(&url)).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!("Server error: {}", resp.status());
        }

        let todos: Vec<Todo> = resp.json().await?;

        if todos.is_empty() {
            println!("No pending tasks for @{}.", target);
            return Ok(());
        }

        println!("Pending tasks for @{}:\n", target);
        for todo in &todos {
            println!("─────────────────────────────────────");
            println!("  {}  (from @{})", &todo.hash[..7], todo.assigned_by);
            println!("  {}", todo.description);
            println!("  Assigned: {}", todo.created_at.format("%Y-%m-%d %H:%M UTC"));
        }
        println!("─────────────────────────────────────");
        Ok(())
    }

    pub async fn todo_done(&self, hash_prefix: &str) -> Result<()> {
        let url = format!("{}/todos/{}/done", self.base_url, hash_prefix);
        let resp = self.auth(self.client.patch(&url)).send().await?;

        match resp.status() {
            s if s == reqwest::StatusCode::NO_CONTENT => {
                println!("✓  Task {} marked complete.", hash_prefix);
                Ok(())
            }
            s if s == reqwest::StatusCode::CONFLICT => {
                anyhow::bail!("Task {} already completed (409).", hash_prefix)
            }
            s if s == reqwest::StatusCode::NOT_FOUND => {
                anyhow::bail!("Task {} not found.", hash_prefix)
            }
            s => anyhow::bail!("Server error: {}", s),
        }
    }

    pub async fn show_roster(&self) -> Result<()> {
        let workers = self.fetch_roster().await?;

        if workers.is_empty() {
            println!("No active workers.");
            return Ok(());
        }

        println!("Active workers:\n");
        for worker in workers {
            let you = if worker.instance_id == self.instance_id { " (you)" } else { "" };
            print!("  @{}{}", worker.instance_id, you);
            if !worker.role.is_empty() {
                print!("  —  {}", worker.role);
            }
            println!();
            println!("    Last seen: {}", worker.last_seen.format("%H:%M:%S UTC"));
            if worker.message_count > 0 {
                println!("    Messages: {}", worker.message_count);
            }
            println!();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialization() {
        let message = Message {
            id: "test-id".to_string(),
            hash: "abc123".to_string(),
            sender: "worker1".to_string(),
            recipient: "worker2".to_string(),
            content: "test content".to_string(),
            refs: vec!["ref1".to_string()],
            timestamp: Utc::now(),
        };

        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("test-id"));
        assert!(json.contains("worker1"));
    }

    #[test]
    fn test_message_deserialization() {
        let json = r#"{
            "id": "test-id",
            "hash": "abc123",
            "sender": "worker1",
            "recipient": "worker2",
            "content": "test content",
            "refs": ["ref1"],
            "timestamp": "2024-03-27T14:30:45Z"
        }"#;

        let message: Message = serde_json::from_str(json).unwrap();
        assert_eq!(message.id, "test-id");
        assert_eq!(message.sender, "worker1");
        assert_eq!(message.refs.len(), 1);
    }

    #[test]
    fn test_collab_client_creation() {
        let client = CollabClient::new("http://localhost:8000", "test-worker", None);
        assert_eq!(client.base_url, "http://localhost:8000");
        assert_eq!(client.instance_id, "test-worker");
    }
}
