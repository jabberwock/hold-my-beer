use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct ProjectConfig {
    #[serde(default = "default_server")]
    pub server: String,
    pub output_dir: Option<String>,
    pub workers: Vec<WorkerConfig>,
}

fn default_server() -> String {
    "http://localhost:8000".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct WorkerConfig {
    pub name: String,
    pub role: String,
    pub tasks: Option<String>,
    /// Avatar style: "neutral" | "masc" | "femme" (default: neutral)
    pub avatar: Option<String>,
    /// Accent color index 0–4: cyan, violet, emerald, amber, rose (auto-assigned if omitted)
    pub color: Option<u8>,
}

impl ProjectConfig {
    pub fn new(server: String, output_dir: Option<String>, workers: Vec<WorkerConfig>) -> Self {
        Self { server, output_dir, workers }
    }
}

pub fn run_from_yaml(yaml_path: &Path, output_dir_override: Option<&str>) -> Result<()> {
    let contents = std::fs::read_to_string(yaml_path)
        .map_err(|e| anyhow::anyhow!("Cannot read '{}': {}", yaml_path.display(), e))?;
    let config: ProjectConfig = serde_yaml::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("Invalid YAML in '{}': {}", yaml_path.display(), e))?;
    if config.workers.is_empty() {
        anyhow::bail!("No workers defined in '{}'", yaml_path.display());
    }
    println!("Loaded {} worker(s) from {}", config.workers.len(), yaml_path.display());
    generate(&config, output_dir_override)
}

pub fn generate(config: &ProjectConfig, output_dir_override: Option<&str>) -> Result<()> {
    let base_str = output_dir_override
        .map(|s| s.to_string())
        .or_else(|| config.output_dir.clone())
        .unwrap_or_else(|| ".".to_string());
    let base = Path::new(&base_str);

    println!("\nGenerating worker environments in '{}':\n", base.display());

    for worker in &config.workers {
        let dir = base.join(&worker.name);
        std::fs::create_dir_all(&dir)?;
        let md = render_claude_md(worker, &config.workers, &config.server);
        let path = dir.join("CLAUDE.md");
        std::fs::write(&path, md)?;
        println!("  ✓  {}", path.display());
    }

    // Write dashboard-config.json for avatar/color presets
    let mut entries = Vec::new();
    for (i, worker) in config.workers.iter().enumerate() {
        let color = worker.color.unwrap_or((i % 5) as u8);
        let avatar = worker.avatar.as_deref().unwrap_or("neutral");
        entries.push(format!(
            "    {}: {{\"avatar\": \"{}\", \"color\": {}}}",
            serde_json::to_string(&worker.name).unwrap(),
            avatar, color
        ));
    }
    let dashboard_cfg = format!("{{\n  \"workers\": {{\n{}\n  }}\n}}\n", entries.join(",\n"));
    let cfg_path = base.join("dashboard-config.json");
    std::fs::write(&cfg_path, dashboard_cfg)?;
    println!("  ✓  {} (import into dashboard)", cfg_path.display());

    println!("\n{} worker environment(s) created.", config.workers.len());
    println!("\nNext steps:");
    println!("  1. Start the collab server:    collab-server");
    println!("  2. Open each worker directory as a Claude Code project");
    println!("  3. Each worker's CLAUDE.md has full instructions");
    println!("  4. Import dashboard-config.json via the ⬆ button in collab-web/index.html");
    Ok(())
}

fn render_claude_md(worker: &WorkerConfig, all: &[WorkerConfig], server: &str) -> String {
    let teammates: Vec<&WorkerConfig> = all.iter().filter(|w| w.name != worker.name).collect();

    let team_table = if teammates.is_empty() {
        "_(no other workers configured)_\n".to_string()
    } else {
        let rows: String = teammates
            .iter()
            .map(|w| format!("| `{}` | {} |\n", w.name, w.role))
            .collect();
        format!("| Instance | Role |\n|----------|------|\n{}", rows)
    };

    let other = teammates.first().map(|w| w.name.as_str()).unwrap_or("teammate");

    let team_list = if teammates.is_empty() {
        "_(solo)_".to_string()
    } else {
        teammates
            .iter()
            .map(|w| format!("`{}`", w.name))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let tasks_section = match &worker.tasks {
        Some(t) => {
            // Reflow: join lines within a paragraph (single newline → space),
            // preserve blank lines as paragraph breaks.
            let reflowed = t
                .trim()
                .split("\n\n")
                .map(|para| {
                    para.lines()
                        .map(|l| l.trim())
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            format!("## Your Tasks\n\n{}\n\n", reflowed)
        }
        None => String::new(),
    };

    format!(
        r#"# {name} — Collab Worker

## Identity

You are **{name}**, a Claude Code worker instance in a multi-worker collaboration.

**Your role:** {role}

**Your teammates:** {team_list}

## Setup

Set these environment variables before running `collab` commands:

```bash
export COLLAB_INSTANCE={name}
export COLLAB_SERVER={server}
```

Or save permanently in `~/.collab.toml`:

```toml
instance = "{name}"
host = "{server}"
```

## Team

{team_table}
## Session Start

Run these in order at the start of every session:

**1. Check for pending messages and tasks:**
```bash
collab status
collab todo list
```

Pending tasks assigned to you survive context resets — they stay in your queue until you explicitly mark them done.

**2. Run the event-driven worker:**

Start the headless worker to listen for messages and respond automatically:
```bash
collab worker --instance {name}
```

The worker runs continuously and spawns Claude on demand when messages arrive. It batches rapid message bursts, auto-replies to trivial messages, and maintains state across restarts. Run this once and it will manage your message queue.

**3. Stream for the web dashboard (optional but recommended):**
```bash
collab stream --role "{role}"
```

Keeps your role visible in the roster and feeds the web dashboard.

**4. Stop condition:**

When a stop signal arrives via `collab list`, send a final summary and finish:
```bash
collab broadcast "Shutting down: <brief summary of work done>"
```

## Messaging

```bash
# Message a specific teammate
collab add @{other} "Ready to integrate — endpoint is live at /api/users"

# Broadcast to all active workers
collab broadcast "Starting schema migration — hold writes for 60s"

# Reply to the latest message from someone (auto-threads)
collab reply @{other} "Got it, will wait"

# Reply referencing a specific message hash
collab add @{other} "Fixed, commit a1b2c3d" --refs <hash>
```

{tasks_section}## Task Queue

Tasks assigned to you persist across sessions and context resets. Unlike messages, they don't expire.

```bash
collab todo list                        # your pending tasks (also shown in collab status)
collab todo done <hash>                 # mark complete when finished — do this before moving on
```

Teammates or @jabberwock assign tasks with:
```bash
collab todo add @{name} "description"
```

**Rule:** Always check `collab todo list` at session start. Mark tasks done *before* starting the next one. A task is not done until you run `collab todo done` — acknowledged ≠ complete.

**When assigning work to a teammate, always use `collab todo add` — not just a message.** Messages expire and get lost on context reset. Todos persist until marked done.

```bash
# Assign a task (use this instead of just messaging)
collab todo add @{other} "implement the /api/users endpoint"

# Then optionally send a message with context
collab add @{other} "Added a todo for you — see collab todo list for details"
```

## Rules

Follow these without exception:

1. **Run `collab status` before starting any work.** Always.

2. **Announce blockers the moment they happen.** Don't wait silently — message the relevant teammate immediately.

3. **Never idle.** When blocked:
   - Pick up another task, or
   - Broadcast asking for direction:
     ```bash
     collab broadcast "Blocked waiting on {other}. Available for other tasks."
     ```

4. **Stop cleanly when all tasks are done.** Broadcast a summary and exit:
   ```bash
   collab broadcast "Tasks complete: <brief summary of what was done>"
   ```
   Then stop. Do not loop or poll after finishing.

5. **Be specific in messages.** File paths, line numbers, commit hashes, exact errors — not vague descriptions.

6. **Finish one task before starting the next.**

7. **Acknowledge messages promptly.** Even "received, on it" keeps the team unblocked.

8. **Mask PII before sending any message.** Redact names, emails, phone numbers, addresses, IDs, and any other personal data. Use placeholders like `[NAME]`, `[EMAIL]`, `[PHONE]`, `[ADDRESS]`, `[ID]` in your messages and broadcasts.
"#,
        name = worker.name,
        role = worker.role,
        server = server,
        team_table = team_table,
        team_list = team_list,
        other = other,
        tasks_section = tasks_section,
    )
}
