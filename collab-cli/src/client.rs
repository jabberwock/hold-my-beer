use anyhow::Result;
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

#[derive(Clone)]
pub struct CollabClient {
    base_url: String,
    instance_id: String,
    token: Option<String>,
    client: reqwest::Client,
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

    pub async fn list_messages(&self, unread_only: bool, from_filter: Option<&str>, since_hash: Option<&str>) -> Result<()> {
        let url = format!("{}/messages/{}", self.base_url, self.instance_id);

        let response = self.auth(self.client.get(&url)).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch messages: {}", response.status());
        }

        let mut messages: Vec<Message> = response.json().await?;

        // Filter out control signals — they're internal plumbing, not human messages.
        messages.retain(|m| m.content.trim() != STOP_WATCH_SIGNAL);

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
        let (roster_r, messages_r) = tokio::join!(
            self.fetch_roster_pub(),
            async {
                let url = format!("{}/messages/{}", self.base_url, self.instance_id);
                let resp = self.auth(self.client.get(&url)).send().await?;
                resp.json::<Vec<Message>>().await.map_err(anyhow::Error::from)
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

        Ok(())
    }

    /// Send a message and return the resulting Message object (no stdout output).
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

    /// Broadcast the stop-watch signal and clear all roster presence entries.
    pub async fn stop_all(&self) -> Result<()> {
        // 1. Broadcast the stop-watch signal so watch loops exit.
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

        println!("  Running `collab watch` instances will exit on next poll.");
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

    pub async fn watch_messages(
        &self,
        interval_secs: u64,
        role: Option<String>,
        recipients: Vec<String>,
    ) -> Result<()> {
        use tokio::time::{sleep, Duration};

        let mut seen_ids: HashSet<String> = HashSet::new();
        // Track which recipients we've already warned about
        let mut warned_missing: HashSet<String> = HashSet::new();
        // Track which recipients have been seen at least once
        let mut seen_recipients: HashSet<String> = HashSet::new();

        // Persist role across context resets: use provided role, fall back to saved role
        let mut state = load_read_state();
        let effective_role = role.clone().or_else(|| {
            state.roles.get(&self.instance_id).cloned()
        });
        if let Some(ref r) = role {
            state.roles.insert(self.instance_id.clone(), r.clone());
            save_read_state(&state);
        }
        let role_str = effective_role.as_deref();

        println!("Watching for messages to @{} (polling every {}s)", self.instance_id, interval_secs);
        if !recipients.is_empty() {
            println!("Waiting for: {}", recipients.iter().map(|r| format!("@{}", r)).collect::<Vec<_>>().join(", "));
        }
        println!("Press Ctrl+C to stop\n");

        loop {
            // Heartbeat presence
            if let Err(e) = self.heartbeat(role_str).await {
                eprintln!("Warning: presence heartbeat failed: {}", e);
            }

            // Check roster for configured recipients
            if !recipients.is_empty() {
                if let Ok(roster) = self.fetch_roster().await {
                    let online: HashSet<String> = roster.iter().map(|w| w.instance_id.clone()).collect();

                    for recipient in &recipients {
                        let r = recipient.trim_start_matches('@').to_string();
                        if online.contains(&r) {
                            if !seen_recipients.contains(&r) {
                                seen_recipients.insert(r.clone());
                                warned_missing.remove(&r);
                                println!("── @{} is online ──", r);
                            }
                        } else if !warned_missing.contains(&r) {
                            warned_missing.insert(r.clone());
                            // Only warn after they were previously seen (went offline)
                            if seen_recipients.contains(&r) {
                                println!("── @{} went offline ──", r);
                            }
                        }
                    }

                    // On first poll, report any recipients not yet online
                    if seen_ids.is_empty() {
                        let missing: Vec<_> = recipients.iter()
                            .filter(|r| {
                                let r = r.trim_start_matches('@');
                                !online.contains(r)
                            })
                            .map(|r| format!("@{}", r.trim_start_matches('@')))
                            .collect();
                        if !missing.is_empty() {
                            println!("Not yet online: {}", missing.join(", "));
                        }
                    }
                }
            }

            // Poll for new messages
            let url = format!("{}/messages/{}", self.base_url, self.instance_id);
            match self.auth(self.client.get(&url)).send().await {
                Ok(response) if response.status().is_success() => {
                    match response.json::<Vec<Message>>().await {
                        Ok(messages) => {
                            let new_messages: Vec<_> = messages
                                .into_iter()
                                .filter(|msg| !seen_ids.contains(&msg.id))
                                .collect();

                            for msg in &new_messages {
                                seen_ids.insert(msg.id.clone());

                                // Stop-watch signal: clear own presence and exit gracefully.
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
                        Err(e) => eprintln!("Warning: failed to parse messages: {}", e),
                    }
                }
                Ok(response) => eprintln!("Warning: server error: {}", response.status()),
                Err(e) => eprintln!("Warning: connection error: {}", e),
            }

            sleep(Duration::from_secs(interval_secs)).await;
        }
    }

    pub async fn stream_messages(&self, role: Option<String>) -> Result<()> {
        use tokio::time::{sleep, Duration};

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
