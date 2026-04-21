use anyhow::Result;
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration, Instant};

use crate::client::{CollabApi, CollabClient};

const PING_PATTERN: &str = r"(?i)^(ping|status|are you there\??|health ?check|you up\??)$";
/// Matches messages that OPEN with an acknowledgment phrase. Combined with a
/// length cap (ACK_MAX_LEN), this catches pure receipts like "Acknowledging — standing by"
/// without swallowing long content-bearing messages that merely open with "Thanks — ".
const ACK_START_PATTERN: &str = r"(?i)^(@[\w-]+[:,]?\s+)*\s*(acknowledged?|acknowledging|ack\b|aligned|standing by|same gate|holding|received|noted|roger|unchanged|freeze (holds|respected|unchanged)|gate freeze|doc freeze|standby|thanks|thank you|perfect|great|locked in|locked|got it|ok\b|okay|will do|on it|sounds good|understood|confirmed|copy that)";
/// Messages opening with an ack phrase AND shorter than this are swallowed.
/// Anything longer is assumed to carry real content after the opener.
const ACK_MAX_LEN: usize = 300;
/// Prefix the harness uses on post-CLI self-kicks ("you still have N pending
/// tasks"). It's a distinct marker so the batch loop can tell an auto-kick
/// apart from a boot-kick / human message / teammate delegation — and, via
/// that, (a) reset the auto-kick chain count when a real external arrives,
/// and (b) cap how many times an auto-kick can chain another auto-kick.
const AUTO_KICK_MARKER: &str = "[auto-kick] pending tasks";
pub const DEFAULT_CLI_TEMPLATE: &str = "claude -p {prompt} --model {model} --allowedTools Bash,Read,Write,Edit";

/// Truncate `s` to at most `max_bytes`, backing up to the nearest char
/// boundary so we don't split a multi-byte UTF-8 sequence. Appends `…`
/// when truncation happened. The naive `&s[..n]` panics if byte `n` lands
/// inside a character (how we first hit this: a teammate's message with
/// `×` produced `byte index 300 is not a char boundary; it is inside '×'
/// (bytes 299..301)` in the history-formatting path).
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Two dispatches: either the Rust harness answers directly (pings, acks —
/// no claude call), or we build a full context prompt and spawn the CLI.
///
/// An older design had a "Light" middle tier that stripped the teammates
/// list, state, and (critically) the todo list for short single messages to
/// save tokens. That optimisation was fighting prompt caching (static prefixes
/// cache for ~10% of nominal cost) and actively broke the product: auto-kick
/// nudges like "you have 6 pending tasks, pick up the next one" are short,
/// routed as Light, and left the worker with no todo list to act on — calls
/// hung for the full 300s timeout producing zero output. Dropped.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PromptTier {
    /// Handled entirely by the harness — no CLI spawn.
    Harness,
    /// Full prompt — role, teammates, state, todos, history, schema.
    Full,
}

impl std::fmt::Display for PromptTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptTier::Harness => write!(f, "harness"),
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

/// Merge an incoming partial state into prior on-disk state, replacing only
/// the fields the model actually populated. Exists because `state_update`
/// in claude's JSON has `#[serde(default)]` on every field, so a missing
/// field (or missing state_update entirely) deserializes to None/empty-vec.
/// A blind overwrite would then wipe memory on every turn the model omits a
/// field — which is most turns, since the prompt describes state_update as
/// optional. The observable symptom before this merge existed:
/// `.worker-state.json` went all-null across sessions, so each new claude
/// invocation had no memory of what it had just done, and workers like
/// d4webdev fell into "no context → nothing to say → silent" loops.
///
/// "Populated" means `Some(_)` for Option fields and non-empty for Vec. A
/// worker that genuinely needs to clear a field would need an explicit
/// sentinel (e.g. empty string for status) — the prompt doesn't document
/// one today, so merge-skip-on-empty is the safe default.
pub(crate) fn merge_state(prior: WorkerState, incoming: &WorkerState) -> WorkerState {
    let mut merged = prior;
    if incoming.last_task.is_some()       { merged.last_task = incoming.last_task.clone(); }
    if incoming.pending.is_some()         { merged.pending = incoming.pending.clone(); }
    if incoming.status.is_some()          { merged.status = incoming.status.clone(); }
    if !incoming.files_touched.is_empty() { merged.files_touched = incoming.files_touched.clone(); }
    merged
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
    client: Arc<dyn CollabApi>,
    instance_id: String,
    workdir: PathBuf,
    model: String,
    /// CLI command template — {prompt}, {model}, {workdir} placeholders
    cli_template: String,
    auto_reply: bool,
    batch_wait_ms: u64,
    message_queue: Arc<Mutex<Vec<Message>>>,
    first_message_time: Arc<Mutex<Option<Instant>>>,
    /// Pipeline: auto-dispatch to these workers on task completion
    hands_off_to: Vec<String>,
    /// All teammates (name + role) for prompt injection
    teammates: Vec<(String, String)>,
    /// Per-call CLI timeout. Constructor-defaulted to env COLLAB_CLI_TIMEOUT_SECS or 300s,
    /// but exposed as a field so tests can inject short values without touching global env.
    cli_timeout_secs: u64,
}

impl WorkerHarness {
    pub fn new(
        client: CollabClient,
        instance_id: String,
        workdir: PathBuf,
        model: String,
        cli_template: Option<String>,
        auto_reply: bool,
        batch_wait_ms: u64,
        hands_off_to: Vec<String>,
        teammates: Vec<(String, String)>,
    ) -> Self {
        Self::new_with_api(
            Arc::new(client),
            instance_id,
            workdir,
            model,
            cli_template,
            auto_reply,
            batch_wait_ms,
            hands_off_to,
            teammates,
        )
    }

    /// Build a harness around any `CollabApi` impl. Used by tests with a fake.
    pub fn new_with_api(
        client: Arc<dyn CollabApi>,
        instance_id: String,
        workdir: PathBuf,
        model: String,
        cli_template: Option<String>,
        auto_reply: bool,
        batch_wait_ms: u64,
        hands_off_to: Vec<String>,
        teammates: Vec<(String, String)>,
    ) -> Self {
        Self {
            client,
            instance_id,
            workdir,
            model,
            cli_template: cli_template.unwrap_or_else(|| DEFAULT_CLI_TEMPLATE.to_string()),
            auto_reply,
            batch_wait_ms,
            message_queue: Arc::new(Mutex::new(Vec::new())),
            first_message_time: Arc::new(Mutex::new(None)),
            hands_off_to,
            teammates,
            cli_timeout_secs: std::env::var("COLLAB_CLI_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(300),
        }
    }

    /// Override the CLI timeout. Returns self for chaining; primarily used by tests.
    pub fn with_cli_timeout_secs(mut self, secs: u64) -> Self {
        self.cli_timeout_secs = secs;
        self
    }

    /// Classify how much context a set of messages needs
    async fn classify_tier(&self, messages: &[Message]) -> PromptTier {
        // Ping/status checks → harness handles directly
        let ping_re = Regex::new(PING_PATTERN).unwrap();
        if messages.iter().all(|m| ping_re.is_match(m.content.trim())) {
            return PromptTier::Harness;
        }

        // Ack loop detection — swallow pure acknowledgments from other workers.
        // A message counts as a pure ack if it opens with an ack phrase AND is short
        // enough that it can't be carrying real content after the opener.
        let ack_re = Regex::new(ACK_START_PATTERN).unwrap();
        let is_pure_ack = |content: &str| -> bool {
            let trimmed = content.trim();
            trimmed.len() <= ACK_MAX_LEN && ack_re.is_match(trimmed)
        };
        let non_self_msgs: Vec<_> = messages.iter().filter(|m| m.sender != self.instance_id).collect();
        if !non_self_msgs.is_empty() && non_self_msgs.iter().all(|m| is_pure_ack(&m.content)) {
            return PromptTier::Harness;
        }

        // Anything that isn't a ping or ack needs the full prompt: the
        // worker has to see its teammates, state, and todo list to do real
        // work. Token cost is kept in check by prompt caching on the static
        // prefix, not by classification heuristics.
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
        let auto_reply = self.auto_reply;
        let hands_off_to = self.hands_off_to.clone();
        let teammates = self.teammates.clone();
        let batch_status = current_status.clone();
        let cli_timeout_secs = self.cli_timeout_secs;

        let max_self_kicks: u32 = 3;
        // Max auto-kicks triggered by a single external message. Each external
        // delegation/reply fires one CLI call; if that returns continue=false
        // and there are still pending todos, the harness auto-kicks — up to
        // this cap — so a chatty teammate delivering one task can trigger
        // work on a short backlog without every subsequent task needing
        // its own external nudge. The counter resets whenever a real
        // external message (not a sender="system" auto-kick) arrives.
        const MAX_AUTO_KICKS: u32 = 3;

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
                    let hands_off_to = watchdog_hands_off_to.clone();
                    let teammates = watchdog_teammates.clone();
                    let batch_status = watchdog_batch_status.clone();
                    let cli_lock = watchdog_cli_lock.clone();

                    tokio::spawn(async move {
                        let mut consecutive_kicks: u32 = 0;
                        // Counts auto-kicks chained off a single external message.
                        // Resets whenever a real external message arrives; stops
                        // chaining at MAX_AUTO_KICKS (see above).
                        let mut consecutive_auto_kicks: u32 = 0;
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

                // Track auto-kick chains. An auto-kick is a sender="system"
                // message whose content starts with AUTO_KICK_MARKER. A real
                // external message (anyone else — human, teammate) resets the
                // chain count, so the next backlog gets a fresh MAX_AUTO_KICKS
                // budget. This is checked below when deciding whether to
                // queue another auto-kick.
                let is_auto_kick_batch = messages.iter().any(|m|
                    m.sender == "system" && m.content.starts_with(AUTO_KICK_MARKER)
                );
                let has_real_external = has_external && !is_auto_kick_batch;
                if is_auto_kick_batch {
                    consecutive_auto_kicks += 1;
                } else if has_real_external {
                    consecutive_auto_kicks = 0;
                }
                let auto_kicks_so_far = consecutive_auto_kicks;

                let harness = WorkerHarness {
                    client: client.clone(),
                    instance_id: instance_id.clone(),
                    workdir: workdir.clone(),
                    model: model.clone(),
                    cli_template: cli_template.clone(),
                    auto_reply,
                    batch_wait_ms,
                    message_queue: Arc::new(Mutex::new(Vec::new())),
                    first_message_time: Arc::new(Mutex::new(None)),
                    hands_off_to: hands_off_to.clone(),
                    teammates: teammates.clone(),
                    cli_timeout_secs,
                };

                let tier = harness.classify_tier(&messages).await;

                match tier {
                    PromptTier::Harness => {
                        // Harness-tier is instant — handle inline, no lock needed
                        if let Err(e) = harness.handle_harness_tier(&messages).await {
                            harness.log_error(&format!("Harness tier failed: {}", e));
                        }
                    }
                    PromptTier::Full => {
                        // Spawn CLI in a background task so the batch loop stays unblocked.
                        // cli_lock serializes invocations — only one claude process at a time.
                        let cli_lock = cli_lock.clone();
                        let batch_status = batch_status.clone();
                        // Clones reserved for the panic-monitor task (must live
                        // outside the handle's `async move` since they're used
                        // AFTER the handle panics).
                        let batch_status_for_monitor = batch_status.clone();
                        let role_for_monitor = harness.get_role();
                        let queue = queue.clone();
                        let first_time_for_kick = first_time.clone();
                        let instance_id_for_kick = instance_id.clone();
                        let instance_id_for_log = instance_id.clone();
                        let hb_client = client.clone();

                        // Before spawning: push an immediate "working on…" status so the
                        // roster reflects activity in real time instead of waiting up to
                        // 30s for the next heartbeat tick.
                        let senders: Vec<String> = messages.iter()
                            .filter(|m| m.sender != instance_id && m.sender != "system")
                            .map(|m| format!("@{}", m.sender))
                            .collect::<std::collections::HashSet<_>>()
                            .into_iter().collect();
                        let working_status = if senders.is_empty() {
                            "working (self-kick)".to_string()
                        } else {
                            format!("working on msg from {}", senders.join(", "))
                        };
                        *batch_status.lock().await = working_status.clone();
                        let _ = hb_client.heartbeat(Some(&working_status)).await;

                        let handle = tokio::spawn(async move {
                            let _guard = cli_lock.lock().await;

                            let worker_continued = match harness.spawn_cli(&messages).await {
                                Ok(c) => c,
                                Err(e) => {
                                    harness.log_error(&format!("Failed to process {} messages: {}", messages.len(), e));
                                    false
                                }
                            };

                            // Update roster presence from worker state and push an
                            // immediate heartbeat so the new status is visible now,
                            // not on the next 30s tick.
                            let state = harness.load_state();
                            if let Some(status) = &state.status {
                                *batch_status.lock().await = status.clone();
                                let _ = harness.client.heartbeat(Some(status)).await;
                            } else {
                                // Worker didn't report a status — clear the "working on…"
                                // marker by falling back to the role line.
                                let role = harness.get_role();
                                *batch_status.lock().await = role.clone();
                                let _ = harness.client.heartbeat(Some(&role)).await;
                            }

                            // Auto-kick if worker has pending todos and didn't
                            // self-continue. Capped at MAX_AUTO_KICKS chained
                            // off a single external message — so one external
                            // delegation can drive up to MAX_AUTO_KICKS+1 CLI
                            // calls (the original + chained auto-kicks) against
                            // the backlog, but then the worker stops until
                            // someone new nudges it. Critical invariant: idle
                            // workers (no external activity) must NOT burn
                            // tokens — the tool ships with "idle = free" as a
                            // hard rule.
                            //
                            // The kick is queued directly as a sender="system"
                            // message rather than posted through add_message —
                            // round-tripping through the server would stamp
                            // sender=instance_id, which the batch loop strips
                            // as a self-message, and the kick would be silently
                            // eaten.
                            if !worker_continued && auto_kicks_so_far < MAX_AUTO_KICKS {
                                if let Ok(todos) = harness.client.fetch_todos(&harness.instance_id).await {
                                    if !todos.is_empty() {
                                        let mut q = queue.lock().await;
                                        if q.is_empty() {
                                            q.push(Message {
                                                sender: "system".to_string(),
                                                recipient: instance_id_for_kick.clone(),
                                                content: format!(
                                                    "{} — you have {} pending task(s). Pick up the next one when ready.",
                                                    AUTO_KICK_MARKER, todos.len()
                                                ),
                                                hash: String::new(),
                                                timestamp: Utc::now(),
                                            });
                                            *first_time_for_kick.lock().await = Some(Instant::now());
                                        }
                                    }
                                }
                            }
                        });

                        // Monitor for panics in the CLI-spawn task. The panic
                        // hook in main.rs captures the site and payload, but
                        // we ALSO need to reset batch_status here — the CLI
                        // task sets it to "working on msg from …" just before
                        // spawn_cli and only resets it in the happy-path
                        // post-CLI block. A panic in between leaves presence
                        // frozen on "working" forever, with no claude process
                        // to back it up — the exact silent-stall mode that
                        // was undiagnosable before the panic hook existed.
                        let status_recovery = batch_status_for_monitor.clone();
                        let role_recovery = role_for_monitor.clone();
                        let hb_recovery = hb_client.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle.await {
                                if e.is_panic() {
                                    let msg = format!(
                                        "[{}] [{}] CLI task panicked — resetting status, cli_lock released",
                                        Utc::now().format("%H:%M:%S UTC"), instance_id_for_log);
                                    eprintln!("{msg}");
                                    // Best-effort append to the worker error
                                    // log so GUI-launched workers (stderr
                                    // null'd) leave a trace.
                                    use std::io::Write;
                                    if let Ok(mut f) = std::fs::OpenOptions::new()
                                        .create(true).append(true)
                                        .open("/tmp/collab-worker-errors.log")
                                    {
                                        let _ = f.write_all(format!("{msg}\n").as_bytes());
                                    }
                                    // Unstick presence — status goes back to
                                    // role so the roster stops lying.
                                    *status_recovery.lock().await = role_recovery.clone();
                                    let _ = hb_recovery.heartbeat(Some(&role_recovery)).await;
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

        // Heartbeat presence — role updates dynamically from worker state.
        //
        // Server-is-gone self-terminate: with the GUI's macOS Cmd+Q intercept
        // wired up (see collab-gui/src-tauri/src/lib.rs::macos_quit_intercept),
        // the dialog → `collab stop all` path is the primary cleanup. This
        // loop only catches the residual cases where that path can't run:
        // hard process kill (`kill -9`, Force Quit), power loss, or a server
        // crash unrelated to user quit.
        //
        // Tightened to fail fast — a single 10s heartbeat outage is enough.
        // The trade-off: ~10s of token burn after server disappears, vs.
        // false-killing a worker if a network hiccup drops one heartbeat.
        // For local-host server (the default), drops are essentially never
        // network — they're the server actually being gone.
        let hb_client = self.client.clone();
        let hb_status = current_status.clone();
        let hb_workdir = self.workdir.clone();
        let hb_instance_id = self.instance_id.clone();
        const HEARTBEAT_INTERVAL_SECS: u64 = 10;
        tokio::spawn(async move {
            const SELF_KILL_AFTER: u32 = 1;
            let mut consecutive_failures: u32 = 0;
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
                match hb_client.heartbeat(Some(&role)).await {
                    Ok(_) => {
                        consecutive_failures = 0;
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        eprintln!(
                            "[{}] [{}] heartbeat failed ({}/{}): {}",
                            chrono::Utc::now().format("%H:%M:%S UTC"),
                            hb_instance_id,
                            consecutive_failures, SELF_KILL_AFTER, e
                        );
                        if consecutive_failures >= SELF_KILL_AFTER {
                            eprintln!(
                                "[{}] [{}] server unreachable for {} consecutive heartbeats — self-terminating to avoid running orphaned",
                                chrono::Utc::now().format("%H:%M:%S UTC"),
                                hb_instance_id,
                                consecutive_failures
                            );
                            // Kill the whole process tree so any in-flight CLI
                            // subprocess (claude -p, cursor -p, ollama …) goes
                            // down with us. No child left holding tokens.
                            //
                            // Unix: `collab worker` was made a process group
                            // leader by spawn_worker in lifecycle.rs, so one
                            // `killpg(getpid(), SIGTERM)` cascades to everyone.
                            //
                            // Windows: no process-group equivalent — we shell
                            // out to `taskkill /F /T /PID <self>` which kills
                            // our PID and every descendant. Synchronous so we
                            // don't race our own exit.
                            #[cfg(unix)]
                            unsafe {
                                libc::killpg(std::process::id() as libc::pid_t, libc::SIGTERM);
                            }
                            #[cfg(windows)]
                            {
                                let pid = std::process::id().to_string();
                                let _ = std::process::Command::new("taskkill")
                                    .args(["/F", "/T", "/PID", &pid])
                                    .status();
                            }
                            // Belt-and-suspenders: if the tree-kill didn't take
                            // us down within a second (signal handler, taskkill
                            // failure, etc.), force-exit this process.
                            sleep(Duration::from_secs(1)).await;
                            std::process::exit(0);
                        }
                    }
                }
                sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
            }
        });

        let mut booted = false;
        let mut backoff_secs = 1u64;

        loop {
            let url = format!("{}/events/{}", self.client.base_url(), self.instance_id);
            let mut req = self.client.http_client().get(&url).header("Accept", "text/event-stream");

            if let Some(token) = self.client.bearer_token() {
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

    /// Build the full-context prompt for a CLI invocation: role, teammates,
    /// previous state, todo list, recent history, rules, and output schema.
    async fn build_prompt(&self, messages: &[Message]) -> Result<String> {
        // Format message lines
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
                        let content = truncate_at_char_boundary(&m.content, 300);
                        lines.push_str(&format!("  @{} → @{}: {}\n", m.sender, m.recipient, content));
                    }
                    lines
                }
            }
            Err(_) => String::new(),
        };

        // Prompt layout is deliberately "stable block first, dynamic block
        // last" so claude's prompt cache has the longest possible common
        // prefix across calls. Anything that changes per call (state, todos,
        // history, new messages) goes at the END — the cache breaks at the
        // first byte of difference, so putting rules / schema / identity
        // lines up front means all of that caches.
        //
        // Per-worker identity is cacheable across THAT worker's calls;
        // teammates list rotates rarely. The "You are @X" / "Your team:" /
        // rules / schema block totals ~2–3KB and should all cache.
        Ok(format!(
            "You are @{instance_id}. Role: {role}

{teammates}
## CRITICAL RULES (absolute — no exceptions)

**NO DATES OR TIMES.** Never mention dates, deadlines, ETAs, or time estimates in any message or response. If a teammate mentioned a date (e.g. 'EOD April 17th', 'by Thursday'), ignore it — it was hallucinated. You have no calendar awareness between invocations.

**VERIFY BEFORE PROPAGATING.** Before acting on any claim from a teammate (file exists, bug found, task complete, feature ready), run a tool call to verify it yourself. Never forward an unverified claim to another worker. If you cannot verify it, say so explicitly — do not assume it's true.

**NO ACK RESPONSES.** Never send acknowledgment-only messages ('Got it', 'Understood', 'Standing by', 'Will do'). Either do the work immediately and report results, or stay silent. Ack loops waste cycles and propagate hallucinated context.

**IF IT DOESN'T EXIST, SAY SO.** If asked to work on something that doesn't exist (a file, a feature, a task), stop, report what you actually found with a tool call, and ask for clarification. Do not invent context.

**NEVER NARRATE DELEGATION.** If you write \"I've delegated to @X\", \"I'll send this to @Y\", \"assigned to @Z\" or similar in your response, you MUST have a matching entry in the delegate[] array. Claiming you delegated when delegate[] is empty is a lie — the recipient never gets a todo, and your sender thinks it was handled. Either fill delegate[] or don't claim to delegate.

## Output format

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
- delegate: the ONLY way to assign work to another worker. Each entry becomes a persistent todo on the server. Writing about delegation in `response` or `messages` does NOT assign anything — if you want someone to act, you MUST fill delegate[] or nothing happens. If someone messages you asking for something and you need THEM to act, delegate back to THEM — not to a random teammate. The task description must be self-contained: facts, URLs, decisions, context — the recipient will NOT see the messages that led to this delegation.
- messages: null always. Never send status updates, confirmations, or narration. Use delegate for work assignments. If you have nothing to assign, omit this field entirely.
- completed_tasks: task hashes you finished — marks done and routes to downstream workers (optional)
- continue: true to keep working autonomously, false when blocked or done
- state_update: persist state for next invocation. Include \"status\" to update your roster presence

Do NOT run any `collab` command in this session — the harness manages collab state and the relevant env vars are unset here. Use Bash/Read/Write/Edit for actual work; emit collab actions via the JSON object above.

═══ Session context (varies per call — everything above this line is cacheable) ═══

Previous state:
{state}

{todos}
{history}New messages ({n}):
{msg_lines}

Act on the new messages above. Use Bash/Read/Write/Edit to do your actual work (coding, research, testing).",
            instance_id = self.instance_id,
            role = self.get_role(),
            teammates = teammates_str,
            state = state_str,
            todos = todos_str,
            history = history_str,
            n = messages.len(),
            msg_lines = msg_lines
        ))
    }

    /// Returns Ok(true) if the worker set continue: true, Ok(false) otherwise.
    async fn spawn_cli(&self, messages: &[Message]) -> Result<bool> {
        let start = std::time::Instant::now();
        let tier = PromptTier::Full;

        let prompt = self.build_prompt(messages).await?;

        let active_template = &self.cli_template;

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
        let substituted: Vec<String> = template_parts.iter().map(|part| {
            part.replace("{prompt}", &prompt)
                .replace("{model}", &self.model)
                .replace("{workdir}", &workdir_str)
        }).collect();

        // Pull leading KEY=VALUE pairs off as subprocess env vars (shell-style).
        // Without this, a template like `OLLAMA_HOST=... script -p {prompt}` spawns
        // `OLLAMA_HOST=...` as a binary and fails — the original symptom that took
        // an hour to track down.
        let (extra_env, mut parts) = split_env_prefix(substituted);
        if parts.is_empty() {
            return Err(anyhow::anyhow!(
                "cli_template has no command after env-var prefixes: {}",
                active_template
            ));
        }

        // Detect claude CLI — inject --output-format json to get real token counts and cost
        let is_claude_cli = parts[0].ends_with("claude");
        if is_claude_cli {
            parts.push("--output-format".to_string());
            parts.push("json".to_string());
        }

        let mut cmd = Command::new(&parts[0]);
        cmd.args(&parts[1..])
            .current_dir(&self.workdir)
            .kill_on_drop(true);

        for (k, v) in &extra_env {
            cmd.env(k, v);
        }

        // Strip collab env vars — harness owns all collab traffic.
        // Workers must NOT call `collab send`/`status`/etc in the subprocess;
        // outgoing actions go via the JSON output schema. The prompt enforces this.
        cmd.env_remove("COLLAB_INSTANCE");
        cmd.env_remove("COLLAB_SERVER");
        cmd.env_remove("COLLAB_TOKEN");

        // Single sink for failure dumps. Writes /tmp/collab-debug-<instance>.txt.
        // Successful invocations clean this up at the end of process_messages.
        let write_debug = |kind: &str, exit: &str, stdout: &str, stderr: &str| {
            let debug_path = format!("/tmp/collab-debug-{}.txt", self.instance_id);
            let _ = std::fs::write(&debug_path, format!(
                "KIND: {}\nEXIT: {}\nCOMMAND: {}\nENV_OVERRIDES: {:?}\n\
                 STDOUT ({} bytes):\n{}\nSTDERR ({} bytes):\n{}\n\
                 PROMPT ({} bytes):\n{}",
                kind, exit, parts.join(" "), extra_env,
                stdout.len(), stdout, stderr.len(), stderr,
                prompt.len(), prompt
            ));
        };

        let timeout_secs = self.cli_timeout_secs;

        let output = match tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            cmd.output(),
        ).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                write_debug("spawn-failed", "n/a", "", &e.to_string());
                self.log_error(&format!(
                    "Failed to spawn '{}': {} — see /tmp/collab-debug-{}.txt",
                    parts[0], e, self.instance_id
                ));
                return Err(e.into());
            }
            Err(_) => {
                write_debug("timeout", &format!("killed after {}s", timeout_secs), "", "");
                self.log_error(&format!(
                    "CLI timed out after {}s — see /tmp/collab-debug-{}.txt (override with COLLAB_CLI_TIMEOUT_SECS)",
                    timeout_secs, self.instance_id
                ));
                return Err(anyhow::anyhow!("CLI timed out after {}s", timeout_secs));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            write_debug("exit-failed", &output.status.to_string(), &stdout, &stderr);
            let detail = if stderr.trim().is_empty() && stdout.trim().is_empty() {
                format!("(empty output — see /tmp/collab-debug-{}.txt)", self.instance_id)
            } else if stderr.trim().is_empty() {
                stdout.to_string()
            } else {
                stderr.to_string()
            };
            self.log_error(&format!("CLI exited with status {}: {}", output.status, detail));
            return Err(anyhow::anyhow!("CLI failed: {}", detail));
        }

        let raw_stdout = String::from_utf8_lossy(&output.stdout);

        // For claude CLI: unwrap --output-format json envelope to get real
        // token counts and cost. Break the three input buckets apart so the
        // usage report can expose cache hit rate to the user — they're what
        // tell us whether prompt caching is actually firing.
        //
        //   input_tokens:               new tokens the API saw for the first time
        //   cache_creation_input_tokens: tokens that got written to cache this call
        //   cache_read_input_tokens:     tokens served from cache (the cheap ones)
        let (stdout, real_input_tokens, cache_creation_tokens, cache_read_tokens,
             real_output_tokens, cost_usd) = if is_claude_cli {
            match serde_json::from_str::<serde_json::Value>(&raw_stdout) {
                Ok(v) => {
                    let inner = v.get("result")
                        .and_then(|r| r.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input_tok = v.pointer("/usage/input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    let cache_creation = v.pointer("/usage/cache_creation_input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    let cache_read = v.pointer("/usage/cache_read_input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    let output_tok = v.pointer("/usage/output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    let cost = v.get("total_cost_usd").and_then(|c| c.as_f64());
                    (std::borrow::Cow::Owned(inner), input_tok, cache_creation, cache_read, output_tok, cost)
                }
                Err(e) => {
                    self.log_error(&format!("Failed to parse claude JSON envelope: {e}"));
                    (raw_stdout, 0u64, 0u64, 0u64, 0u64, None)
                }
            }
        } else {
            (raw_stdout, 0u64, 0u64, 0u64, 0u64, None)
        };

        let duration = start.elapsed().as_secs();

        // Parse structured output
        let mut did_continue = false;
        let mut debug_was_dumped = false;
        if let Some(mut collab_output) = self.parse_collab_output(&stdout) {
            // Known teammates for delegate-target validation + auto-extraction.
            let known_teammates: std::collections::HashSet<String> = self.teammates.iter()
                .map(|(name, _)| name.clone())
                .collect();

            // Guardrail: workers repeatedly claim \"I've delegated\" / \"delegating
            // to @X\" in their response text but leave delegate[] empty. The
            // prompt explicitly forbids this, and models still do it — the
            // failure is stable enough that the only remedy left is to make
            // code enforce what the prompt can only ask for.
            //
            // Detection: response mentions delegation AND delegate[] is empty.
            // Resolution: if the response has exactly one @mention that's a
            // known teammate, synthesize a DelegateTask with the full
            // response as the task body (self-contained context). Otherwise
            // scrub the lie from the response so downstream readers aren't
            // misled. Either way, log so the human can see it happened.
            let delegation_claim_re = regex::Regex::new(
                r"(?i)\b(I(?:'ve|\s+have)?\s+(?:delegated|assigned|sent)|I(?:'ll|\s+will)\s+(?:delegate|assign|send)|delegating|delegated|assigned|assigning)\b"
            ).unwrap();
            let mention_re = regex::Regex::new(r"@(\w+)").unwrap();

            if collab_output.delegate.is_empty() {
                if let Some(response) = collab_output.response.as_ref() {
                    if delegation_claim_re.is_match(response) {
                        let candidates: Vec<String> = mention_re.captures_iter(response)
                            .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
                            .filter(|name| known_teammates.contains(name) && name != &self.instance_id)
                            .collect::<std::collections::HashSet<_>>()
                            .into_iter()
                            .collect();
                        if candidates.len() == 1 {
                            let target = candidates.into_iter().next().unwrap();
                            self.log(&format!(
                                "auto-extracting delegation: response claimed delegation with no delegate[] entry; synthesizing @{} with response as task body",
                                target
                            ));
                            collab_output.delegate.push(DelegateTask {
                                to: target,
                                task: response.clone(),
                            });
                        } else {
                            self.log(&format!(
                                "scrubbing phantom delegation claim from response ({} unambiguous @-mentions of known teammates); the model lied about delegating",
                                candidates.len()
                            ));
                            // Replace the response with a truthful note so the
                            // sender knows the claimed delegation didn't happen
                            // rather than being told it did.
                            collab_output.response = Some(
                                "(worker claimed to delegate but couldn't — no unambiguous target found; please restate what you need)".to_string()
                            );
                        }
                    }
                }
            }

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

            // Delegate tasks — create todo (server inserts the "📋 New task assigned"
            // notification atomically with the todo; no ping needed from here).
            // Validate target against known teammates to prevent hallucinated
            // delegations (ghost-worker todos that pile up on the server).
            //
            // Always validate — the previous guard short-circuited when
            // `known_teammates` was empty, which let a solo worker invent
            // teammates freely and led to the d4dataminer → @webdev incident.
            // Self-delegation and the reserved broadcast target `@all` stay
            // allowed so a worker can queue work for itself or ping everyone.
            // (known_teammates set was built above for the delegation-claim
            // guardrail — reused here.)
            for task in &collab_output.delegate {
                let to = task.to.trim_start_matches('@');
                if !is_allowed_delegate_target(to, &self.instance_id, &known_teammates) {
                    self.log_error(&format!(
                        "Skipping delegation to unknown worker @{} (not a teammate, not self, not a reserved target) — possible hallucination",
                        to
                    ));
                    continue;
                }
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

            // Mark completed tasks and auto-route to downstream workers.
            // Only route pipeline if tasks were *actually* confirmed done by the server —
            // prevents hallucinated hashes from triggering downstream work.
            //
            // Cap exists to catch a worker that hallucinates bulk completions
            // ("I finished all 50 tasks you had!"). A real backlog plus some
            // follow-on work can legitimately hit ~10–15 completions in one
            // turn, so 20 is a reasonable ceiling — still an obvious outlier
            // when tripped.
            let max_completions = 20;
            if collab_output.completed_tasks.len() > max_completions {
                self.log_error(&format!(
                    "Worker tried to mark {} tasks done in one call (cap: {}) — processing first {}, ignoring rest",
                    collab_output.completed_tasks.len(), max_completions, max_completions
                ));
            }
            let mut completed_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut actually_completed: usize = 0;
            for hash in collab_output.completed_tasks.iter().take(max_completions) {
                let hash_clean = hash.trim();
                if hash_clean.is_empty() { continue; }
                if !completed_seen.insert(hash_clean.to_string()) {
                    self.log(&format!("skipped duplicate completion for {}", hash_clean));
                    continue;
                }
                match self.client.todo_done(hash_clean).await {
                    Ok(_) => {
                        self.log(&format!("task {} marked done", hash_clean));
                        actually_completed += 1;
                    }
                    Err(e) => self.log_error(&format!("Failed to mark task {} done: {}", hash_clean, e)),
                }
            }

            // Pipeline: auto-dispatch to downstream workers. Skip any that
            // already received a message this turn via response/delegate/messages.
            // Guard: only route if at least one task was *actually* confirmed done.
            if actually_completed > 0 && !self.hands_off_to.is_empty() {
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
                    // Dump raw output so we can see what the model actually produced —
                    // silent drop with no artifact was untraceable.
                    let debug_path = format!("/tmp/collab-debug-{}.txt", self.instance_id);
                    let _ = std::fs::write(&debug_path, format!(
                        "KIND: json-parse-failed\nRAW_OUTPUT ({} bytes):\n{}\n",
                        raw.len(), raw
                    ));
                    debug_was_dumped = true;
                    self.log_error(&format!(
                        "JSON parse failed — output looks like structured JSON but couldn't be parsed. \
                         Raw dumped to {}. Not sending to team.",
                        debug_path
                    ));
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
        let (log_input_tokens, log_cache_creation, log_cache_read, log_output_tokens) = if is_claude_cli {
            (real_input_tokens, cache_creation_tokens, cache_read_tokens, real_output_tokens)
        } else {
            (prompt.len() as u64 / 4, 0, 0, stdout.len() as u64 / 4)
        };
        let cost_str = cost_usd.map(|c| format!(", ${:.4}", c)).unwrap_or_default();
        // Fold cache activity into the log line so we can eyeball hit rate
        // without hitting the /usage endpoint. Only printed when non-zero.
        let cache_str = if log_cache_creation + log_cache_read > 0 {
            format!(" (cache write {}, read {})", log_cache_creation, log_cache_read)
        } else {
            String::new()
        };
        self.log(&format!("done — {}s, {}+{} tokens{}{}",
            duration, log_input_tokens, log_output_tokens, cache_str, cost_str));

        // Report usage delta to server — authoritative running totals.
        let cli_name = self.cli_template.split_whitespace().next().unwrap_or("unknown");
        let tier_str = tier.to_string();
        let report = crate::client::UsageReport {
            worker: &self.instance_id,
            duration_secs: duration,
            input_tokens: log_input_tokens,
            cache_creation_tokens: log_cache_creation,
            cache_read_tokens: log_cache_read,
            output_tokens: log_output_tokens,
            tier: &tier_str,
            cost_usd,
            cli: Some(cli_name),
        };
        if let Err(e) = self.client.report_usage(&report).await {
            self.log(&format!("warn: failed to report usage to server: {}", e));
        }

        // Clean up temp files from this invocation
        for msg in messages {
            if msg.content.len() > 2000 {
                let hash_short = &msg.hash[..7.min(msg.hash.len())];
                let tmp_path = format!("/tmp/collab-msg-{}.md", hash_short);
                let _ = std::fs::remove_file(&tmp_path);
            }
        }
        // Remove debug dump from previous failure (if this call succeeded).
        // Skip cleanup if THIS call dumped — otherwise we wipe the artifact we just wrote.
        if !debug_was_dumped {
            let debug_path = format!("/tmp/collab-debug-{}.txt", self.instance_id);
            let _ = std::fs::remove_file(&debug_path);
        }

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
        let merged = merge_state(self.load_state(), state);
        if let Ok(json) = serde_json::to_string_pretty(&merged) {
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

/// Pull leading `KEY=VALUE` parts off a shell-split command, so cli_templates like
/// `OLLAMA_HOST=... OLLAMA_NUM_CTX=... /path/to/script -p {prompt}` work the way
/// they would in a shell. Without this, `Command::new("OLLAMA_HOST=...")` tries
/// to spawn that string as a binary and silently fails.
fn split_env_prefix(parts: Vec<String>) -> (Vec<(String, String)>, Vec<String>) {
    let split_at = parts
        .iter()
        .position(|p| !is_env_assignment(p))
        .unwrap_or(parts.len());
    let env: Vec<(String, String)> = parts[..split_at]
        .iter()
        .map(|p| {
            let eq = p.find('=').unwrap();
            (p[..eq].to_string(), p[eq + 1..].to_string())
        })
        .collect();
    (env, parts[split_at..].to_vec())
}

fn is_env_assignment(s: &str) -> bool {
    let Some(eq) = s.find('=') else {
        return false;
    };
    let key = &s[..eq];
    !key.is_empty()
        && key
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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

/// Whether `to` is a valid delegate target for a worker with the given
/// `own_instance_id` and `known_teammates`. Reserved targets (`@all` for
/// broadcast, `@human` for the person at the keyboard) and self-delegation
/// are always allowed; everything else must be a known teammate or it's
/// rejected as a hallucination.
pub(crate) fn is_allowed_delegate_target(
    to: &str,
    own_instance_id: &str,
    known_teammates: &std::collections::HashSet<String>,
) -> bool {
    if to == own_instance_id {
        return true;
    }
    if to == "all" || to == "human" {
        return true;
    }
    known_teammates.contains(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REGRESSION: `&s[..300]` panics when byte 300 lands inside a multi-byte
    /// character. Live workers crashed on a teammate message containing `×`
    /// (2-byte UTF-8) aligned so that byte 300 was byte-1-of-2.
    #[test]
    fn truncate_at_char_boundary_never_splits_multibyte() {
        // Build a string where a × lands spanning byte 299..=300, so a naive
        // `&s[..300]` would panic with "not a char boundary".
        let mut s = String::new();
        while s.len() < 299 { s.push('a'); }
        s.push('×'); // 2 bytes — now at byte 299..=300
        while s.len() < 400 { s.push('b'); }
        let out = truncate_at_char_boundary(&s, 300);
        // The × would've made 300 an illegal boundary; we should have backed
        // up to 299 and appended the ellipsis.
        assert!(out.ends_with('…'));
        assert!(out.starts_with(&"a".repeat(299)));
        assert!(!out.contains('×'), "partial × should not appear");
    }

    #[test]
    fn truncate_at_char_boundary_passes_through_short_input() {
        assert_eq!(truncate_at_char_boundary("hi", 300), "hi");
    }

    #[test]
    fn truncate_at_char_boundary_handles_ascii() {
        let s = "a".repeat(500);
        let out = truncate_at_char_boundary(&s, 100);
        assert_eq!(out.len(), 100 + "…".len());
        assert!(out.ends_with('…'));
    }

    #[test]
    fn allowed_delegate_accepts_self_broadcast_human_and_teammates() {
        let teammates: std::collections::HashSet<String> =
            ["webdev", "architect"].iter().map(|s| s.to_string()).collect();
        assert!(is_allowed_delegate_target("d4dataminer", "d4dataminer", &teammates), "self");
        assert!(is_allowed_delegate_target("all", "d4dataminer", &teammates), "broadcast");
        assert!(is_allowed_delegate_target("human", "d4dataminer", &teammates), "human");
        assert!(is_allowed_delegate_target("webdev", "d4dataminer", &teammates), "known teammate");
        assert!(!is_allowed_delegate_target("ghost", "d4dataminer", &teammates), "unknown rejected");
    }

    /// REGRESSION: the pre-fix guard short-circuited when teammates was empty,
    /// which is how d4dataminer (solo, hands_off_to: []) silently created a
    /// todo for the nonexistent @webdev. Empty teammates list must still
    /// validate against the reserved allow-list.
    #[test]
    fn allowed_delegate_rejects_unknown_when_teammates_empty() {
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(!is_allowed_delegate_target("webdev", "d4dataminer", &empty),
            "solo worker must not invent teammates — this is the d4dataminer regression");
        // But the reserved targets still work even with no teammates.
        assert!(is_allowed_delegate_target("human", "d4dataminer", &empty));
        assert!(is_allowed_delegate_target("all", "d4dataminer", &empty));
        assert!(is_allowed_delegate_target("d4dataminer", "d4dataminer", &empty), "self");
    }

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

    // ── merge_state — regression: see merge_state() doc for why this matters.
    // Every test name below describes a shape of claude JSON we've seen in the
    // wild that *used* to wipe prior state.

    fn prior_full() -> WorkerState {
        WorkerState {
            last_task: Some("abc1234".to_string()),
            pending: Some("follow-up to abc1234".to_string()),
            files_touched: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            status: Some("working on map fix".to_string()),
        }
    }

    #[test]
    fn merge_state_missing_state_update_preserves_prior() {
        // Claude returned {"response": "...", "continue": false} with no
        // state_update. serde fills the incoming with all defaults. Prior
        // state must survive.
        let incoming = WorkerState::default();
        let merged = merge_state(prior_full(), &incoming);
        assert_eq!(merged.status.as_deref(), Some("working on map fix"));
        assert_eq!(merged.last_task.as_deref(), Some("abc1234"));
        assert_eq!(merged.files_touched.len(), 2);
    }

    #[test]
    fn merge_state_empty_object_preserves_prior() {
        // "state_update": {} — serde still fills defaults. Same outcome as
        // missing entirely; prior state must survive.
        let incoming: WorkerState = serde_json::from_str("{}").unwrap();
        let merged = merge_state(prior_full(), &incoming);
        assert_eq!(merged.status.as_deref(), Some("working on map fix"));
        assert_eq!(merged.files_touched.len(), 2);
    }

    #[test]
    fn merge_state_partial_update_replaces_only_provided_fields() {
        // Claude updates status but doesn't mention files_touched. We keep
        // the old files_touched instead of wiping it to [].
        let incoming: WorkerState = serde_json::from_str(
            r#"{"status": "idle — map fix landed"}"#
        ).unwrap();
        let merged = merge_state(prior_full(), &incoming);
        assert_eq!(merged.status.as_deref(), Some("idle — map fix landed"));
        // Untouched fields stay:
        assert_eq!(merged.last_task.as_deref(), Some("abc1234"));
        assert_eq!(merged.files_touched, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
    }

    #[test]
    fn merge_state_nonempty_files_touched_replaces() {
        // When claude DOES provide files_touched, it replaces wholesale (not
        // appends). That's consistent with status: the worker's latest call
        // is the authoritative snapshot of what it touched this turn.
        let incoming: WorkerState = serde_json::from_str(
            r#"{"files_touched": ["src/map.css"]}"#
        ).unwrap();
        let merged = merge_state(prior_full(), &incoming);
        assert_eq!(merged.files_touched, vec!["src/map.css".to_string()]);
        // Other fields preserved:
        assert_eq!(merged.status.as_deref(), Some("working on map fix"));
    }

    #[test]
    fn merge_state_full_update_replaces_everything() {
        // Sanity check: when claude populates all fields, merge behaves like
        // the old blind-overwrite did.
        let incoming = WorkerState {
            last_task: Some("def5678".to_string()),
            pending: None,
            files_touched: vec!["src/x.rs".to_string()],
            status: Some("done".to_string()),
        };
        let merged = merge_state(prior_full(), &incoming);
        assert_eq!(merged.last_task.as_deref(), Some("def5678"));
        // pending is None in incoming → prior is preserved. This is the
        // designed semantic; if a worker needs to clear pending, the prompt
        // will need a sentinel.
        assert_eq!(merged.pending.as_deref(), Some("follow-up to abc1234"));
        assert_eq!(merged.status.as_deref(), Some("done"));
        assert_eq!(merged.files_touched, vec!["src/x.rs".to_string()]);
    }

    #[test]
    fn merge_state_null_fields_preserve_prior() {
        // Claude occasionally emits explicit nulls instead of omitting.
        // serde deserializes both to None, so behavior is identical — prior
        // state survives.
        let incoming: WorkerState = serde_json::from_str(
            r#"{"last_task": null, "pending": null, "status": null, "files_touched": null}"#
        ).unwrap_or_default();
        let merged = merge_state(prior_full(), &incoming);
        assert_eq!(merged.status.as_deref(), Some("working on map fix"));
        assert_eq!(merged.files_touched.len(), 2);
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
    }

    #[test]
    fn ack_pattern_matches_real_chat_openers() {
        // From actual traffic in the StarvingActor session — these were triggering
        // full CLI spawns because the old pattern missed them.
        let re = Regex::new(ACK_START_PATTERN).unwrap();
        assert!(re.is_match("Acknowledging two items:"));
        assert!(re.is_match("Thanks — glad the cross-platform mapping landed well"));
        assert!(re.is_match("Perfect — standing by. Flag anything"));
        assert!(re.is_match("Locked in — will ping you the moment"));
        assert!(re.is_match("Got it — will hold on that"));
        assert!(re.is_match("Confirmed, moving to Sprint 3"));
        assert!(re.is_match("Copy that"));
        assert!(re.is_match("Sounds good — proceeding"));
        assert!(re.is_match("Understood"));
    }

    #[test]
    fn ack_length_cap_protects_content_bearing_messages() {
        // Opening with an ack word but carrying real content afterward should NOT
        // be swallowed. The classify_tier caller applies ACK_MAX_LEN; the regex alone
        // will still match, so we verify both: pattern matches, length cap rejects.
        let re = Regex::new(ACK_START_PATTERN).unwrap();
        let long = "Perfect — exactly what I wanted to see. The design system translating well to Android is exactly what we hoped for. Material3 dark surface overlays (#0E0E12, #15151A) map 1:1 to our iOS brand tokens, and the 48dp minimum tap targets are aligned with our accessibility audit findings. Please continue with Phase 0 of the Android implementation.";
        assert!(re.is_match(long), "pattern still matches — cap is what filters");
        assert!(long.len() > ACK_MAX_LEN, "this is the real case we need to let through");
    }

    fn s(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn env_prefix_extracts_leading_assignments() {
        let (env, cmd) = split_env_prefix(s(&[
            "OLLAMA_HOST=http://mbpc:11434",
            "OLLAMA_NUM_CTX=32768",
            "/path/to/script",
            "-p",
            "{prompt}",
        ]));
        assert_eq!(env.len(), 2);
        assert_eq!(env[0], ("OLLAMA_HOST".to_string(), "http://mbpc:11434".to_string()));
        assert_eq!(env[1], ("OLLAMA_NUM_CTX".to_string(), "32768".to_string()));
        assert_eq!(cmd, s(&["/path/to/script", "-p", "{prompt}"]));
    }

    #[test]
    fn env_prefix_stops_at_first_non_assignment() {
        // KEY=VALUE after the command should NOT be treated as env
        let (env, cmd) = split_env_prefix(s(&["claude", "FOO=bar", "-p", "x"]));
        assert!(env.is_empty());
        assert_eq!(cmd, s(&["claude", "FOO=bar", "-p", "x"]));
    }

    #[test]
    fn env_prefix_no_assignments() {
        let (env, cmd) = split_env_prefix(s(&["claude", "-p", "{prompt}"]));
        assert!(env.is_empty());
        assert_eq!(cmd, s(&["claude", "-p", "{prompt}"]));
    }

    #[test]
    fn env_prefix_rejects_invalid_keys() {
        // Things that LOOK like assignments but aren't valid identifiers — pass through as args
        let (env, cmd) = split_env_prefix(s(&["1FOO=bar", "claude"]));
        assert!(env.is_empty());
        assert_eq!(cmd, s(&["1FOO=bar", "claude"]));

        let (env, cmd) = split_env_prefix(s(&["FOO-BAR=baz", "claude"]));
        assert!(env.is_empty());
        assert_eq!(cmd, s(&["FOO-BAR=baz", "claude"]));
    }

    #[test]
    fn env_prefix_value_can_contain_equals_and_be_empty() {
        let (env, cmd) = split_env_prefix(s(&["URL=http://x?a=1&b=2", "EMPTY=", "cmd"]));
        assert_eq!(env, vec![
            ("URL".to_string(), "http://x?a=1&b=2".to_string()),
            ("EMPTY".to_string(), "".to_string()),
        ]);
        assert_eq!(cmd, s(&["cmd"]));
    }
}

/// Integration tests against a fake CollabApi + real subprocess shims.
///
/// Each test that spawns a subprocess uses a unique instance_id so concurrent
/// tests don't clobber each other's `/tmp/collab-debug-<instance>.txt` artifacts.
///
/// Bugs each test guards against (so we don't relive 2026-04-16):
/// - JSON-parse-fail debug dump being clobbered by unconditional cleanup
/// - Successful follow-up failing to clear stale debug files
/// - `KEY=VAL script` cli_templates spawning the literal string as a binary
/// - COLLAB_INSTANCE/SERVER/TOKEN leaking into the subprocess
/// - Spawn failures producing no diagnostic artifact
/// - CLI timeouts producing no diagnostic artifact (and not killing the child)
/// - Delegate target also receiving a duplicate `response` message
#[cfg(test)]
mod integration {
    use super::*;
    use crate::client::CollabApi;
    use async_trait::async_trait;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// Records every call made through the trait + lets tests pre-stage the
    /// canned responses for fetch_history_pub / fetch_todos.
    struct FakeApi {
        added_messages: StdMutex<Vec<(String, String)>>,
        added_todos: StdMutex<Vec<(String, String)>>,
        completed_todos: StdMutex<Vec<String>>,
        history: StdMutex<Vec<crate::client::Message>>,
        todos: StdMutex<Vec<crate::client::Todo>>,
        // Reqwest::Client is required by the trait for SSE; tests never invoke SSE
        // but the field still has to exist.
        sse_client: reqwest::Client,
    }

    impl FakeApi {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                added_messages: StdMutex::new(Vec::new()),
                added_todos: StdMutex::new(Vec::new()),
                completed_todos: StdMutex::new(Vec::new()),
                history: StdMutex::new(Vec::new()),
                todos: StdMutex::new(Vec::new()),
                sse_client: reqwest::Client::new(),
            })
        }

        fn added_messages(&self) -> Vec<(String, String)> {
            self.added_messages.lock().unwrap().clone()
        }

        fn added_todos(&self) -> Vec<(String, String)> {
            self.added_todos.lock().unwrap().clone()
        }

        fn push_history(&self, sender: &str, recipient: &str, content: &str) {
            self.history.lock().unwrap().push(crate::client::Message {
                id: format!("hist-{}", content.len()),
                hash: format!("h-{}", content.len()),
                sender: sender.into(),
                recipient: recipient.into(),
                content: content.into(),
                refs: vec![],
                timestamp: chrono::Utc::now(),
            });
        }
    }

    #[async_trait]
    impl CollabApi for FakeApi {
        async fn add_message(&self, recipient: &str, content: &str, _refs: Option<Vec<String>>) -> Result<()> {
            self.added_messages.lock().unwrap().push((recipient.into(), content.into()));
            Ok(())
        }
        async fn todo_add(&self, instance: &str, description: &str) -> Result<()> {
            self.added_todos.lock().unwrap().push((instance.into(), description.into()));
            Ok(())
        }
        async fn todo_done(&self, hash: &str) -> Result<()> {
            self.completed_todos.lock().unwrap().push(hash.into());
            Ok(())
        }
        async fn fetch_pending_messages(&self) -> Result<Vec<crate::client::Message>> { Ok(Vec::new()) }
        async fn fetch_history_pub(&self, _instance_id: &str) -> Result<Vec<crate::client::Message>> {
            Ok(self.history.lock().unwrap().clone())
        }
        async fn fetch_todos(&self, _instance: &str) -> Result<Vec<crate::client::Todo>> {
            Ok(self.todos.lock().unwrap().clone())
        }
        async fn heartbeat(&self, _role: Option<&str>) -> Result<()> { Ok(()) }
        async fn acquire_lease(&self, _pid: i64, _host: &str) -> Result<crate::client::LeaseOutcome> {
            Ok(crate::client::LeaseOutcome::Held { taken_over: false })
        }
        async fn release_lease(&self, _pid: i64) -> Result<()> { Ok(()) }
        async fn report_usage(&self, _report: &crate::client::UsageReport<'_>) -> Result<()> { Ok(()) }
        fn base_url(&self) -> &str { "http://fake" }
        fn bearer_token(&self) -> Option<&str> { None }
        fn http_client(&self) -> &reqwest::Client { &self.sse_client }
    }

    /// Globally unique-per-test instance id so concurrent tests don't share
    /// the /tmp/collab-debug-<instance>.txt artifact.
    fn unique_id(prefix: &str) -> String {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        format!("test-{}-{}-{}", prefix, std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    fn debug_path(instance_id: &str) -> String {
        format!("/tmp/collab-debug-{}.txt", instance_id)
    }

    /// Write a small bash script in `dir` and return its absolute path. The body
    /// is wrapped in a shebang and made executable.
    fn write_script(dir: &TempDir, name: &str, body: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/usr/bin/env bash\n{}\n", body)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().to_string()
    }

    fn make_harness(
        cli_template: &str,
        workdir: &Path,
        fake: Arc<dyn CollabApi>,
        instance_id: &str,
    ) -> WorkerHarness {
        WorkerHarness::new_with_api(
            fake,
            instance_id.into(),
            workdir.to_path_buf(),
            String::new(),
            Some(cli_template.into()),
            true,
            10,
            vec![],
            vec![],
        )
    }

    fn user_msg(content: &str) -> Message {
        Message {
            sender: "human".into(),
            recipient: "test-worker".into(),
            content: content.into(),
            hash: format!("hash-{}", content.len()),
            timestamp: chrono::Utc::now(),
        }
    }

    // ─── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn spawn_cli_handles_valid_json_response() {
        let id = unique_id("valid-json");
        let dir = TempDir::new().unwrap();
        let script = write_script(&dir, "ok.sh",
            r#"printf '%s' '{"response":"hello","continue":false}'"#);
        let fake = FakeApi::new();
        let harness = make_harness(&script, dir.path(), fake.clone() as Arc<dyn CollabApi>, &id);

        let did_continue = harness.spawn_cli(&[user_msg("hi")]).await.unwrap();

        assert!(!did_continue);
        assert_eq!(fake.added_messages(), vec![("human".into(), "hello".into())]);
        assert!(!Path::new(&debug_path(&id)).exists(), "no debug file on success");
    }

    /// REGRESSION: JSON-parse-fail used to write a debug dump that the unconditional
    /// success-cleanup at the end of process_messages would immediately wipe.
    /// The error log said "Raw dumped to..." but the file never existed.
    #[tokio::test]
    async fn spawn_cli_persists_debug_on_json_parse_fail() {
        let id = unique_id("json-fail");
        let dir = TempDir::new().unwrap();
        // Looks like JSON (has "response" key + brace) but unparseable
        let script = write_script(&dir, "bad.sh",
            r#"printf '%s' '{"response": "broken because no closing'"#);
        let fake = FakeApi::new();
        let harness = make_harness(&script, dir.path(), fake.clone() as Arc<dyn CollabApi>, &id);

        let _ = harness.spawn_cli(&[user_msg("hi")]).await;

        let path = debug_path(&id);
        assert!(Path::new(&path).exists(),
            "JSON-parse-fail must leave debug file — was being clobbered by cleanup");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("json-parse-failed"));
        assert!(fake.added_messages().is_empty(), "no message should be sent when JSON parse fails");
        let _ = std::fs::remove_file(&path);
    }

    /// A successful invocation following a failed one must clear the stale debug file.
    #[tokio::test]
    async fn spawn_cli_clears_stale_debug_on_subsequent_success() {
        let id = unique_id("clear-debug");
        let dir = TempDir::new().unwrap();
        let bad = write_script(&dir, "bad.sh", r#"printf '%s' '{"response": "broken'"#);
        let good = write_script(&dir, "good.sh",
            r#"printf '%s' '{"response":"ok","continue":false}'"#);
        let fake = FakeApi::new();
        let path = debug_path(&id);

        let h_bad = make_harness(&bad, dir.path(), fake.clone() as Arc<dyn CollabApi>, &id);
        let _ = h_bad.spawn_cli(&[user_msg("hi")]).await;
        assert!(Path::new(&path).exists(), "first call should leave debug");

        let h_good = make_harness(&good, dir.path(), fake.clone() as Arc<dyn CollabApi>, &id);
        let _ = h_good.spawn_cli(&[user_msg("again")]).await;
        assert!(!Path::new(&path).exists(),
            "successful follow-up should clear the stale debug file");
    }

    /// REGRESSION: cli_templates like `OLLAMA_HOST=... script` were silently failing
    /// because env-prefix wasn't being parsed — the literal string was spawned as a binary.
    #[tokio::test]
    async fn spawn_cli_applies_env_prefix() {
        let id = unique_id("env-prefix");
        let dir = TempDir::new().unwrap();
        let script = write_script(&dir, "echo-env.sh",
            r#"printf '{"response":"%s","continue":false}' "$MY_TEST_VAR""#);
        let fake = FakeApi::new();
        let template = format!("MY_TEST_VAR=hello-from-env {}", script);
        let harness = make_harness(&template, dir.path(), fake.clone() as Arc<dyn CollabApi>, &id);

        harness.spawn_cli(&[user_msg("go")]).await.unwrap();

        let sent = fake.added_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, "hello-from-env",
            "env-prefix on cli_template must reach the subprocess");
    }

    /// COLLAB_* env vars must be stripped from the subprocess. We feed them in
    /// via the cli_template's env-prefix mechanism (so the spawn sees them in
    /// its env list before strip), then assert the script doesn't see them.
    #[tokio::test]
    async fn spawn_cli_strips_collab_env_vars() {
        let id = unique_id("env-strip");
        let dir = TempDir::new().unwrap();
        let script = write_script(&dir, "echo-collab-env.sh",
            r#"printf '{"response":"INST=%s SERVER=%s TOKEN=%s","continue":false}' \
                "${COLLAB_INSTANCE:-stripped}" "${COLLAB_SERVER:-stripped}" "${COLLAB_TOKEN:-stripped}""#);
        let fake = FakeApi::new();
        // env-prefix puts them on cmd.env(); spawn_cli's env_remove must override.
        let template = format!(
            "COLLAB_INSTANCE=should-be-stripped COLLAB_SERVER=should-be-stripped COLLAB_TOKEN=should-be-stripped {}",
            script
        );
        let harness = make_harness(&template, dir.path(), fake.clone() as Arc<dyn CollabApi>, &id);

        harness.spawn_cli(&[user_msg("go")]).await.unwrap();

        let sent = fake.added_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, "INST=stripped SERVER=stripped TOKEN=stripped",
            "all three COLLAB_* vars must be stripped, even if env-prefix tries to set them");
    }

    /// REGRESSION: spawn failures used to log to stderr only; no debug file produced.
    #[tokio::test]
    async fn spawn_cli_writes_debug_on_spawn_fail() {
        let id = unique_id("spawn-fail");
        let dir = TempDir::new().unwrap();
        let fake = FakeApi::new();
        let harness = make_harness("/no/such/binary {prompt}", dir.path(), fake as Arc<dyn CollabApi>, &id);

        let result = harness.spawn_cli(&[user_msg("hi")]).await;

        assert!(result.is_err());
        let path = debug_path(&id);
        assert!(Path::new(&path).exists(), "spawn failure must produce debug file");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("spawn-failed"));
        let _ = std::fs::remove_file(&path);
    }

    /// REGRESSION: timeouts used to leave the child running and produce no diagnostic.
    /// kill_on_drop + tokio::timeout + debug write all need to compose.
    #[tokio::test]
    async fn spawn_cli_writes_debug_on_timeout_and_kills_child() {
        let id = unique_id("timeout");
        let dir = TempDir::new().unwrap();
        // Sentinel file the child writes AFTER its sleep — if it survives the
        // timeout, the file appears.
        let sentinel = dir.path().join("survived.txt");
        let script = write_script(&dir, "slow.sh",
            &format!("sleep 30 && touch {}", sentinel.display()));
        let fake = FakeApi::new();
        let harness = make_harness(&script, dir.path(), fake as Arc<dyn CollabApi>, &id)
            .with_cli_timeout_secs(1);

        let result = harness.spawn_cli(&[user_msg("hi")]).await;

        assert!(result.is_err(), "should error on timeout");
        let path = debug_path(&id);
        assert!(Path::new(&path).exists(), "timeout must produce debug file");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("timeout"));
        // Give the would-be sleep enough wall clock to have created the sentinel
        // if kill_on_drop didn't kick in; assert it never appears.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!sentinel.exists(),
            "child must be killed on timeout (kill_on_drop) — sentinel would only exist if it ran to completion");
        let _ = std::fs::remove_file(&path);
    }

/// Delegate target should not also receive a duplicate `response` message.
    /// The delegate handoff already creates a todo + notification; sending the
    /// `response` field on top would double-message them.
    #[tokio::test]
    async fn spawn_cli_dedupes_response_to_delegate_target() {
        let id = unique_id("dedupe");
        let dir = TempDir::new().unwrap();
        let json = r#"{"response":"got it","delegate":[{"to":"@human","task":"do the thing"}],"continue":false}"#;
        let script = write_script(&dir, "delegate.sh", &format!("printf '%s' '{}'", json));
        let fake = FakeApi::new();
        let harness = make_harness(&script, dir.path(), fake.clone() as Arc<dyn CollabApi>, &id);

        harness.spawn_cli(&[user_msg("please")]).await.unwrap();

        // The "response" field text should not be sent to @human as a separate message
        // because @human is already a delegate target.
        let response_dupes_to_human: Vec<_> = fake.added_messages()
            .into_iter()
            .filter(|(r, c)| r == "human" && c == "got it")
            .collect();
        assert!(response_dupes_to_human.is_empty(),
            "response field must not be sent as a separate message to a delegate target");
        // The todo itself must still be created
        assert_eq!(fake.added_todos(), vec![("human".into(), "do the thing".into())]);
    }
}
