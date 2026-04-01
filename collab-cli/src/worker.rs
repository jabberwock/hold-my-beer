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
}

#[derive(Debug, Serialize, Deserialize)]
struct CollabOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(default)]
    pub delegate: Vec<DelegateTask>,
    #[serde(default)]
    pub state_update: WorkerState,
}

#[derive(Debug, Serialize, Deserialize)]
struct DelegateTask {
    pub to: String,
    pub task: String,
}

pub struct WorkerHarness {
    client: Arc<CollabClient>,
    instance_id: String,
    workdir: PathBuf,
    model: String,
    auto_reply: bool,
    batch_wait_ms: u64,
    message_queue: Arc<Mutex<Vec<Message>>>,
    first_message_time: Arc<Mutex<Option<Instant>>>,
}

impl WorkerHarness {
    pub fn new(
        client: CollabClient,
        instance_id: String,
        workdir: PathBuf,
        model: String,
        auto_reply: bool,
        batch_wait_ms: u64,
    ) -> Self {
        Self {
            client: Arc::new(client),
            instance_id,
            workdir,
            model,
            auto_reply,
            batch_wait_ms,
            message_queue: Arc::new(Mutex::new(Vec::new())),
            first_message_time: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn run(&self) -> Result<()> {
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
                                                // Queue the message
                                                {
                                                    let mut queue = self.message_queue.lock().await;
                                                    queue.push(msg);

                                                    // Record first message time for batching
                                                    if queue.len() == 1 {
                                                        *self.first_message_time.lock().await = Some(Instant::now());
                                                    }
                                                }

                                                // Check if we should spawn claude now
                                                if self.should_spawn().await {
                                                    self.handle_messages().await.ok();
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

    async fn should_spawn(&self) -> bool {
        let queue = self.message_queue.lock().await;
        if queue.is_empty() {
            return false;
        }

        // Check if batch_wait has elapsed since first message
        if let Some(first_time) = *self.first_message_time.lock().await {
            first_time.elapsed() >= Duration::from_millis(self.batch_wait_ms)
        } else {
            false
        }
    }

    async fn handle_messages(&self) -> Result<()> {
        let messages = {
            let mut queue = self.message_queue.lock().await;
            let msgs = std::mem::take(&mut *queue);
            *self.first_message_time.lock().await = None;
            msgs
        };

        if messages.is_empty() {
            return Ok(());
        }

        let senders: Vec<String> = messages.iter().map(|m| format!("@{}", m.sender)).collect();
        let sender_str = senders.join(", ");

        self.log(&format!("wake — {} message(s) from {} → spawning claude", messages.len(), sender_str));

        // Check for trivial replies
        for msg in &messages {
            if self.auto_reply && self.is_trivial_reply(&msg.content) {
                self.log(&format!("auto — trivial reply to @{}", msg.sender));
                let _ = self.client.add_message(&msg.sender, &format!("@{} ack", msg.sender), None).await;
                continue;
            }
        }

        // Handle messages via claude
        self.spawn_claude(&messages).await?;

        Ok(())
    }

    fn is_trivial_reply(&self, content: &str) -> bool {
        Regex::new(TRIVIAL_REPLY_PATTERN)
            .map(|re| re.is_match(content.trim()))
            .unwrap_or(false)
    }

    async fn spawn_claude(&self, messages: &[Message]) -> Result<()> {
        let start = std::time::Instant::now();

        // Load previous state
        let state = self.load_state();

        // Build prompt
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

        let state_str = serde_json::to_string_pretty(&state).unwrap_or_else(|_| "No previous state.".to_string());

        let prompt = format!(
            "You are @{}. Role: {}

Previous state:
{}

Messages ({}):
{}

Instructions: Read CLAUDE.md only if you need to remember your rules. Act on the messages above. When done, output a JSON block between ---COLLAB_OUTPUT--- and ---END_COLLAB_OUTPUT--- markers with fields: response (string or null), delegate (array of {{to: string, task: string}}, optional), state_update (object, optional). Do NOT run collab CLI commands — the harness handles delivery.",
            self.instance_id,
            self.get_role(),
            state_str,
            messages.len(),
            msg_lines
        );

        // Spawn claude
        let output = Command::new("claude")
            .arg("-p")
            .arg(&prompt)
            .arg("--model")
            .arg(&self.model)
            .arg("--allowedTools")
            .arg("Bash,Read,Write,Edit")
            .current_dir(&self.workdir)
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let duration = start.elapsed().as_secs();

        // Parse structured output
        if let Some(collab_output) = self.parse_collab_output(&stdout) {
            // Send response
            if let Some(response) = &collab_output.response {
                if !response.is_empty() {
                    for msg in messages {
                        let _ = self.client.add_message(&msg.sender, response, None).await;
                    }
                }
            }

            // Delegate tasks
            for task in &collab_output.delegate {
                let to = task.to.trim_start_matches('@');
                let _ = self.client.todo_add(to, &task.task).await;
            }

            // Update state
            self.save_state(&collab_output.state_update);
        }

        let response_count = messages.len();
        self.log(&format!("done — claude exited ({}s), delivered {} responses", duration, response_count));

        Ok(())
    }

    fn parse_collab_output(&self, output: &str) -> Option<CollabOutput> {
        let start = output.find("---COLLAB_OUTPUT---")?;
        let end = output.find("---END_COLLAB_OUTPUT---")?;
        let json_str = &output[start + 19..end].trim();
        serde_json::from_str(json_str).ok()
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
        // Try to read from CLAUDE.md
        let claude_md = self.workdir.join("CLAUDE.md");
        if let Ok(contents) = std::fs::read_to_string(&claude_md) {
            // Extract first line after "Your role:"
            for line in contents.lines() {
                if line.contains("Your role:") {
                    if let Some(rest) = line.split("Your role:").nth(1) {
                        return rest.trim().trim_end_matches('*').to_string();
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
}
