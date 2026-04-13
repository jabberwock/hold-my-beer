use anyhow::Result;
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration, Instant};

use crate::client::CollabClient;

const TRIVIAL_REPLY_PATTERN: &str = r"(?i)^(acknowledged|got it|thanks|thank you|ok|okay|will do|on it|roger)$";
const PING_PATTERN: &str = r"(?i)^(ping|status|are you there\??|health ?check|you up\??)$";
/// Matches messages that are pure acknowledgments — no new information, just confirming receipt.
/// These start with ack-like phrases and contain no task assignments or new requests.
const ACK_START_PATTERN: &str = r"(?i)^(@[\w-]+\s+)*\s*(acknowledged|ack\b|aligned|standing by|same gate|holding|received|noted|roger|unchanged|freeze (holds|respected|unchanged)|gate freeze|doc freeze|standby)";
pub const DEFAULT_CLI_TEMPLATE: &str = "claude -p {prompt} --model {model} --allowedTools Bash,Read,Write,Edit";

#[derive(Debug, Clone, Copy, PartialEq)]
enum PromptTier {
    /// Handled entirely by the harness — no CLI spawn
    Harness,
    /// Minimal prompt — role + message + compact schema
    Light,
    /// Full prompt — teammates, state, todos, full schema
    Full,
}

impl std::fmt::Display for PromptTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptTier::Harness => write!(f, "harness"),
            PromptTier::Light => write!(f, "light"),
            PromptTier::Full => write!(f, "full"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub sender: String,
    pub content: String,
    pub hash: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub recipient: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerState {
    #[serde(default)]
    pub last_task: Option<String>,
    #[serde(default)]
    pub pending: Option<String>,
    #[serde(default)]
    pub files_touched: Vec<String>,
    /// Shown on roster — what this worker is currently doing
    #[serde(default)]
    pub status: Option<String>,
}

/// Deserialize a Vec that might be null (models output null instead of [])
fn null_as_empty_vec<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(|v| v.unwrap_or_default())
}

#[derive(Debug, Serialize, Deserialize)]
struct CollabOutput {
    #[serde(default)]
    pub response: Option<String>,
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    pub delegate: Vec<DelegateTask>,
    #[serde(default)]
    pub state_update: WorkerState,
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    pub completed_tasks: Vec<String>,
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    pub messages: Vec<DirectMessage>,
    #[serde(default)]
    pub r#continue: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct DelegateTask {
    pub to: String,
    pub task: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct DirectMessage {
    pub to: String,
    pub text: String,
}

pub struct WorkerHarness {
    client: Arc<CollabClient>,
    instance_id: String,
    workdir: PathBuf,
    model: String,
    /// CLI command template for full-tier (agent mode) — {prompt}, {model}, {workdir} placeholders
    cli_template: String,
    /// CLI command template for light-tier (plan/think mode) — if unset, falls back to cli_template
    cli_template_light: Option<String>,
    auto_reply: bool,
    batch_wait_ms: u64,
    message_queue: Arc<Mutex<Vec<Message>>>,
    first_message_time: Arc<Mutex<Option<Instant>>>,
    /// Pipeline: auto-dispatch to these workers on task completion
    hands_off_to: Vec<String>,
    /// All teammates (name + role) for prompt injection
    teammates: Vec<(String, String)>,
}

impl WorkerHarness {
    pub fn new(
        client: CollabClient,
        instance_id: String,
        workdir: PathBuf,
        model: String,
        cli_template: Option<String>,
        cli_template_light: Option<String>,
        auto_reply: bool,
        batch_wait_ms: u64,
        hands_off_to: Vec<String>,
        teammates: Vec<(String, String)>,
    ) -> Self {
        Self {
            client: Arc::new(client),
            instance_id,
            workdir,
            model,
            cli_template: cli_template.unwrap_or_else(|| DEFAULT_CLI_TEMPLATE.to_string()),
            cli_template_light,
            auto_reply,
            batch_wait_ms,
            message_queue: Arc::new(Mutex::new(Vec::new())),
            first_message_time: Arc::new(Mutex::new(None)),
            hands_off_to,
            teammates,
        }
    }

    /// Classify how much context a set of messages needs
    async fn classify_tier(&self, messages: &[Message]) -> PromptTier {
        // Ping/status checks → harness handles directly
        let ping_re = Regex::new(PING_PATTERN).unwrap();
        if messages.iter().all(|m| ping_re.is_match(m.content.trim())) {
            return PromptTier::Harness;
        }

        // Ack loop detection — swallow pure acknowledgments from other workers.
        // These are messages that start with ack-like phrases and carry no new information.
        let ack_re = Regex::new(ACK_START_PATTERN).unwrap();
        let non_self_msgs: Vec<_> = messages.iter().filter(|m| m.sender != self.instance_id).collect();
        if !non_self_msgs.is_empty() && non_self_msgs.iter().all(|m| ack_re.is_match(m.content.trim())) {
            // All external messages are acks — swallow them
            return PromptTier::Harness;
        }

        // Self-messages always need full context — the worker must see its todo list
        // to act on tasks. Light tier omits todos, making these calls useless.
        if messages.iter().any(|m| m.sender == self.instance_id) {
            return PromptTier::Full;
        }

        // Multiple messages batched → full context
        if messages.len() > 1 {
            return PromptTier::Full;
        }

        // Single short external message with no todos → light
        if let Some(msg) = messages.first() {
            if msg.content.len() < 200 {
                return PromptTier::Light;
            }
        }

        PromptTier::Full
    }

    /// Handle harness-tier messages without spawning CLI.
    /// Pings get a status reply; acks get swallowed silently to break ack loops.
    async fn handle_harness_tier(&self, messages: &[Message]) -> Result<()> {
        let ping_re = Regex::new(PING_PATTERN).unwrap();
        let is_ping = messages.iter().all(|m| ping_re.is_match(m.content.trim()));

        if is_ping {
            // Respond to pings with current status
            let state = self.load_state();
            let status = state.status.as_deref().unwrap_or("idle");
            let files_count = state.files_touched.len();
            let pending = state.pending.as_deref().unwrap_or("none");

            let reply = format!(
                "Online. Status: {}. Files touched: {}. Pending: {}",
                status, files_count, pending
            );

            let mut replied = std::collections::HashSet::new();
            for msg in messages {
                if msg.sender != self.instance_id && replied.insert(msg.sender.clone()) {
                    if let Err(e) = self.client.add_message(&msg.sender, &reply, None).await {
                        self.log_error(&format!("Failed to reply to @{}: {}", msg.sender, e));
                    }
                }
            }
            self.log(&format!("harness-handled ping → {}", status));
        } else {
            // Ack messages — swallow silently to break ack loops
            let senders: Vec<_> = messages.iter()
                .filter(|m| m.sender != self.instance_id)
                .map(|m| format!("@{}", m.sender))
                .collect::<std::collections::HashSet<_>>()
                .into_iter().collect();
            self.log(&format!("swallowed {} ack(s) from {} — no CLI spawn",
                messages.len(), senders.join(", ")));
        }
        Ok(())
    }

    pub async fn run(&self) -> Result<()> {
        // Shared status string for dynamic roster presence
        let current_status = Arc::new(Mutex::new(self.get_role()));

        // Spawn batch processor task that wakes on timer
        let queue = self.message_queue.clone();
        let first_time = self.first_message_time.clone();
        let batch_wait_ms = self.batch_wait_ms;
        let client = self.client.clone();
        let instance_id = self.instance_id.clone();
        let workdir = self.workdir.clone();
        let model = self.model.clone();
        let cli_template = self.cli_template.clone();
        let cli_template_light = self.cli_template_light.clone();
        let auto_reply = self.auto_reply;
        let hands_off_to = self.hands_off_to.clone();
        let teammates = self.teammates.clone();
        let batch_status = current_status.clone();

        let max_self_kicks: u32 = 3;

        // Serializes CLI invocations — only one claude process at a time per worker,
        // but the batch loop itself is never blocked waiting for claude to finish.
        let cli_lock: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));

        // Watchdog: restart batch processor if it ever panics or exits unexpectedly
        let watchdog_queue = queue.clone();
        let watchdog_first_time = first_time.clone();
        let watchdog_client = client.clone();
        let watchdog_instance_id = instance_id.clone();
        let watchdog_workdir = workdir.clone();
        let watchdog_model = model.clone();
        let watchdog_cli_template = cli_template.clone();
        let watchdog_cli_template_light = cli_template_light.clone();
        let watchdog_hands_off_to = hands_off_to.clone();
        let watchdog_teammates = teammates.clone();
        let watchdog_batch_status = current_status.clone();
        let watchdog_cli_lock = cli_lock.clone();

        tokio::spawn(async move {
            loop {
                let handle = {
                    let queue = watchdog_queue.clone();
                    let first_time = watchdog_first_time.clone();
                    let client = watchdog_client.clone();
                    let instance_id = watchdog_instance_id.clone();
                    let workdir = watchdog_workdir.clone();
                    let model = watchdog_model.clone();
                    let cli_template = watchdog_cli_template.clone();
                    let cli_template_light = watchdog_cli_template_light.clone();
                    let hands_off_to = watchdog_hands_off_to.clone();
                    let teammates = watchdog_teammates.clone();
                    let batch_status = watchdog_batch_status.clone();
                    let cli_lock = watchdog_cli_lock.clone();

                    tokio::spawn(async move {
                        let mut consecutive_kicks: u32 = 0;
                        loop {
                sleep(Duration::from_millis(batch_wait_ms)).await;

                // Check if queue has messages and batch window has passed
                let should_process = {
                    let q = queue.lock().await;
                    if q.is_empty() {
                        false
                    } else if let Some(first) = *first_time.lock().await {
                        first.elapsed() >= Duration::from_millis(batch_wait_ms)
                    } else {
                        false
                    }
                };

                if !should_process {
                    continue;
                }

                let mut messages = {
                    let mut q = queue.lock().await;
                    std::mem::take(&mut *q)
                };
                *first_time.lock().await = None;

                // Always strip self-messages before building prompt — never feed them as input
                messages.retain(|m| m.sender != instance_id);

                let has_external = !messages.is_empty();
                let is_self_kick = !has_external;
                if is_self_kick {
                    consecutive_kicks += 1;
                    if consecutive_kicks > max_self_kicks {
                        eprintln!("[{}] self-kick cap reached ({}) — pausing until external message",
                            Utc::now().format("%H:%M:%S UTC"), max_self_kicks);
                        consecutive_kicks = 0;
                        continue;
                    }
                } else {
                    consecutive_kicks = 0;
                }

                let harness = WorkerHarness {
                    client: client.clone(),
                    instance_id: instance_id.clone(),
                    workdir: workdir.clone(),
                    model: model.clone(),
                    cli_template: cli_template.clone(),
                    cli_template_light: cli_template_light.clone(),
                    auto_reply,
                    batch_wait_ms,
                    message_queue: Arc::new(Mutex::new(Vec::new())),
                    first_message_time: Arc::new(Mutex::new(None)),
                    hands_off_to: hands_off_to.clone(),
                    teammates: teammates.clone(),
                };

                let tier = harness.classify_tier(&messages).await;

                match tier {
                    PromptTier::Harness => {
                        // Harness-tier is instant — handle inline, no lock needed
                        if let Err(e) = harness.handle_harness_tier(&messages).await {
                            harness.log_error(&format!("Harness tier failed: {}", e));
                        }
                    }
                    _ => {
                        // Spawn CLI in a background task so the batch loop stays unblocked.
                        // cli_lock serializes invocations — only one claude process at a time.
                        let cli_lock = cli_lock.clone();
                        let batch_status = batch_status.clone();
                        let queue = queue.clone();
                        let full_context = tier == PromptTier::Full;
                        let instance_id_for_log = instance_id.clone();

                        let handle = tokio::spawn(async move {
                            let _guard = cli_lock.lock().await;

                            let worker_continued = match harness.spawn_cli(&messages, full_context).await {
                                Ok(c) => c,
                                Err(e) => {
                                    harness.log_error(&format!("Failed to process {} messages: {}", messages.len(), e));
                                    false
                                }
                            };

                            // Update roster presence from worker state
                            let state = harness.load_state();
                            if let Some(status) = &state.status {
                                *batch_status.lock().await = status.clone();
                            }

                            // Auto-kick if worker has pending todos but didn't self-continue.
                            // Skip if this was an auto-kick (avoid kick→kick→kick loops).
                            let was_auto_kick = is_self_kick && messages.iter().any(|m| m.content.contains("pending tasks"));
                            if !worker_continued && !was_auto_kick {
                                if let Ok(todos) = harness.client.fetch_todos(&harness.instance_id).await {
                                    if !todos.is_empty() {
                                        let q = queue.lock().await;
                                        if q.is_empty() {
                                            drop(q);
                                            let _ = harness.client.add_message(
                                                &harness.instance_id,
                                                &format!("You have {} pending tasks — pick up the next one when ready.", todos.len()),
                                                None
                                            ).await;
                                        }
                                    }
                                }
                            }
                        });

                        // Monitor for panics — log them so they're not silently swallowed
                        tokio::spawn(async move {
                            if let Err(e) = handle.await {
                                if e.is_panic() {
                                    eprintln!("[{}] [{}] CLI task panicked — cli_lock released, batch loop continues",
                                        Utc::now().format("%H:%M:%S UTC"), instance_id_for_log);
                                }
                            }
                        });
                    }
                }
                        } // end loop
                    }) // end inner batch loop task
                }; // end handle assignment

                if let Err(e) = handle.await {
                    if e.is_panic() {
                        eprintln!("[{}] [{}] Batch processor panicked — restarting in 1s",
                            Utc::now().format("%H:%M:%S UTC"), watchdog_instance_id);
                        sleep(Duration::from_secs(1)).await;
                    }
                }
                // If handle returned Ok(()), the loop exited normally — restart immediately
            }
        }); // end watchdog

        // Heartbeat presence every 30s — role updates dynamically from worker state
        let hb_client = self.client.clone();
        let hb_status = current_status.clone();
        let hb_workdir = self.workdir.clone();
        tokio::spawn(async move {
            loop {
                // Load role from AGENT.md/CLAUDE.md dynamically on each heartbeat
                let mut role = hb_status.lock().await.clone();
                for filename in &["AGENT.md", "CLAUDE.md"] {
                    let path = hb_workdir.join(filename);
                    if let Ok(contents) = std::fs::read_to_string(&path) {
                        for line in contents.lines() {
                            // Look for "Your role" (with or without colon, accounting for markdown)
                            if let Some(pos) = line.find("Your role") {
                                // Extract everything after "Your role" and any following punctuation/formatting
                                let after_marker = &line[pos + "Your role".len()..];
                                // Strip leading markdown (*), colons, and whitespace
                                let cleaned = after_marker
                                    .trim_start_matches(|c: char| c == '*' || c == ':' || c.is_whitespace())
                                    .trim_end_matches('*')
                                    .to_string();
                                if !cleaned.is_empty() {
                                    role = cleaned;
                                    break;
                                }
                            }
                        }
                        if !role.is_empty() && role != "Worker" {
                            break;
                        }
                    }
                }
                let _ = hb_client.heartbeat(Some(&role)).await;
                sleep(Duration::from_secs(30)).await;
            }
        });

        let mut booted = false;
        let mut backoff_secs = 1u64;

        loop {
            let url = format!("{}/events/{}", self.client.base_url, self.instance_id);
            let mut req = self.client.client.get(&url).header("Accept", "text/event-stream");

            if let Some(token) = &self.client.token {
                req = req.header("Authorization", format!("Bearer {}", token));
            }

            match req.send().await {
                Ok(response) if response.status().is_success() => {
                    backoff_secs = 1;
                    self.log(&format!("idle — listening for @{}", self.instance_id));

                    // On connect, fetch any messages that arrived while offline and queue them
                    match self.client.fetch_pending_messages().await {
                        Ok(pending) => {
                            let mut queue = self.message_queue.lock().await;
                            for msg in pending {
                                // Skip self-messages — they're noise
                                if msg.sender != self.instance_id {
                                    queue.push(Message {
                                        sender: msg.sender,
                                        content: msg.content,
                                        hash: msg.hash,
                                        timestamp: msg.timestamp,
                                        recipient: msg.recipient,
                                    });
                                }
                            }
                            if !queue.is_empty() {
                                *self.first_message_time.lock().await = Some(Instant::now());
                                self.log(&format!("queued {} offline message(s)", queue.len()));
                            }
                        }
                        Err(e) => self.log_error(&format!("Failed to fetch pending messages: {}", e)),
                    }

                    // Auto-kick: queue boot message directly (only once).
                    // We used to post this as a self-message through the server,
                    // but the queue processor strips all self-messages (line 308),
                    // so the boot kick was silently discarded — workers sat idle
                    // until an external message arrived.
                    if !booted {
                        booted = true;
                        let boot_msg = Message {
                            sender: "system".to_string(),
                            recipient: self.instance_id.clone(),
                            content: "Session start — welcome back. Check your pending tasks and pick up where you left off. Set continue:true to keep working through your task list, or continue:false when you're blocked or done.".to_string(),
                            hash: String::new(),
                            timestamp: chrono::Utc::now(),
                        };
                        let mut queue = self.message_queue.lock().await;
                        queue.push(boot_msg);
                        *self.first_message_time.lock().await = Some(Instant::now());
                        self.log("queued boot message");
                    }

                    let mut buffer = String::new();
                    let mut response = response;

                    loop {
                        match response.chunk().await {
                            Ok(Some(chunk)) => {
                                buffer.push_str(&String::from_utf8_lossy(&chunk));
                                while let Some(end) = buffer.find("\n\n") {
                                    let event_str = buffer[..end].to_string();
                                    buffer.drain(..end + 2);

                                    for line in event_str.lines() {
                                        if let Some(data) = line.strip_prefix("data: ") {
                                            if let Ok(msg) = serde_json::from_str::<Message>(data) {
                                                // Pings get answered immediately — never block on queue
                                                let ping_re = Regex::new(PING_PATTERN).unwrap();
                                                if ping_re.is_match(msg.content.trim()) {
                                                    let _ = self.handle_harness_tier(&[msg]).await;
                                                } else {
                                                    // Queue the message
                                                    let mut queue = self.message_queue.lock().await;
                                                    queue.push(msg);

                                                    // Record first message time for batching
                                                    if queue.len() == 1 {
                                                        *self.first_message_time.lock().await = Some(Instant::now());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(None) => {
                                self.log(&format!("connection closed, reconnecting in {}s", backoff_secs));
                                break;
                            }
                            Err(e) => {
                                self.log(&format!("stream error: {} — reconnecting in {}s", e, backoff_secs));
                                break;
                            }
                        }
                    }
                }
                Ok(response) => {
                    self.log(&format!("server error: {} — reconnecting in {}s", response.status(), backoff_secs));
                }
                Err(e) => {
                    self.log(&format!("connection error: {} — reconnecting in {}s", e, backoff_secs));
                }
            }

            sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);
        }
    }

    fn is_trivial_reply(&self, content: &str) -> bool {
        Regex::new(TRIVIAL_REPLY_PATTERN)
            .map(|re| re.is_match(content.trim()))
            .unwrap_or(false)
    }

    /// Build the prompt for a CLI invocation.
    /// `full_context`: true = full prompt (teammates, state, todos, full schema), false = light prompt
    async fn build_prompt(&self, messages: &[Message], full_context: bool) -> Result<String> {
        // Format message lines (shared by both tiers)
        let mut msg_lines = String::new();
        for msg in messages {
            let body = if msg.content.len() > 2000 {
                let hash_short = &msg.hash[..7.min(msg.hash.len())];
                let tmp_path = format!("/tmp/collab-msg-{}.md", hash_short);
                let _ = std::fs::write(&tmp_path, &msg.content);
                format!("(see file: {})", tmp_path)
            } else {
                msg.content.clone()
            };
            msg_lines.push_str(&format!("@{}: {}\n", msg.sender, body));
        }

        if !full_context {
            // Light prompt — minimal context
            return Ok(format!(
                "You are @{}. Role: {}

Messages ({}):
{}

Act on the messages above. Use Bash/Read/Write/Edit to do your actual work.

When done, your FINAL output must be ONLY a JSON object (no other text before or after):

{{
  \"response\": \"your reply to the sender (string or null)\",
  \"delegate\": [],
  \"messages\": null,
  \"completed_tasks\": [],
  \"continue\": false,
  \"state_update\": {{\"status\": \"what you're doing now\"}}
}}

Do NOT use `collab send`, `collab todo add`, or `collab broadcast` — output those via JSON instead. Read commands (`collab status`, `collab todo list`) are fine. Focus on your actual work.",
                self.instance_id,
                self.get_role(),
                messages.len(),
                msg_lines
            ));
        }

        // Full prompt — complete context
        let state = self.load_state();
        let state_str = serde_json::to_string_pretty(&state).unwrap_or_else(|_| "No previous state.".to_string());

        let todos_str = match self.client.fetch_todos(&self.instance_id).await {
            Ok(todos) if !todos.is_empty() => {
                let mut lines = String::from("Pending tasks assigned to you:\n");
                for todo in &todos {
                    lines.push_str(&format!("  - [{}] (from @{}): {}\n",
                        &todo.hash[..7.min(todo.hash.len())],
                        todo.assigned_by,
                        todo.description
                    ));
                }
                lines
            }
            _ => "No pending tasks.".to_string(),
        };

        let teammates_str = if self.teammates.is_empty() {
            "No teammates configured.".to_string()
        } else {
            let mut lines = String::from("Your team:\n");
            for (name, role) in &self.teammates {
                if name != &self.instance_id {
                    lines.push_str(&format!("  @{} — {}\n", name, role));
                }
            }
            if !self.hands_off_to.is_empty() {
                lines.push_str(&format!("\nWhen you complete a task, your work auto-routes to: {}\n",
                    self.hands_off_to.iter().map(|w| format!("@{}", w)).collect::<Vec<_>>().join(", ")));
            }
            lines
        };

        // Fetch recent message history for conversational context
        let current_hashes: std::collections::HashSet<_> = messages.iter().map(|m| m.hash.as_str()).collect();
        let history_str = match self.client.fetch_history_pub(&self.instance_id).await {
            Ok(history) => {
                let recent: Vec<_> = history.iter()
                    .filter(|m| !current_hashes.contains(m.hash.as_str()))
                    .rev()
                    .take(20)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                if recent.is_empty() {
                    String::new()
                } else {
                    let mut lines = String::from("Recent conversation history (for context — do NOT re-process these, only act on the new messages below):\n");
                    for m in &recent {
                        let content = if m.content.len() > 300 { format!("{}…", &m.content[..300]) } else { m.content.clone() };
                        lines.push_str(&format!("  @{} → @{}: {}\n", m.sender, m.recipient, content));
                    }
                    lines
                }
            }
            Err(_) => String::new(),
        };

        Ok(format!(
            "You are @{}. Role: {}

{}

Previous state:
{}

{}

{}
New messages ({}):
{}

Act on the new messages above. Use Bash/Read/Write/Edit to do your actual work (coding, research, testing).

When done, your FINAL output must be ONLY a JSON object (no other text before or after):

{{
  \"response\": \"your reply to the sender (string or null)\",
  \"delegate\": [{{\"to\": \"@worker\", \"task\": \"description\"}}],
  \"messages\": [{{\"to\": \"@worker\", \"text\": \"message\"}}],
  \"completed_tasks\": [\"hash1\", \"hash2\"],
  \"continue\": false,
  \"state_update\": {{\"key\": \"value\"}}
}}

Fields:
- response: reply back to whoever messaged you
- delegate: assign tasks to ANY instance (teammates, humans, anyone) — creates a persistent todo and pings them. If someone messages you asking for something and you need THEM to act, delegate back to THEM — not to a random teammate. IMPORTANT: do NOT put task assignments in response or messages, those are ephemeral and will be lost on context reset. The task description MUST be self-contained: include all facts, URLs, decisions, and context the recipient needs — they will NOT see the original messages that led to this task
- messages: send messages to any teammate directly — for status updates and context only, NOT for assigning work (optional)
- completed_tasks: task hashes you finished — marks done and routes to downstream workers (optional)
- continue: true to keep working autonomously, false when blocked or done
- state_update: persist state for next invocation. Include \"status\" to update your roster presence

Do NOT use `collab send`, `collab todo add`, or `collab broadcast` — the harness delivers those from your JSON output. Read commands (`collab status`, `collab todo list`) are fine if you need to verify state. Focus on your actual work.",
            self.instance_id,
            self.get_role(),
            teammates_str,
            state_str,
            todos_str,
            history_str,
            messages.len(),
            msg_lines
        ))
    }

    /// Returns Ok(true) if the worker set continue: true, Ok(false) otherwise.
    async fn spawn_cli(&self, messages: &[Message], full_context: bool) -> Result<bool> {
        let start = std::time::Instant::now();
        let tier = if full_context { PromptTier::Full } else { PromptTier::Light };

        let prompt = self.build_prompt(messages, full_context).await?;

        // Select template: light tier uses cli_template_light if available
        let active_template = if !full_context {
            self.cli_template_light.as_deref().unwrap_or(&self.cli_template)
        } else {
            &self.cli_template
        };

        // Validate: error if template uses {model} but no model is set
        if active_template.contains("{model}") && self.model.is_empty() {
            return Err(anyhow::anyhow!(
                "cli_template uses {{model}} but no model is configured.\n\
                 Set 'model' in workers.yaml or pass --model to collab worker."
            ));
        }

        // Validate: catch unconfigured placeholder from collab init
        if active_template.contains("{agent}") {
            return Err(anyhow::anyhow!(
                "cli_template still contains {{agent}} placeholder — you need to configure it.\n\
                 Edit .collab/workers.json or workers.yaml and replace {{agent}} with your CLI tool.\n\
                 Examples:\n\
                 \x20 claude -p {{prompt}} --model {{model}} --allowedTools Bash,Read,Write,Edit\n\
                 \x20 cursor -p {{prompt}} --model {{model}}\n\
                 \x20 ollama run {{model}} {{prompt}}"
            ));
        }

        // Shell-split the template BEFORE substitution so {prompt} stays as one arg
        let template_parts = shlex::split(active_template).ok_or_else(|| {
            anyhow::anyhow!("Invalid cli_template (bad quoting): {}", active_template)
        })?;
        if template_parts.is_empty() {
            return Err(anyhow::anyhow!("cli_template expanded to empty command"));
        }

        let workdir_str = self.workdir.to_string_lossy();
        let mut parts: Vec<String> = template_parts.iter().map(|part| {
            part.replace("{prompt}", &prompt)
                .replace("{model}", &self.model)
                .replace("{workdir}", &workdir_str)
        }).collect();

        // Detect claude CLI — inject --output-format json to get real token counts and cost
        let is_claude_cli = parts[0].ends_with("claude");
        if is_claude_cli {
            parts.push("--output-format".to_string());
            parts.push("json".to_string());
        }

        let mut cmd = Command::new(&parts[0]);
        cmd.args(&parts[1..])
            .current_dir(&self.workdir);

        // Remove collab env vars from subprocess — harness handles all communication
        cmd.env_remove("COLLAB_INSTANCE");
        cmd.env_remove("COLLAB_SERVER");
        cmd.env_remove("COLLAB_TOKEN");

        let output = match cmd.output()
        {
            Ok(out) => out,
            Err(e) => {
                self.log_error(&format!("Failed to spawn '{}': {}", parts[0], e));
                return Err(e.into());
            }
        };

        // Debug: always dump claude output on failure
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let debug_path = format!("/tmp/collab-debug-{}.txt", self.instance_id);
            let _ = std::fs::write(&debug_path, format!(
                "EXIT: {}\nSTDOUT ({} bytes):\n{}\nSTDERR ({} bytes):\n{}\nPROMPT:\n{}",
                output.status, stdout.len(), stdout, stderr.len(), stderr, prompt
            ));
            let detail = if stderr.trim().is_empty() && stdout.trim().is_empty() {
                format!("(empty output — see {})", debug_path)
            } else if stderr.trim().is_empty() {
                stdout.to_string()
            } else {
                stderr.to_string()
            };
            self.log_error(&format!("CLI exited with status {}: {}", output.status, detail));
            return Err(anyhow::anyhow!("CLI failed: {}", detail));
        }

        let raw_stdout = String::from_utf8_lossy(&output.stdout);

        // For claude CLI: unwrap --output-format json envelope to get real token counts and cost
        let (stdout, real_input_tokens, real_output_tokens, cost_usd) = if is_claude_cli {
            match serde_json::from_str::<serde_json::Value>(&raw_stdout) {
                Ok(v) => {
                    let inner = v.get("result")
                        .and_then(|r| r.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input_tok = v.pointer("/usage/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0)
                        + v.pointer("/usage/cache_creation_input_tokens").and_then(|t| t.as_u64()).unwrap_or(0)
                        + v.pointer("/usage/cache_read_input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    let output_tok = v.pointer("/usage/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    let cost = v.get("total_cost_usd").and_then(|c| c.as_f64());
                    (std::borrow::Cow::Owned(inner), input_tok, output_tok, cost)
                }
                Err(e) => {
                    self.log_error(&format!("Failed to parse claude JSON envelope: {e}"));
                    (raw_stdout, 0u64, 0u64, None)
                }
            }
        } else {
            (raw_stdout, 0u64, 0u64, None)
        };

        let duration = start.elapsed().as_secs();

        // Parse structured output
        let mut did_continue = false;
        if let Some(collab_output) = self.parse_collab_output(&stdout) {
            // Build set of delegate targets to avoid duplicate messages
            let delegated_to: std::collections::HashSet<String> = collab_output.delegate.iter()
                .map(|t| t.to.trim_start_matches('@').to_string())
                .collect();

            // Send response once per unique sender (skip self, system, and delegate targets)
            let mut replied: std::collections::HashSet<String> = std::collections::HashSet::new();
            if let Some(response) = &collab_output.response {
                if !response.is_empty() {
                    for msg in messages {
                        if msg.sender != self.instance_id
                            && msg.sender != "system"
                            && !delegated_to.contains(&msg.sender)
                            && replied.insert(msg.sender.clone())
                        {
                            if let Err(e) = self.client.add_message(&msg.sender, response, None).await {
                                self.log_error(&format!("Failed to send response to @{}: {}", msg.sender, e));
                            }
                        }
                    }
                }
            }

            // Delegate tasks — create todo (todo_add already sends a ping message)
            for task in &collab_output.delegate {
                let to = task.to.trim_start_matches('@');
                if let Err(e) = self.client.todo_add(to, &task.task).await {
                    self.log_error(&format!("Failed to add todo for @{}: {}", to, e));
                }
            }

            // Send direct messages to specific teammates. Skip recipients that
            // already received a `response` or a `delegate` in this turn — the
            // model often duplicates the same content across fields, especially
            // on cheaper tiers that ignore the "messages must be null" rule.
            let mut messaged: std::collections::HashSet<String> = std::collections::HashSet::new();
            for dm in &collab_output.messages {
                let to = dm.to.trim_start_matches('@').to_string();
                if delegated_to.contains(&to) {
                    self.log(&format!("skipped duplicate message to @{} (already delegated)", to));
                    continue;
                }
                if replied.contains(&to) {
                    self.log(&format!("skipped duplicate message to @{} (already in response)", to));
                    continue;
                }
                if !messaged.insert(to.clone()) {
                    self.log(&format!("skipped duplicate message to @{} (already messaged this turn)", to));
                    continue;
                }
                if let Err(e) = self.client.add_message(&to, &dm.text, None).await {
                    self.log_error(&format!("Failed to message @{}: {}", to, e));
                }
            }

            // Mark completed tasks and auto-route to downstream workers
            // Cap at 5 per call — process first 5, warn about any beyond that
            let max_completions = 5;
            if collab_output.completed_tasks.len() > max_completions {
                self.log_error(&format!(
                    "Worker tried to mark {} tasks done in one call (cap: {}) — processing first {}, ignoring rest",
                    collab_output.completed_tasks.len(), max_completions, max_completions
                ));
            }
            let mut completed_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for hash in collab_output.completed_tasks.iter().take(max_completions) {
                let hash_clean = hash.trim();
                if hash_clean.is_empty() { continue; }
                if !completed_seen.insert(hash_clean.to_string()) {
                    self.log(&format!("skipped duplicate completion for {}", hash_clean));
                    continue;
                }
                match self.client.todo_done(hash_clean).await {
                    Ok(_) => self.log(&format!("task {} marked done", hash_clean)),
                    Err(e) => self.log_error(&format!("Failed to mark task {} done: {}", hash_clean, e)),
                }
            }

            // Pipeline: auto-dispatch to downstream workers. Skip any that
            // already received a message this turn via response/delegate/messages.
            if !collab_output.completed_tasks.is_empty() && !self.hands_off_to.is_empty() {
                let summary = collab_output.response.as_deref().unwrap_or("Task completed.");
                let handoff_msg = format!("Completed work from @{}: {}", self.instance_id, summary);
                for downstream in &self.hands_off_to {
                    let to = downstream.trim_start_matches('@').to_string();
                    if replied.contains(&to)
                        || delegated_to.contains(&to)
                        || messaged.contains(&to)
                    {
                        self.log(&format!("skipped pipeline → @{} (already contacted this turn)", to));
                        continue;
                    }
                    if let Err(e) = self.client.add_message(&to, &handoff_msg, None).await {
                        self.log_error(&format!("Failed to route to @{}: {}", to, e));
                    } else {
                        self.log(&format!("pipeline → @{}", to));
                    }
                }
            }

            // Self-kick: worker wants to keep going
            did_continue = collab_output.r#continue;
            if collab_output.r#continue {
                let kick_msg = collab_output.response.as_deref().unwrap_or("Continuing...");
                let self_msg = format!("(self-continue) Previous output: {}", kick_msg);
                if let Err(e) = self.client.add_message(&self.instance_id, &self_msg, None).await {
                    self.log_error(&format!("Failed to self-kick: {}", e));
                } else {
                    self.log("continuing → self-kick");
                }
            }

            // Update state
            self.save_state(&collab_output.state_update);
        } else {
            // Fallback: no markers found
            let raw = stdout.trim().to_string();
            if !raw.is_empty() {
                // If it looks like a failed JSON parse (contains "response" key), don't send raw JSON
                if raw.contains("\"response\"") && raw.contains("{") {
                    self.log_error(&format!("JSON parse failed — output looks like structured JSON but couldn't be parsed. Not sending raw JSON to team."));
                } else {
                    // Plain text response — send it
                    self.log(&format!("no markers — sending raw response"));
                    for msg in messages {
                        if msg.sender != self.instance_id && msg.sender != "system" {
                            if let Err(e) = self.client.add_message(&msg.sender, &raw, None).await {
                                self.log_error(&format!("Failed to send response to @{}: {}", msg.sender, e));
                            }
                        }
                    }
                }
            }
        }

        // Token usage — real counts from claude JSON envelope, estimates for other CLIs
        let (log_input_tokens, log_output_tokens) = if is_claude_cli {
            (real_input_tokens, real_output_tokens)
        } else {
            (prompt.len() as u64 / 4, stdout.len() as u64 / 4)
        };
        let cost_str = cost_usd.map(|c| format!(", ${:.4}", c)).unwrap_or_default();
        self.log(&format!("done — {}s, {}+{} tokens{}", duration, log_input_tokens, log_output_tokens, cost_str));

        // Append to usage log
        let log_line = format!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            self.instance_id,
            duration,
            log_input_tokens,
            log_output_tokens,
            self.cli_template.split_whitespace().next().unwrap_or("unknown"),
            tier,
            cost_usd.map(|c| format!("{:.6}", c)).unwrap_or_default()
        );
        match crate::find_collab_dir_from(&self.workdir) {
            Some(collab_dir) => {
                let log_path = collab_dir.join("usage.log");
                if let Err(e) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .and_then(|mut f| std::io::Write::write_all(&mut f, log_line.as_bytes()))
                {
                    self.log(&format!("warn: failed to append usage.log at {}: {}", log_path.display(), e));
                }
            }
            None => {
                self.log(&format!(
                    "warn: no .collab/workers.json found walking up from {} — usage not recorded",
                    self.workdir.display()
                ));
            }
        }

        // Clean up temp files from this invocation
        for msg in messages {
            if msg.content.len() > 2000 {
                let hash_short = &msg.hash[..7.min(msg.hash.len())];
                let tmp_path = format!("/tmp/collab-msg-{}.md", hash_short);
                let _ = std::fs::remove_file(&tmp_path);
            }
        }
        // Remove debug dump from previous failure (if this call succeeded)
        let debug_path = format!("/tmp/collab-debug-{}.txt", self.instance_id);
        let _ = std::fs::remove_file(&debug_path);

        Ok(did_continue)
    }

    fn parse_collab_output(&self, output: &str) -> Option<CollabOutput> {
        parse_collab_output(output)
    }

    fn load_state(&self) -> WorkerState {
        let path = self.workdir.join(".worker-state.json");
        if let Ok(contents) = std::fs::read_to_string(&path) {
            serde_json::from_str(&contents).unwrap_or_default()
        } else {
            WorkerState::default()
        }
    }

    fn save_state(&self, state: &WorkerState) {
        let path = self.workdir.join(".worker-state.json");
        if let Ok(json) = serde_json::to_string_pretty(state) {
            let _ = std::fs::write(&path, json);
        }
    }

    fn get_role(&self) -> String {
        // Try AGENT.md first, fall back to CLAUDE.md for existing setups
        for filename in &["AGENT.md", "CLAUDE.md"] {
            let path = self.workdir.join(filename);
            if let Ok(contents) = std::fs::read_to_string(&path) {
                for line in contents.lines() {
                    if line.contains("Your role:") {
                        if let Some(rest) = line.split("Your role:").nth(1) {
                            return rest.trim().trim_end_matches('*').to_string();
                        }
                    }
                }
            }
        }
        "Worker".to_string()
    }

    fn log(&self, msg: &str) {
        let now = Utc::now().format("%H:%M:%S UTC");
        println!("[{}] {}", now, msg);
    }

    fn log_error(&self, msg: &str) {
        let now = Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        let log_entry = format!("[{}] @{}: {}\n", now, self.instance_id, msg);

        // Append to error log file
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/collab-worker-errors.log")
        {
            let _ = file.write_all(log_entry.as_bytes());
        }

        // Also print to stderr
        eprintln!("{}", log_entry);
    }
}

fn parse_collab_output(output: &str) -> Option<CollabOutput> {
    // Strip markdown code fences if present
    let cleaned = if output.contains("```") {
        let mut result = String::new();
        let mut in_fence = false;
        for line in output.lines() {
            if line.trim().starts_with("```") {
                in_fence = !in_fence;
                if !in_fence { continue; } // closing fence
                continue; // opening fence (```json etc)
            }
            if in_fence {
                result.push_str(line);
                result.push('\n');
            }
        }
        if result.trim().is_empty() { output.to_string() } else { result }
    } else {
        output.to_string()
    };

    // Try to find valid CollabOutput JSON — scan from the end backwards
    let bytes = cleaned.as_bytes();
    let mut depth = 0i32;
    let mut end_pos = None;

    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'}' {
            if depth == 0 { end_pos = Some(i); }
            depth += 1;
        } else if bytes[i] == b'{' {
            depth -= 1;
            if depth == 0 {
                if let Some(end) = end_pos {
                    let json_str = &cleaned[i..=end];
                    if let Ok(parsed) = serde_json::from_str::<CollabOutput>(json_str) {
                        return Some(parsed);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_handles_null_fields() {
        let input = r#"{"response": "hi", "delegate": null, "messages": null, "completed_tasks": null, "continue": false, "state_update": {}}"#;
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.response.as_deref(), Some("hi"));
        assert!(result.delegate.is_empty());
        assert!(result.messages.is_empty());
        assert!(result.completed_tasks.is_empty());
        assert!(!result.r#continue);
    }

    #[test]
    fn parse_handles_missing_fields() {
        let input = r#"{"response": "hi"}"#;
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.response.as_deref(), Some("hi"));
        assert!(result.delegate.is_empty());
        assert!(result.messages.is_empty());
        assert!(result.completed_tasks.is_empty());
    }

    #[test]
    fn parse_handles_markdown_fences() {
        let input = "Here is the output:\n\n```json\n{\"response\": \"done\", \"continue\": false}\n```\n";
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.response.as_deref(), Some("done"));
    }

    #[test]
    fn parse_handles_text_before_json() {
        let input = "Let me check...\n\n{\"response\": \"found it\", \"continue\": false}";
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.response.as_deref(), Some("found it"));
    }

    #[test]
    fn parse_handles_text_after_json() {
        let input = "{\"response\": \"all good\", \"continue\": false}\n\nHope that helps!";
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.response.as_deref(), Some("all good"));
    }

    #[test]
    fn parse_handles_nested_json_in_state() {
        let input = r#"{"response": "ok", "state_update": {"status": "working", "files_touched": ["a.rs", "b.rs"]}, "continue": false}"#;
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.response.as_deref(), Some("ok"));
        assert_eq!(result.state_update.status.as_deref(), Some("working"));
        assert_eq!(result.state_update.files_touched, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn parse_handles_empty_string() {
        assert!(parse_collab_output("").is_none());
    }

    #[test]
    fn parse_handles_no_json() {
        assert!(parse_collab_output("Just some plain text response").is_none());
    }

    #[test]
    fn parse_handles_invalid_json() {
        assert!(parse_collab_output("{response: broken}").is_none());
    }

    #[test]
    fn parse_handles_continue_true() {
        let input = r#"{"response": null, "continue": true}"#;
        let result = parse_collab_output(input).expect("should parse");
        assert!(result.response.is_none());
        assert!(result.r#continue);
    }

    #[test]
    fn parse_handles_messages_field() {
        let input = r#"{"response": "sent", "messages": [{"to": "@frontend", "text": "API ready"}], "continue": false}"#;
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].to, "@frontend");
        assert_eq!(result.messages[0].text, "API ready");
    }

    #[test]
    fn parse_handles_delegate_field() {
        let input = r#"{"response": "delegated", "delegate": [{"to": "@backend", "task": "fix the bug"}], "continue": false}"#;
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.delegate.len(), 1);
        assert_eq!(result.delegate[0].to, "@backend");
        assert_eq!(result.delegate[0].task, "fix the bug");
    }

    #[test]
    fn parse_handles_completed_tasks() {
        let input = r#"{"response": "done", "completed_tasks": ["abc123", "def456"], "continue": false}"#;
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.completed_tasks, vec!["abc123", "def456"]);
    }

    #[test]
    fn parse_extracts_status_from_state() {
        let input = r#"{"response": "ok", "state_update": {"status": "building UI"}, "continue": false}"#;
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.state_update.status.as_deref(), Some("building UI"));
    }

    #[test]
    fn parse_handles_extra_unknown_fields() {
        let input = r#"{"response": "ok", "unknown_field": 42, "another": "value", "continue": false}"#;
        let result = parse_collab_output(input).expect("should parse");
        assert_eq!(result.response.as_deref(), Some("ok"));
    }

    #[test]
    fn ack_pattern_matches_acknowledged() {
        let re = Regex::new(ACK_START_PATTERN).unwrap();
        assert!(re.is_match("Acknowledged — gate freeze holds"));
        assert!(re.is_match("Ack — freeze unchanged"));
        assert!(re.is_match("Aligned on gate freeze"));
        assert!(re.is_match("Standing by for joint build"));
        assert!(re.is_match("Same gate on my side"));
        assert!(re.is_match("Holding research/dataset churn per gate"));
        assert!(re.is_match("Received — holding Option A"));
        assert!(re.is_match("Noted; unchanged until PM records"));
        assert!(re.is_match("Gate freeze respected — no validator-driven spec churn"));
        assert!(re.is_match("Freeze holds — standing by"));
    }

    #[test]
    fn ack_pattern_matches_with_at_mentions() {
        let re = Regex::new(ACK_START_PATTERN).unwrap();
        assert!(re.is_match("@researcher Acknowledged — holding"));
        assert!(re.is_match("@project-manager @validator Acknowledged freeze"));
        assert!(re.is_match("@database Aligned: holding research churn"));
    }

    #[test]
    fn ack_pattern_does_not_match_real_messages() {
        let re = Regex::new(ACK_START_PATTERN).unwrap();
        assert!(!re.is_match("Fixed the auth redirect issue"));
        assert!(!re.is_match("New dataset ready for integration"));
        assert!(!re.is_match("Found bug in payment processor"));
        assert!(!re.is_match("Please review the schema changes"));
        assert!(!re.is_match("Write access is unblocked on my side"));
        assert!(!re.is_match("Completed work from @builder: API ready"));
    }
}
