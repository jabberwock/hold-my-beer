use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::fs;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, MetadataExt as _};

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(default = "default_server")]
    pub server: String,
    pub output_dir: Option<String>,
    /// Shared data root for cross-worker file exchange (SMB/NFS/Tailscale etc.).
    /// Falls back to output_dir if unset or unreachable.
    /// Structure mirrors output_dir: shared_data_dir/<worker-name>/
    pub shared_data_dir: Option<String>,
    /// Path to the shared codebase that workers will exec from (e.g., ~/code/claude-ipc)
    pub codebase_path: Option<String>,
    /// Default model for all workers — only needed if cli_template uses {model}
    #[serde(default)]
    pub model: Option<String>,
    /// CLI command template with {prompt}, {model}, {workdir} placeholders
    pub cli_template: Option<String>,
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
    /// Claude model for this worker (e.g., haiku, sonnet) — overrides project default if set
    pub model: Option<String>,
    /// CLI command template — overrides project default if set
    pub cli_template: Option<String>,
    /// Pipeline: workers to auto-dispatch to when this worker completes a task
    #[serde(default)]
    pub hands_off_to: Vec<String>,
}

impl ProjectConfig {
    pub fn new(server: String, output_dir: Option<String>, codebase_path: Option<String>, model: Option<String>, workers: Vec<WorkerConfig>) -> Self {
        Self { server, output_dir, shared_data_dir: None, codebase_path, model, cli_template: None, workers }
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
        let worker_model = worker.model.as_ref()
            .or(config.model.as_ref())
            .cloned()
            .unwrap_or_default();
        let md = render_claude_md(worker, &config.workers, &config.server, &config.codebase_path, &worker_model, &config.shared_data_dir, &base_str);
        let path = dir.join("AGENT.md");
        std::fs::write(&path, md)?;
        println!("  ✓  {}", path.display());
    }

    // Write worker manifest to .collab/workers.json in the PROJECT ROOT, not output_dir
    // This allows 'collab start all' to find it regardless of output_dir location
    let project_root = Path::new(".");
    write_worker_manifest(project_root, base, &base_str, config)?;

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
    println!("\n⚠️  If any workers are already running, restart them to pick up the new server URL ({}).", config.server);
    println!("\nNext steps:");
    println!("  1. Start the collab server:    collab-server");
    println!("  2. Open each worker directory as a Claude Code project");
    println!("  3. Each worker's AGENT.md has full instructions");
    println!("  4. Import dashboard-config.json via the ⬆ button in collab-web/index.html");
    Ok(())
}

fn render_claude_md(worker: &WorkerConfig, all: &[WorkerConfig], server: &str, codebase_path: &Option<String>, model: &str, shared_data_dir: &Option<String>, output_dir: &str) -> String {
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
            format!("## Your Tasks\n\n{}\n\n", t.trim())
        }
        None => String::new(),
    };

    // Resolve shared data root: shared_data_dir if set, else output_dir
    let data_root = shared_data_dir.as_deref().unwrap_or(output_dir);
    let sibling_dirs: String = all.iter()
        .filter(|w| w.name != worker.name)
        .map(|w| format!("  {}/{}/", data_root, w.name))
        .collect::<Vec<_>>()
        .join("\n");
    let data_section = format!(
        "## Data\n\n\
        **Check the filesystem before asking a teammate.** Large data lives on disk — \
        messages are for coordination only (\"I finished X\", \"blocked on Y\").\n\n\
        Your output directory: `{data_root}/{name}/`\n\n\
        Sibling worker data:\n{siblings}\n\n\
        If `shared_data_dir` is unreachable (e.g. network share down), fall back to \
        reading from sibling directories under `{output_dir}/`.\n\n",
        data_root = data_root,
        name = worker.name,
        siblings = if sibling_dirs.is_empty() { "  _(no other workers)_".to_string() } else { sibling_dirs },
        output_dir = output_dir,
    );

    let workdir_cmd = codebase_path
        .as_ref()
        .map(|p| format!("collab worker --workdir {} --model {}", p, model))
        .unwrap_or_else(|| format!("collab worker --workdir <path-to-shared-codebase> --model {}", model));

    format!(
        r#"# {name} — Collab Worker

## Identity

You are **{name}**, a worker instance in a multi-worker collaboration.

**Your role:** {role}

**Your teammates:** {team_list}

## Setup (COPY-PASTE THIS AT SESSION START)

Before running any `collab` commands, set these three environment variables:

```bash
export COLLAB_INSTANCE={name}
export COLLAB_SERVER={server}
export COLLAB_TOKEN="<your-token-from-human>"
```

**Do this every session.** Add to your shell profile if you want to skip it later, but start with copy-paste so you learn the three required variables.

💡 **Where to get COLLAB_TOKEN:** Ask your team lead — it's generated when the server starts. Keep it secret.

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

Start the headless worker to listen for messages and respond automatically. Run this **after** setting env vars (step 1):
```bash
{workdir_cmd}
```

This spawns your configured CLI tool on demand when messages arrive, batches rapid bursts, auto-replies to trivial messages, and maintains state across restarts. **IMPORTANT:** The worker needs:
- Your environment variables set (step 1) ✓
- Your CLI tool installed and in your PATH (configured via `cli_template` in workers.yaml)
- A working internet connection to collab server

If the worker fails silently, check `/tmp/collab-worker-errors.log` for diagnosis.

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

## Output JSON — STRICT RULES

Your final output must be ONLY a JSON object. Do NOT use `collab add`, `collab todo add`, or `collab broadcast` — the harness delivers those from your JSON output. Read commands (`collab status`, `collab todo list`, `collab whoami`) are fine if you need to verify state.

- **`response`**: Reply to the sender only if they asked a direct question. Otherwise `null`.
- **`delegate`**: Assign tasks to teammates. One entry per task. Description must be fully self-contained.
- **`messages`**: **Always `null`.** Never send status updates, confirmations, or narration.
- **`completed_tasks`**: **REQUIRED when you finish work.** Include the hash of every task you completed this turn. Never leave finished tasks open.
- **`continue`**: Set `true` to keep working autonomously (multi-step tasks), `false` when done or blocked.
- **`state_update`**: One-line status only (e.g. `{{"status": "assigned routing task to @d4-web"}}`).

{tasks_section}## Task Queue

Your pending tasks survive context resets. Check them with `collab todo list` (bash tool).

**When you finish a task, you MUST include its hash in `completed_tasks` in your JSON output.** Do not leave finished tasks open — they pile up and confuse the team. If you completed multiple tasks in one turn, list all their hashes.

{data_section}## Rules

Follow these without exception:

1. **Only act on explicit instructions.** Do not invent tasks. Only assign what you were directly told to assign.

2. **One delegate entry per task.** Never send the same task twice.

3. **`messages` is always null.** No status updates, no confirmations, no summaries. Ever.

4. **`continue` is true while you have more work to do, false when done or blocked.**

5. **`response` is null unless the sender asked you a direct question.** Do not acknowledge or summarize.

6. **Be specific when delegating.** File paths, exact requirements — not vague descriptions.

7. **Finish one task before starting the next.**

8. **Mask PII.** Redact names, emails, IDs with `[NAME]`, `[EMAIL]`, `[ID]`.
"#,
        name = worker.name,
        role = worker.role,
        server = server,
        team_table = team_table,
        team_list = team_list,
        tasks_section = tasks_section,
        workdir_cmd = workdir_cmd,
        data_section = data_section,
    )
}

use crate::lifecycle::WorkerManifestEntry;

/// Write .collab/workers.json manifest for lifecycle management
fn write_worker_manifest(project_root: &Path, output_dir: &Path, output_dir_str: &str, config: &ProjectConfig) -> Result<()> {
    let collab_dir = project_root.join(".collab");
    fs::create_dir_all(&collab_dir)?;

    let mut manifest_entries = Vec::new();
    for worker in &config.workers {
        let worker_model = worker.model.as_ref()
            .or(config.model.as_ref())
            .cloned()
            .unwrap_or_default();
        let codebase_path = config.codebase_path.as_ref()
            .map(|p| p.clone())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            });

        let cli_tmpl = Some(worker.cli_template.clone()
            .or_else(|| config.cli_template.clone())
            .unwrap_or_else(|| "{agent} -p {prompt} --model {model}".to_string()));
        manifest_entries.push(WorkerManifestEntry {
            name: worker.name.clone(),
            role: worker.role.clone(),
            codebase_path,
            model: worker_model,
            cli_template: cli_tmpl,
            output_dir: {
                let base_str = output_dir.to_string_lossy();
                let clean = base_str.strip_prefix("./").unwrap_or(&base_str);
                let rel = Path::new(clean).join(&worker.name);
                std::env::current_dir()
                    .map(|cwd| cwd.join(&rel))
                    .unwrap_or(rel)
                    .to_string_lossy().to_string()
            },
            hands_off_to: worker.hands_off_to.clone(),
            shared_data_dir: config.shared_data_dir.clone()
                .or_else(|| Some(output_dir_str.to_string())),
        });
    }

    let manifest_json = serde_json::to_string_pretty(&manifest_entries)?;
    let manifest_path = collab_dir.join("workers.json");

    fs::write(&manifest_path, manifest_json)?;

    // Set permissions to 0600 (user read/write only) — SECURITY
    #[cfg(unix)]
    {
        let perms = std::os::unix::fs::PermissionsExt::from_mode(0o600);
        fs::set_permissions(&manifest_path, perms)?;
    }

    println!("  ✓  {} (manifest for lifecycle commands)", manifest_path.display());

    Ok(())
}
