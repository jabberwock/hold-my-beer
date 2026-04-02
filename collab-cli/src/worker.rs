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
    /// Task hashes marked as completed this invocation
    #[serde(default)]
    pub completed_tasks: Vec<String>,
    /// If true, harness re-sends this output back to the worker as a new message,
    /// keeping them working autonomously until they're actually blocked.
    #[serde(default)]
    pub r#continue: bool,
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
            auto_reply,
            batch_wait_ms,
            message_queue: Arc::new(Mutex::new(Vec::new())),
            first_message_time: Arc::new(Mutex::new(None)),
            hands_off_to,
            teammates,
        }
    }

    pub async fn run(&self) -> Result<()> {
        // Spawn batch processor task that wakes on timer
        let queue = self.message_queue.clone();
        let first_time = self.first_message_time.clone();
        let batch_wait_ms = self.batch_wait_ms;
        let client = self.client.clone();
        let instance_id = self.instance_id.clone();
        let workdir = self.workdir.clone();
        let model = self.model.clone();
        let auto_reply = self.auto_reply;
        let hands_off_to = self.hands_off_to.clone();
        let teammates = self.teammates.clone();

        tokio::spawn(async move {
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

                if should_process {
                    let messages = {
                        let mut q = queue.lock().await;
                        std::mem::take(&mut *q)
                    };
                    *first_time.lock().await = None;

                    // Process messages
                    let harness = WorkerHarness {
                        client: client.clone(),
                        instance_id: instance_id.clone(),
                        workdir: workdir.clone(),
                        model: model.clone(),
                        auto_reply,
                        batch_wait_ms,
                        message_queue: Arc::new(Mutex::new(Vec::new())),
                        first_message_time: Arc::new(Mutex::new(None)),
                        hands_off_to: hands_off_to.clone(),
                        teammates: teammates.clone(),
                    };
                    if let Err(e) = harness.spawn_claude(&messages).await {
                        harness.log_error(&format!("Failed to process {} messages: {}", messages.len(), e));
                    }
                }
            }
        });

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

    async fn spawn_claude(&self, messages: &[Message]) -> Result<()> {
        let start = std::time::Instant::now();

        // Load previous state
        let state = self.load_state();

        // Fetch pending todos for this worker
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

        // Build teammates section
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

        let prompt = format!(
            "You are @{}. Role: {}

{}

Previous state:
{}

{}

Messages ({}):
{}

Instructions: Act on the messages above. Use Bash/Read/Write/Edit to do your actual work (coding, research, testing). When done, output a JSON block between ---COLLAB_OUTPUT--- and ---END_COLLAB_OUTPUT--- markers:

```
---COLLAB_OUTPUT---
{{
  \"response\": \"your reply to the sender (string or null)\",
  \"delegate\": [{{\"to\": \"@worker\", \"task\": \"description\"}}],
  \"completed_tasks\": [\"hash1\", \"hash2\"],
  \"continue\": false,
  \"state_update\": {{\"key\": \"value\"}}
}}
---END_COLLAB_OUTPUT---
```

- \"response\": message back to whoever messaged you
- \"delegate\": assign new tasks to teammates
- \"completed_tasks\": list task hashes you finished (from your pending tasks above). The harness will mark them done and auto-route your output to downstream workers.
- \"continue\": set true to keep working — the harness will re-invoke you immediately with your output as context. Use this when you have more work to do. Set false when you're blocked or done.
- \"state_update\": persist any state for your next invocation

Do NOT run any collab CLI commands. The harness handles all messaging and task delivery. Focus on your actual work.",
            self.instance_id,
            self.get_role(),
            teammates_str,
            state_str,
            todos_str,
            messages.len(),
            msg_lines
        );

        // Spawn claude — strip COLLAB_* env vars so it can't shell out to collab CLI
        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg(&prompt)
            .arg("--model")
            .arg(&self.model)
            .arg("--allowedTools")
            .arg("Bash,Read,Write,Edit")
            .current_dir(&self.workdir);

        // Remove collab env vars from subprocess — harness handles all communication
        cmd.env_remove("COLLAB_INSTANCE");
        cmd.env_remove("COLLAB_SERVER");
        cmd.env_remove("COLLAB_TOKEN");

        let output = match cmd.output()
        {
            Ok(out) => out,
            Err(e) => {
                self.log_error(&format!("Failed to spawn claude: {}", e));
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
            self.log_error(&format!("Claude exited with status {}: {}", output.status, detail));
            return Err(anyhow::anyhow!("Claude failed: {}", detail));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let duration = start.elapsed().as_secs();

        // Parse structured output
        if let Some(collab_output) = self.parse_collab_output(&stdout) {
            // Send response
            if let Some(response) = &collab_output.response {
                if !response.is_empty() {
                    for msg in messages {
                        if let Err(e) = self.client.add_message(&msg.sender, response, None).await {
                            self.log_error(&format!("Failed to send response to @{}: {}", msg.sender, e));
                        }
                    }
                }
            }

            // Delegate tasks
            for task in &collab_output.delegate {
                let to = task.to.trim_start_matches('@');
                if let Err(e) = self.client.todo_add(to, &task.task).await {
                    self.log_error(&format!("Failed to add todo for @{}: {}", to, e));
                }
            }

            // Mark completed tasks and auto-route to downstream workers
            for hash in &collab_output.completed_tasks {
                let hash_clean = hash.trim();
                if hash_clean.is_empty() { continue; }
                match self.client.todo_done(hash_clean).await {
                    Ok(_) => self.log(&format!("task {} marked done", hash_clean)),
                    Err(e) => self.log_error(&format!("Failed to mark task {} done: {}", hash_clean, e)),
                }
            }

            // Pipeline: auto-dispatch to downstream workers
            if !collab_output.completed_tasks.is_empty() && !self.hands_off_to.is_empty() {
                let summary = collab_output.response.as_deref().unwrap_or("Task completed.");
                let handoff_msg = format!("Completed work from @{}: {}", self.instance_id, summary);
                for downstream in &self.hands_off_to {
                    let to = downstream.trim_start_matches('@');
                    if let Err(e) = self.client.add_message(to, &handoff_msg, None).await {
                        self.log_error(&format!("Failed to route to @{}: {}", to, e));
                    } else {
                        self.log(&format!("pipeline → @{}", to));
                    }
                }
            }

            // Self-kick: worker wants to keep going
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
            // Fallback: no markers found — treat entire stdout as the response
            let raw = stdout.trim().to_string();
            if !raw.is_empty() {
                self.log(&format!("no markers — sending raw response"));
                for msg in messages {
                    if let Err(e) = self.client.add_message(&msg.sender, &raw, None).await {
                        self.log_error(&format!("Failed to send response to @{}: {}", msg.sender, e));
                    }
                }
            }
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
