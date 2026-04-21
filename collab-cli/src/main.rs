#![doc = include_str!("../../README.md")]
use clap::{Parser, Subcommand};
use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

mod client;
mod init;
mod worker;
mod lifecycle;
mod team;
mod team_cli;
mod team_init;
#[cfg(feature = "monitor")]
mod monitor;
#[cfg(feature = "monitor")]
mod wizard;

use client::CollabClient;

#[derive(Debug, Deserialize, Default)]
struct Config {
    host: Option<String>,
    instance: Option<String>,
    token: Option<String>,
}

fn load_config() -> Config {
    let local = local_config_path().and_then(|p| read_config(&p));
    let global = config_path().and_then(|p| read_config(&p));

    match (local, global) {
        (Some(l), Some(g)) => Config {
            host: l.host.or(g.host),
            instance: l.instance.or(g.instance),
            token: l.token.or(g.token),
        },
        (Some(c), None) | (None, Some(c)) => c,
        (None, None) => Config::default(),
    }
}

fn read_config(path: &PathBuf) -> Option<Config> {
    let contents = std::fs::read_to_string(path).ok()?;
    toml::from_str::<Config>(&contents).ok()
}

/// Walk up from CWD looking for a local .collab.toml (stops before home dir).
fn local_config_path() -> Option<PathBuf> {
    let home = home_dir()?;
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(".collab.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        // Don't read the global ~/.collab.toml as a local config
        if dir == home {
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn config_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".collab.toml"))
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from).or_else(|| {
            let drive = std::env::var("HOMEDRIVE").ok()?;
            let path = std::env::var("HOMEPATH").ok()?;
            Some(PathBuf::from(format!("{}{}", drive, path)))
        })
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

/// Load a .env file by walking up from cwd (same search as .collab.toml).
/// Sets values as real environment variables so std::env::var picks them up.
///
/// Pre-set shell env vars shadow .env values (standard .env loader behaviour).
/// When we detect a shadow for any COLLAB_* key we print a warning to stderr —
/// silent shadowing is what bit the user who spent 10 minutes trying to
/// figure out why their freshly-edited `.env` wasn't being picked up.
fn load_dotenv() {
    let home = home_dir();
    let mut dir = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return,
    };
    loop {
        let candidate = dir.join(".env");
        if candidate.is_file() {
            if let Ok(contents) = std::fs::read_to_string(&candidate) {
                for line in contents.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((key, val)) = line.split_once('=') {
                        let key = key.trim();
                        let val = val.trim().trim_matches('"').trim_matches('\'');
                        match std::env::var(key) {
                            Err(_) => unsafe { std::env::set_var(key, val); },
                            Ok(existing) if existing != val && key.starts_with("COLLAB_") => {
                                // Tokens get fingerprinted (first 8 chars).
                                // Everything else — server URL, instance
                                // id — shows the full value so the human
                                // can actually tell what's what. "http://l…"
                                // is useless.
                                let is_secret =
                                    key.contains("TOKEN") || key.contains("SECRET");
                                let display = |s: &str| -> String {
                                    if is_secret {
                                        let pfx: String = s.chars().take(8).collect();
                                        format!("'{pfx}…'")
                                    } else {
                                        format!("'{s}'")
                                    }
                                };
                                eprintln!(
                                    "(warning) shell ${key}={ex} shadows {new} from {path}. To use the .env value instead: unset {key} && eval $(cat {path})",
                                    key = key,
                                    ex = display(&existing),
                                    new = display(val),
                                    path = candidate.display()
                                );
                            }
                            Ok(_) => {} // same value, no-op
                        }
                    }
                }
            }
            return;
        }
        if home.as_ref().map_or(false, |h| &dir == h) {
            return;
        }
        if !dir.pop() {
            return;
        }
    }
}

/// CLI for inter-instance communication between Claude Code workers
#[derive(Parser)]
#[command(name = "collab", version)]
#[command(about = "Collaboration tool for Claude Code instances", long_about = None)]
#[command(args_conflicts_with_subcommands = false)]
struct Cli {
    /// Server URL (overrides $COLLAB_SERVER and ~/.collab.toml)
    #[arg(long, global = true)]
    server: Option<String>,

    /// Instance identifier (overrides $COLLAB_INSTANCE and ~/.collab.toml)
    #[arg(short, long, global = true)]
    instance: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum TodoAction {
    /// Assign a task to an instance
    Add {
        /// Target instance (e.g., @worker or worker)
        #[arg(value_name = "@INSTANCE")]
        instance: String,

        /// Task description
        #[arg(value_name = "DESCRIPTION")]
        description: String,
    },

    /// List pending tasks (defaults to your own instance)
    List {
        /// Show tasks for a specific instance instead of yourself
        #[arg(value_name = "@INSTANCE")]
        instance: Option<String>,

        /// Alias: show tasks for a specific instance (e.g., --for @d4-stats)
        #[arg(long = "for", value_name = "INSTANCE")]
        for_instance: Option<String>,
    },

    /// Mark a task complete
    Done {
        /// Hash prefix of the task (at least 4 chars)
        #[arg(value_name = "HASH")]
        hash: String,
    },
}

#[derive(Subcommand)]
enum RoleAction {
    /// Print your role, tasks, and hands_off_to as declared in the manifest
    /// that governs your codebase. Resolves team.yml first (via
    /// .collab/team-managed), then falls back to workers.yml / .collab/workers.json.
    Show,

    /// Open the governing manifest in $EDITOR. After the edit closes, run
    /// `collab init <source>` to regenerate AGENT.md.
    Edit,
}

#[derive(Subcommand)]
enum TeamAction {
    /// Create a team on the server. Prints a token ONCE — distribute it to
    /// each worker's COLLAB_TOKEN env var. Requires server admin auth (the
    /// legacy COLLAB_TOKEN env var if the server is configured with one).
    Create {
        /// Team name (alphanumeric + dash/underscore, ≤64 chars)
        #[arg(value_name = "NAME")]
        name: String,
    },

    /// List teams on the server (admin).
    List,

    /// Show details for a team — ID, token count, and (if a team.yml is
    /// provided) the worker roster from that manifest.
    Show {
        /// Team name
        #[arg(value_name = "NAME")]
        name: String,

        /// Optional path to the team.yml for roster details
        #[arg(long = "from", value_name = "TEAM_YML")]
        from: Option<PathBuf>,
    },

    /// Mint a new token for a team and revoke the old one after a brief
    /// grace window. Prints the new token ONCE.
    RotateToken {
        /// Team name
        #[arg(value_name = "NAME")]
        name: String,
    },

    /// Migrate a legacy workers.yml into an existing team.yml. Reads the
    /// workers.yml, appends its workers to the team.yml (codebase_path =
    /// dirname of the workers.yml), deletes the workers.yml, writes the
    /// `.collab/team-managed` marker.
    ///
    /// By default this is a pure local-file operation. Pass `--mint-token`
    /// to also round-trip to the server and create the team there (saves
    /// you from running `collab team create` afterward).
    Adopt {
        /// Path to the legacy workers.yml to absorb
        #[arg(value_name = "WORKERS_YML")]
        workers_yml: PathBuf,

        /// Path to the team.yml that should absorb it (will be created if
        /// it doesn't exist yet)
        #[arg(value_name = "TEAM_YML")]
        team_yml: PathBuf,

        /// Also create the team on the server and mint a token in one go.
        /// Requires COLLAB_TOKEN to be set to the server's admin token.
        #[arg(long)]
        mint_token: bool,
    },
}

#[derive(Subcommand)]
enum Commands {
    /// List messages intended for this instance (unread by default)
    List {
        /// Show all messages from the last hour, not just unread
        #[arg(short, long)]
        all: bool,

        /// Only show messages from a specific sender (e.g., @kali)
        #[arg(short, long, value_name = "@INSTANCE")]
        from: Option<String>,

        /// Only show messages after the message with this hash prefix
        #[arg(long, value_name = "HASH")]
        since: Option<String>,
    },

    /// Reply to the most recent message from a sender (auto-fills --refs)
    Reply {
        /// Sender to reply to (e.g., @kali)
        #[arg(value_name = "@INSTANCE")]
        sender: String,

        /// Message content
        #[arg(value_name = "MESSAGE")]
        message: String,
    },

    /// Show a single message by hash prefix
    Show {
        /// Hash prefix of the message to display (at least 4 chars)
        #[arg(value_name = "HASH")]
        hash: String,
    },

    /// Show unread messages and roster in one command (recommended cold-start)
    Status,

    /// Send a message to another instance
    ///
    /// Old name: `add`. Still works via alias (with a deprecation hint).
    #[command(alias = "add")]
    Send {
        /// Target instance (e.g., @other_instance or other_instance)
        #[arg(value_name = "@INSTANCE")]
        recipient: String,

        /// Message content
        #[arg(value_name = "MESSAGE")]
        message: String,

        /// Reference message hash(es) - comma-separated
        #[arg(short, long, value_name = "HASH1,HASH2")]
        refs: Option<String>,
    },

    /// Send a message to all currently active workers (everyone in the roster except you)
    Broadcast {
        /// Message content
        #[arg(value_name = "MESSAGE")]
        message: String,

        /// Reference message hash(es) - comma-separated
        #[arg(short, long, value_name = "HASH1,HASH2")]
        refs: Option<String>,
    },

    /// Stream messages in real-time via SSE (zero-poll, instant delivery)
    Stream {
        /// One-line status shown in the server roster (e.g. "writing tests"). --role is a deprecated alias.
        #[arg(short, long, value_name = "DESCRIPTION", alias = "role")]
        status: Option<String>,
    },

    /// View message history including sent and received messages
    ///
    /// `log` is an alias for this command.
    #[command(alias = "log")]
    History {
        /// Filter by conversation partner (e.g., @other_instance)
        #[arg(value_name = "@INSTANCE")]
        filter: Option<String>,
    },

    /// Show the server-side roster (who's heartbeating). For local worker
    /// processes on this machine, use `collab ps`.
    Roster,

    /// Live TUI monitor showing roster and message activity (requires --features monitor)
    #[cfg(feature = "monitor")]
    Monitor {
        /// Refresh interval in seconds (default: 2)
        #[arg(short, long, default_value = "2")]
        interval: u64,
    },

    /// Print the path to the config file
    ConfigPath,

    /// Show token usage from worker invocations
    Usage,

    /// Manage persistent task queue (survives context resets)
    Todo {
        #[command(subcommand)]
        action: TodoAction,
    },

    /// Manage teams (multi-codebase worker manifests)
    Team {
        #[command(subcommand)]
        action: TeamAction,
    },

    /// Show or edit your role as declared in the governing manifest
    Role {
        #[command(subcommand)]
        action: RoleAction,
    },

    /// Show who you are to collab: instance, team, server, token fingerprint,
    /// lease status, codebase path, loaded config files. The first command
    /// every worker should run at cold start.
    Whoami,

    /// Set up worker environments from a YAML config (or interactive wizard)
    ///
    /// Example YAML:
    ///
    ///   server: http://localhost:8000
    ///   output_dir: ./workers     # optional
    ///   workers:
    ///     - name: frontend
    ///       role: "Build the React UI and manage component state"
    ///     - name: backend
    ///       role: "Implement REST API endpoints and database queries"
    Init {
        /// Path to workers YAML file (omit to launch interactive wizard)
        #[arg(value_name = "FILE")]
        file: Option<PathBuf>,

        /// Override the output directory from the YAML
        #[arg(short, long, value_name = "DIR")]
        output: Option<String>,
    },

    /// Event-driven headless worker (replaces polling)
    Worker {
        /// Project directory to run claude in (default: cwd)
        #[arg(long, value_name = "PATH")]
        workdir: Option<PathBuf>,

        /// Model to pass to claude (default: haiku)
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,

        /// CLI command template with {prompt}, {model}, {workdir} placeholders
        /// (default: "claude -p {prompt} --model {model} --allowedTools Bash,Read,Write,Edit")
        #[arg(long, value_name = "TEMPLATE")]
        cli_template: Option<String>,

        /// Enable trivial message auto-reply (default: true)
        #[arg(long)]
        auto_reply: Option<bool>,

        /// Wait this long (ms) after first message before spawning (default: 2000)
        #[arg(long, value_name = "MS")]
        batch_wait: Option<u64>,
    },

    /// Start worker process(es) in background
    Start {
        /// Which worker(s) to start: 'all' or '@name'
        #[arg(value_name = "TARGET")]
        target: String,
    },

    /// Stop running worker process(es)
    Stop {
        /// Which worker(s) to stop: 'all' or '@name'
        #[arg(value_name = "TARGET")]
        target: String,
    },

    /// Stop and restart worker process(es)
    Restart {
        /// Which worker(s) to restart: 'all' or '@name'
        #[arg(value_name = "TARGET")]
        target: String,
    },

    /// Show running local worker processes (from `.collab/workers.pids`).
    /// The old name `lifecycle-status` still works as an alias.
    #[command(alias = "lifecycle-status")]
    Ps,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present (walk up from cwd, stop before home)
    load_dotenv();

    let cli = Cli::parse();
    warn_on_deprecated_command_aliases();
    let file_config = load_config();

    // Priority: CLI flag > env var > .env file (already loaded) > config file > default
    let server = cli.server
        .or_else(|| std::env::var("COLLAB_SERVER").ok())
        .or(file_config.host.clone())
        .unwrap_or_else(|| "http://localhost:8000".to_string());

    let instance = cli.instance
        .or_else(|| std::env::var("COLLAB_INSTANCE").ok())
        .or(file_config.instance.clone());

    let token = std::env::var("COLLAB_TOKEN").ok().or(file_config.token.clone());

    if let Commands::Init { file, output } = cli.command {
        match file {
            Some(path) => {
                // Dispatch on the YAML's shape: a top-level `team:` key means
                // team.yml (multi-codebase), otherwise it's a legacy
                // workers.yml (single-codebase). Reading once and sniffing is
                // cheap and avoids silently running the wrong init code path
                // if the human passes the wrong file.
                let contents = std::fs::read_to_string(&path)
                    .map_err(|e| anyhow::anyhow!("Cannot read '{}': {}", path.display(), e))?;
                if team::yaml_is_team_config(&contents) {
                    team_init::run(&path)?;
                } else {
                    // Guard against `collab init workers.yml` being run
                    // inside a codebase that's already managed by a team —
                    // the two paths don't compose and this is how we'd get
                    // duplicate AGENT.md + PID files.
                    let cwd = std::env::current_dir().ok();
                    if let Some(cwd) = cwd {
                        if let Some(marker) = team::TeamManagedMarker::read(&cwd) {
                            anyhow::bail!(
                                "Refusing to init a workers.yml in a codebase managed by team '{}' \
                                 (source: {}). Edit the team manifest and re-run `collab init {}` instead.",
                                marker.team, marker.source, marker.source
                            );
                        }
                    }
                    init::run_from_yaml(&path, output.as_deref())?;
                }
            }
            None => {
                #[cfg(feature = "monitor")]
                {
                    match wizard::run()? {
                        Some(config) => init::generate(&config, output.as_deref())?,
                        None => println!("Wizard cancelled."),
                    }
                }
                #[cfg(not(feature = "monitor"))]
                {
                    anyhow::bail!(
                        "Interactive wizard requires the 'monitor' feature.\n\
                         Provide a YAML file instead: collab init workers.yaml"
                    );
                }
            }
        }
        return Ok(());
    }

    if let Commands::Worker { workdir, model, cli_template, auto_reply, batch_wait } = cli.command {
        let auto_reply = auto_reply.unwrap_or(true);
        let batch_wait = batch_wait.unwrap_or(2000);

        let instance_id = instance.ok_or_else(|| {
            anyhow::anyhow!(
                "Instance ID required. Set via --instance, $COLLAB_INSTANCE, or ~/.collab.toml"
            )
        })?;

        // Install a panic hook that writes to /tmp/collab-worker-errors.log.
        // Workers spawned by the GUI have stderr redirected to /dev/null
        // (see collab-gui commands::resolve_user_path + lifecycle's
        // configure_detached_stdio), so Rust's default panic output vanishes.
        // Without this hook, a panic inside the batch-processor or CLI-spawn
        // task produces a silent stall: the heartbeat keeps firing (it lives
        // in a different task) and presence says "working on msg from …"
        // forever, with no error ever surfaced. Panic details go through
        // the same log file as our log_error path so `tail` shows both.
        {
            let panicking_instance = instance_id.clone();
            std::panic::set_hook(Box::new(move |info| {
                use std::io::Write;
                let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
                let location = info.location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_else(|| "<unknown>".into());
                let payload = info.payload();
                let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "<non-string panic payload>".into()
                };
                let entry = format!(
                    "[{now}] @{panicking_instance}: PANIC at {location}: {msg}\n{:?}\n",
                    std::backtrace::Backtrace::capture(),
                );
                let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("/tmp/collab-worker-errors.log")
                    .and_then(|mut f| f.write_all(entry.as_bytes()));
                // Also print to stderr — useful when running from a terminal
                // where the default hook would've printed anyway.
                eprintln!("{entry}");
            }));
        }

        // Manifest resolution walks two paths in order:
        //   1. `.collab/team-managed` marker at cwd → load team.yml.
        //   2. `.collab/workers.json` manifest (legacy single-repo path).
        // Flags passed on the command line still win over whatever we find.
        //
        // Probe cwd for the marker (team.yml can live anywhere, but the
        // marker is always in the worker's codebase), then feed each
        // discovered value through a flag/manifest/default fallback chain
        // so `collab worker` with zero flags does the right thing.
        let probe_dir = workdir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let (reports_to, works_with, teammates, manifest_cli_template,
             manifest_codebase, manifest_model) =
            resolve_worker_manifest(&probe_dir, &instance_id);

        // Priority for each of these: CLI flag > manifest > default.
        let resolved_workdir = workdir
            .or_else(|| manifest_codebase.map(std::path::PathBuf::from))
            .unwrap_or(probe_dir);
        let resolved_model = model
            .or(manifest_model)
            .unwrap_or_default();
        let resolved_cli_template = cli_template.or(manifest_cli_template);

        let client = CollabClient::new(&server, &instance_id, token.as_deref());

        // Singleton-lease handshake: every `collab worker` claims its
        // (team, instance_id) slot on the server before spinning up the
        // harness. Two processes with the same identity can no longer
        // silently split messages — the second one exits loudly. A stale
        // lease (previous process died without releasing) gets taken over
        // automatically on the server side; we just log it here for the
        // human. See collab-server::acquire_lease for the state machine.
        let pid = std::process::id() as i64;
        let host = hostname_best_effort();
        match client.acquire_lease(pid, &host).await {
            Ok(client::LeaseOutcome::Held { taken_over }) => {
                if taken_over {
                    eprintln!(
                        "[{}] lease taken over: previous {} worker appears to have crashed without releasing",
                        chrono::Utc::now().format("%H:%M:%S UTC"),
                        &instance_id
                    );
                }
            }
            Ok(client::LeaseOutcome::Conflict { holder_pid, holder_host, seconds_since_heartbeat }) => {
                anyhow::bail!(
                    "Another worker for @{} is already running \
                     (pid={} host={}, heartbeat {}s ago). \
                     Stop it first, or use a different --instance name.",
                    instance_id, holder_pid, holder_host, seconds_since_heartbeat
                );
            }
            Err(e) => {
                // Server unreachable: fall through with a warning rather
                // than blocking the worker. The singleton guarantee only
                // matters while the server is up; if it's down, nobody
                // can compete for messages anyway.
                eprintln!(
                    "[{}] warning: lease acquire failed ({}). Starting anyway; another worker may already be running.",
                    chrono::Utc::now().format("%H:%M:%S UTC"), e
                );
            }
        }

        // Background lease heartbeat. Keeps our slot fresh while the harness
        // runs; if this task's ticks ever stop, the server evicts us after
        // LEASE_TTL_SECS so a crashed-but-unreleased lease doesn't block
        // the next run forever.
        let hb_client = client.clone();
        let hb_host = host.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            interval.tick().await; // first tick fires immediately — skip.
            loop {
                interval.tick().await;
                if let Err(e) = hb_client.acquire_lease(pid, &hb_host).await {
                    eprintln!(
                        "[{}] warning: lease heartbeat failed: {}",
                        chrono::Utc::now().format("%H:%M:%S UTC"), e
                    );
                }
            }
        });

        // Release the lease AND clear the presence row on graceful shutdown
        // (Ctrl+C / SIGTERM). Without the presence delete, `collab stop all`
        // on Unix — which uses `killpg` → SIGTERM — leaves stale presence
        // rows behind; if the GUI immediately relaunches, the preflight
        // freshness check sees "heartbeated 5 seconds ago" and refuses to
        // start the newly-spawned worker. SIGINT (Ctrl+C) hits the same
        // cleanup path.
        //
        // Best-effort: we don't wait on the HTTP calls — a slow server
        // would block exit, and stale presence also ages out naturally.
        let shutdown_client = client.clone();
        let shutdown_instance = instance_id.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            let sigterm = async {
                use tokio::signal::unix::{signal, SignalKind};
                if let Ok(mut s) = signal(SignalKind::terminate()) {
                    s.recv().await;
                }
            };
            #[cfg(not(unix))]
            let sigterm = std::future::pending::<()>();

            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = sigterm => {},
            }
            eprintln!("\n[{}] @{} releasing lease + clearing presence and exiting…",
                chrono::Utc::now().format("%H:%M:%S UTC"), shutdown_instance);
            // Fire both in parallel so a slow server doesn't serialise the delays.
            let _ = tokio::join!(
                shutdown_client.release_lease(pid),
                shutdown_client.delete_presence(),
            );
            std::process::exit(0);
        });

        let harness = worker::WorkerHarness::new(
            client,
            instance_id,
            resolved_workdir,
            resolved_model,
            resolved_cli_template,
            auto_reply,
            batch_wait,
            reports_to,
            works_with,
            teammates,
        );
        harness.run().await?;
        return Ok(());
    }

    if let Commands::Start { target } = cli.command {
        return lifecycle_start(&target, &server, token.as_deref()).await;
    }

    if let Commands::Stop { target } = cli.command {
        return lifecycle_stop(&target, &server, token.as_deref()).await;
    }

    if let Commands::Restart { target } = cli.command {
        return lifecycle_restart(&target, &server, token.as_deref()).await;
    }

    if matches!(cli.command, Commands::Ps) {
        return lifecycle_status().await;
    }

    if matches!(cli.command, Commands::Roster) {
        let client = CollabClient::new(&server, "", token.as_deref());
        client.show_roster().await?;
        return Ok(());
    }

    if matches!(cli.command, Commands::ConfigPath) {
        if let Some(local) = local_config_path() {
            println!("local:  {}", local.display());
        }
        match config_path() {
            Some(path) => println!("global: {}", path.display()),
            None => println!("Could not determine home directory"),
        }
        return Ok(());
    }

    if let Commands::Team { action } = cli.command {
        // Admin commands prefer COLLAB_ADMIN_TOKEN (unambiguous) and fall
        // back to COLLAB_TOKEN (back-compat). This is the fix for the
        // "clobbered my admin token with a team token" trap.
        let admin_tok = std::env::var("COLLAB_ADMIN_TOKEN").ok().or_else(|| token.clone());
        return dispatch_team_command(action, &server, admin_tok.as_deref()).await;
    }

    if matches!(cli.command, Commands::Whoami) {
        return dispatch_whoami(&server, instance.as_deref(), token.as_deref()).await;
    }

    if let Commands::Role { action } = cli.command {
        return dispatch_role(action, instance.as_deref());
    }

    if matches!(cli.command, Commands::Usage) {
        let client = CollabClient::new(&server, "", token.as_deref());
        let usage = match client.fetch_usage().await {
            Ok(u) => u,
            Err(e) => {
                eprintln!("Failed to fetch usage from {}: {}", server, e);
                return Ok(());
            }
        };

        if usage.workers.is_empty() {
            println!("No usage data yet. Workers report to the server after each invocation.");
            return Ok(());
        }

        let fmt_time = |secs: u64| -> String {
            let h = secs / 3600;
            let m = (secs % 3600) / 60;
            let s = secs % 60;
            if h > 0 { format!("{:>2}:{:02}:{:02}", h, m, s) }
            else { format!("   {:02}:{:02}", m, s) }
        };

        let any_cost = usage.total_cost_usd > 0.0;
        let header = if any_cost { "Token usage (actual)\n" } else { "Token usage (estimated ~4 chars/token)\n" };
        println!("{}", header);

        // Todo counts per worker — same call pattern the old path used.
        let mut todo_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for row in &usage.workers {
            if let Ok(todos) = client.fetch_todos(&row.worker).await {
                todo_counts.insert(row.worker.clone(), todos.len());
            }
        }
        let total_todos: usize = todo_counts.values().sum();

        let cost_col = if any_cost { "  Cost" } else { "" };
        println!("{:<20} {:>8} {:>8} {:>6} {:>8}  {:<10} {:<10} {:<6}{}", "Worker", "Input", "Output", "Calls", "Time", "CLI", "Tiers", "Todos", cost_col);
        println!("{}", "─".repeat(if any_cost { 96 } else { 88 }));

        for row in &usage.workers {
            let tier_str = format!("{}F/{}L", row.full_calls, row.light_calls);
            let todo_str = match todo_counts.get(&row.worker) {
                Some(0) => "—".to_string(),
                Some(n) => format!("{}", n),
                None => "?".to_string(),
            };
            let cost_str = if any_cost { format!("  ${:.4}", row.cost_usd) } else { String::new() };
            let cli_name = if row.cli.is_empty() { "?" } else { row.cli.as_str() };
            println!(
                "{:<20} {:>7}K {:>7}K {:>6} {:>8}  {:<10} {:<10} {:<6}{}",
                row.worker,
                row.input_tokens / 1000,
                row.output_tokens / 1000,
                row.calls,
                fmt_time(row.duration_secs),
                cli_name,
                tier_str,
                todo_str,
                cost_str,
            );
        }

        println!("{}", "─".repeat(if any_cost { 96 } else { 88 }));
        let total_tier_str = format!("{}F/{}L", usage.total_full_calls, usage.total_light_calls);
        let total_todo_str = if total_todos > 0 { format!("{}", total_todos) } else { "—".to_string() };
        let total_cost_str = if any_cost { format!("  ${:.4}", usage.total_cost_usd) } else { String::new() };
        println!(
            "{:<20} {:>7}K {:>7}K {:>6} {:>8}  {:<10} {:<10} {:<6}{}",
            "TOTAL",
            usage.total_input_tokens / 1000,
            usage.total_output_tokens / 1000,
            usage.total_calls,
            fmt_time(usage.total_duration_secs),
            "",
            total_tier_str,
            total_todo_str,
            total_cost_str,
        );

        return Ok(());
    }

    let instance_id = instance.ok_or_else(|| {
        anyhow::anyhow!(
            "Instance ID required. Set via --instance, $COLLAB_INSTANCE, or ~/.collab.toml\n\
             \n\
             Example ~/.collab.toml:\n\
             host = \"http://localhost:8000\"\n\
             instance = \"worker1\""
        )
    })?;

    let client = CollabClient::new(&server, &instance_id, token.as_deref());

    // Update presence on every command so the roster stays current even without `watch`.
    // Ignore errors — if the server is unreachable the command itself will surface that.
    let _ = client.heartbeat(None).await;

    match cli.command {
        Commands::List { all, from, since } => {
            client.list_messages(!all, from.as_deref(), since.as_deref()).await?;
        }
        Commands::Reply { sender, message } => {
            client.reply_to_latest(&sender, &message).await?;
        }
        Commands::Show { hash } => {
            client.show_message(&hash).await?;
        }
        Commands::Status => {
            client.show_status().await?;
        }
        Commands::Send { recipient, message, refs } => {
            let recipient = recipient.trim_start_matches('@');
            let ref_hashes = refs.map(|r| {
                r.split(',').map(|s| s.trim().to_string()).collect()
            });
            client.add_message(recipient, &message, ref_hashes).await?;
        }
        Commands::Stream { status } => {
            client.stream_messages(status).await?;
        }
        Commands::Broadcast { message, refs } => {
            let ref_hashes = refs.map(|r| {
                r.split(',').map(|s| s.trim().to_string()).collect()
            });
            client.broadcast(&message, ref_hashes).await?;
        }
        Commands::History { filter } => {
            let filter_id = filter.as_deref().map(|s| s.trim_start_matches('@'));
            client.show_history(filter_id).await?;
        }
        Commands::Todo { action } => match action {
            TodoAction::Add { instance, description } => {
                let instance = instance.trim_start_matches('@');
                client.todo_add(instance, &description).await?;
            }
            TodoAction::List { instance, for_instance } => {
                let target = for_instance.as_deref().or(instance.as_deref());
                let target = target.map(|s| s.trim_start_matches('@'));
                client.todo_list(target).await?;
            }
            TodoAction::Done { hash } => {
                client.todo_done(&hash).await?;
            }
        },
        #[cfg(feature = "monitor")]
        Commands::Monitor { interval } => {
            let server2 = server.clone();
            let instance2 = instance_id.clone();
            let token2 = token.clone();
            std::thread::spawn(move || {
                monitor::run(&server2, &instance2, interval, token2.as_deref())
            })
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("monitor panicked")))?;
        }
        Commands::Roster | Commands::ConfigPath | Commands::Usage | Commands::Init { .. }
        | Commands::Start { .. } | Commands::Stop { .. } | Commands::Restart { .. }
        | Commands::Ps | Commands::Team { .. } | Commands::Whoami
        | Commands::Role { .. } => unreachable!(),
        #[allow(unreachable_patterns)]
        #[allow(unreachable_patterns)]
        _ => unreachable!(),
    }

    Ok(())
}

/// SECURITY: Parse target string, preventing injection
fn parse_target(target: &str) -> Result<Vec<String>> {
    let target = target.trim();
    if target == "all" {
        // Will be expanded using manifest
        Ok(vec!["all".to_string()])
    } else if target.starts_with('@') {
        // Single instance
        let name = &target[1..];
        if name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            Ok(vec![name.to_string()])
        } else {
            Err(anyhow::anyhow!("Invalid instance name: {}", name))
        }
    } else if target.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        // Instance name without @
        Ok(vec![target.to_string()])
    } else {
        Err(anyhow::anyhow!("Invalid target: {}", target))
    }
}

async fn lifecycle_start(target: &str, server: &str, token: Option<&str>) -> Result<()> {
    let targets = parse_target(target)?;
    let (manifest, pids_file) = load_lifecycle_manifest()?;

    // Clean up stale PIDs — remove entries for processes that are no longer alive
    if pids_file.exists() {
        let content = std::fs::read_to_string(&pids_file)?;
        let state: std::collections::HashMap<String, lifecycle::WorkerState> =
            serde_json::from_str(&content).unwrap_or_default();
        for (name, ws) in &state {
            if !lifecycle::process_exists(ws.pid) {
                println!("⚠ Cleaning up stale PID for {} (PID {} no longer running)", name, ws.pid);
                lifecycle::remove_worker_pid(&pids_file, name)?;
            }
        }
    }

    // Determine which workers to start
    let workers = if targets[0] == "all" {
        manifest.clone()
    } else {
        manifest.into_iter()
            .filter(|w| targets.contains(&w.name))
            .collect()
    };

    if workers.is_empty() {
        println!("No matching workers found");
        return Ok(());
    }

    // Pre-flight: ask the server whether any of these workers are already
    // heartbeating a lease. If so, we refuse to spawn — the server's lease
    // endpoint would reject them anyway, but catching it here means we
    // don't even spawn the child process (no CLI burn, no PID file churn).
    let preflight_client = reqwest::Client::new();
    let roster_url = format!("{}/roster", server.trim_end_matches('/'));
    let mut req = preflight_client.get(&roster_url);
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }
    let live_roster: Vec<serde_json::Value> = match req.send().await {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => Vec::new(), // server unreachable — skip pre-flight, let the server-side lease catch it
    };
    let live_now = chrono::Utc::now();
    // Workers heartbeat every 10s (see worker.rs HEARTBEAT_INTERVAL_SECS).
    // 30s = 3 missed heartbeats → confidently dead, and tight enough that a
    // Cmd+Q-then-relaunch cycle doesn't trip the guard on presence rows that
    // ungracefully-killed workers left behind.
    const LIVENESS_WINDOW_SECS: i64 = 30;
    let fresh_set: std::collections::HashSet<String> = live_roster
        .into_iter()
        .filter_map(|v| {
            let id = v.get("instance_id")?.as_str()?.to_string();
            let last = v.get("last_seen")?.as_str()?;
            let parsed = chrono::DateTime::parse_from_rfc3339(last).ok()?;
            let delta = (live_now - parsed.with_timezone(&chrono::Utc)).num_seconds();
            if delta < LIVENESS_WINDOW_SECS { Some(id) } else { None }
        })
        .collect();

    let mut blocked: Vec<String> = workers
        .iter()
        .filter(|w| fresh_set.contains(&w.name))
        .map(|w| w.name.clone())
        .collect();
    blocked.sort();
    blocked.dedup();
    if !blocked.is_empty() {
        anyhow::bail!(
            "Refusing to start — these workers are already heartbeating on the server: {}. \
             Run `collab stop {}` first, or start workers individually by name.",
            blocked.iter().map(|n| format!("@{}", n)).collect::<Vec<_>>().join(", "),
            blocked[0]
        );
    }

    for worker in workers {
        let workdir = std::path::PathBuf::from(&worker.output_dir);
        let child = lifecycle::spawn_worker(
            &worker.name,
            &workdir,
            &worker.model,
            &worker.name,
            server,
            token,
            worker.cli_template.as_deref(),
        )?;

        let pid = child.id();
        let mut cmd = format!("collab worker --workdir {} --model {}", worker.output_dir, worker.model);
        if let Some(tmpl) = &worker.cli_template {
            cmd.push_str(&format!(" --cli-template {:?}", tmpl));
        }
        lifecycle::save_worker_pid(&pids_file, &worker.name, pid, &cmd)?;

        // Detach the child process
        std::mem::drop(child);
    }

    println!("✓ Workers started. Check status with: collab ps");
    Ok(())
}

async fn lifecycle_stop(target: &str, server: &str, token: Option<&str>) -> Result<()> {
    let targets = parse_target(target)?;
    let (_manifest, pids_file) = load_lifecycle_manifest()?;

    // Read current PIDs
    let mut state: std::collections::HashMap<String, lifecycle::WorkerState> = if pids_file.exists() {
        let content = std::fs::read_to_string(&pids_file)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        println!("No running workers found");
        return Ok(());
    };

    // Determine which workers to stop
    let workers_to_stop: Vec<String> = if targets[0] == "all" {
        state.keys().cloned().collect()
    } else {
        targets.iter()
            .filter(|t| state.contains_key(*t))
            .cloned()
            .collect()
    };

    if workers_to_stop.is_empty() {
        println!("No matching running workers found");
        return Ok(());
    }

    for name in &workers_to_stop {
        if let Some(worker_state) = state.remove(name) {
            lifecycle::kill_process(worker_state.pid, name)?;
            lifecycle::remove_worker_pid(&pids_file, name)?;
        }
    }

    println!("✓ Workers stopped");
    Ok(())
}

async fn lifecycle_restart(target: &str, server: &str, token: Option<&str>) -> Result<()> {
    lifecycle_stop(target, server, token).await?;
    std::thread::sleep(std::time::Duration::from_millis(500));
    lifecycle_start(target, server, token).await?;
    Ok(())
}

async fn lifecycle_status() -> Result<()> {
    let (_manifest, pids_file) = load_lifecycle_manifest()?;

    if !pids_file.exists() {
        println!("No workers running");
        return Ok(());
    }

    let content = std::fs::read_to_string(&pids_file)?;
    let state: std::collections::HashMap<String, lifecycle::WorkerState> = serde_json::from_str(&content)?;

    println!("Running workers:");
    for (name, worker_state) in &state {
        println!("  {} (PID: {})", name, worker_state.pid);
        println!("    Started: {}", worker_state.started_at);
        println!("    Command: {}", worker_state.command);
    }

    Ok(())
}

pub fn find_collab_dir_from(start: &std::path::Path) -> Option<std::path::PathBuf> {
    // Walk upward from `start` looking for a `.collab/workers.json` manifest.
    // Returns the `.collab` directory itself.
    let mut current = start.to_path_buf();
    loop {
        let collab = current.join(".collab");
        if collab.join("workers.json").exists() {
            return Some(collab);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn find_manifest() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir()?;
    if let Some(dir) = find_collab_dir_from(&cwd) {
        return Ok(dir.join("workers.json"));
    }

    // The "no manifest" path is where users hit the confusing error. Sniff
    // the cwd for a team.yml — if one's sitting right here, the human
    // probably just forgot to run `collab init` on it. Same for workers.yml.
    let team_yml = cwd.join("team.yml");
    if team_yml.is_file() {
        anyhow::bail!(
            "No manifest found, but there's a team.yml here. Run this first:\n\
             \n    collab init {}\n\n\
             That writes AGENT.md + .collab/team-managed markers into each \
             worker's codebase_path, which is what `collab start` / `ps` / \
             `stop` look for.",
            team_yml.display()
        );
    }
    let workers_yml = cwd.join("workers.yml");
    if workers_yml.is_file() {
        anyhow::bail!(
            "No manifest found, but there's a workers.yml here. Run:\n\
             \n    collab init {}\n",
            workers_yml.display()
        );
    }
    anyhow::bail!(
        "Manifest not found. Either:\n\
         - run `collab init <path>/team.yml`  (multi-codebase team)\n\
         - run `collab init <path>/workers.yml`  (single-repo, legacy)\n\
         from the directory that contains the manifest."
    )
}

/// Unified manifest loader for `collab start` / `stop` / `ps`. Tries two
/// sources in order:
///   1. `.collab/team-managed` marker (new team.yml world). We walk UP from
///      cwd looking for the marker so the command works from any worker's
///      codebase or the directory containing team.yml.
///   2. `.collab/workers.json` (legacy single-repo).
///
/// Returns the list of WorkerManifestEntry rows to iterate + the path where
/// the workers.pids file should live. For team mode the pids file goes in
/// `~/.collab/teams/<team>/workers.pids` so stop/ps find it regardless of
/// which codebase you run them from.
fn load_lifecycle_manifest() -> Result<(Vec<lifecycle::WorkerManifestEntry>, std::path::PathBuf)> {
    if let Some((cfg, _source, marker_dir)) = find_team_config_walking_up()? {
        let pids_file = team_pids_file_path(&cfg.team)?;
        let entries: Vec<lifecycle::WorkerManifestEntry> = cfg
            .workers
            .iter()
            .map(|w| {
                let cli_tmpl = cfg.resolved_cli_template(w);
                let model = cfg.resolved_model(w).unwrap_or_default();
                lifecycle::WorkerManifestEntry {
                    name: w.name.clone(),
                    role: w.role.clone(),
                    codebase_path: w.codebase_path.clone(),
                    model,
                    // output_dir for team.yml = the codebase_path itself.
                    // collab worker's AGENT.md lives at codebase_path/<name>/,
                    // but the worker's workdir is codebase_path.
                    output_dir: w.codebase_path.clone(),
                    shared_data_dir: cfg.shared_data_dir.clone(),
                    cli_template: cli_tmpl,
                    hands_off_to: w.hands_off_to.clone(),
                }
            })
            .collect();
        let _ = marker_dir; // future: per-codebase override for shared_data_dir
        return Ok((entries, pids_file));
    }

    // Legacy path: .collab/workers.json next to cwd or an ancestor.
    let manifest_path = find_manifest()?;
    let pids_file = manifest_path.parent().unwrap().join("workers.pids");
    let manifest = lifecycle::read_manifest(&manifest_path)?;
    Ok((manifest, pids_file))
}

/// Walk UP from cwd looking for a `.collab/team-managed` marker. Returns
/// the loaded TeamConfig + the marker's source path + the codebase dir
/// that held the marker.
fn find_team_config_walking_up() -> Result<Option<(team::TeamConfig, std::path::PathBuf, std::path::PathBuf)>> {
    let cwd = std::env::current_dir()?;

    // First: walk up looking for a marker. That's the canonical case —
    // `collab worker`/`start`/`stop` typically run from inside a
    // team-managed codebase.
    let mut dir = cwd.clone();
    loop {
        if let Some(marker) = team::TeamManagedMarker::read(&dir) {
            let source = std::path::PathBuf::from(&marker.source);
            let cfg = team::TeamConfig::from_yaml_file(&source)
                .map_err(|e| anyhow::anyhow!("loading team manifest {}: {}", source.display(), e))?;
            return Ok(Some((cfg, source, dir)));
        }
        if !dir.pop() {
            break;
        }
    }

    // Second: a team.yml sitting directly in cwd. This is the "I run
    // `collab start all` from the folder that holds my team.yml" case —
    // fine, and previously produced a misleading "Manifest not found"
    // error. Accept it as an authoritative team manifest even without a
    // marker — no codebase is being managed from this folder, but we can
    // still drive the pipeline from here.
    let team_yml = cwd.join("team.yml");
    if team_yml.is_file() {
        let cfg = team::TeamConfig::from_yaml_file(&team_yml)
            .map_err(|e| anyhow::anyhow!("loading team manifest {}: {}", team_yml.display(), e))?;
        return Ok(Some((cfg, team_yml, cwd)));
    }

    Ok(None)
}

/// Per-team pids file location. Lives under $HOME/.collab/teams/<name>/
/// so start/stop/ps always find the same file regardless of which
/// codebase the human is cd'd into.
fn team_pids_file_path(team_name: &str) -> Result<std::path::PathBuf> {
    let home = home_dir().ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
    let dir = home.join(".collab").join("teams").join(team_name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("workers.pids"))
}

/// Emit a one-line deprecation notice to stderr when the user typed an old
/// command/flag name that we still honor via alias. Silent when they used
/// the current name. Kept non-fatal so scripts keep working — we just tell
/// humans + LLMs to migrate.
fn warn_on_deprecated_command_aliases() {
    let args: Vec<String> = std::env::args().collect();
    // Subcommand is the first non-global-flag positional. We scan in a way
    // that doesn't care about argument order: if any arg matches a known
    // deprecated name, we warn. Overzealous in edge cases (a message body
    // that literally contains "add" won't trigger because we only match
    // whole arg equality), but good enough for the common case.
    for arg in &args[1..] {
        match arg.as_str() {
            "add" => {
                eprintln!("(deprecation) `collab add` is now `collab send`. The old name still works.");
                return;
            }
            "lifecycle-status" => {
                eprintln!("(deprecation) `collab lifecycle-status` is now `collab ps`. The old name still works.");
                return;
            }
            "--role" if args.iter().any(|a| a == "stream") => {
                eprintln!("(deprecation) `collab stream --role` is now `--status`. The old flag still works.");
                return;
            }
            _ => {}
        }
    }
}

async fn dispatch_team_command(
    action: TeamAction,
    server: &str,
    admin_token: Option<&str>,
) -> Result<()> {
    match action {
        TeamAction::Create { name } => team_cli::create(server, admin_token, &name).await,
        TeamAction::List => team_cli::list(server, admin_token).await,
        TeamAction::Show { name, from } => {
            team_cli::show(server, admin_token, &name, from.as_deref()).await
        }
        TeamAction::RotateToken { name } => team_cli::rotate_token(server, admin_token, &name).await,
        TeamAction::Adopt { workers_yml, team_yml, mint_token } => {
            if mint_token {
                team_cli::adopt_with_token_mint(&workers_yml, &team_yml, server, admin_token).await
            } else {
                team_cli::adopt(&workers_yml, &team_yml)
            }
        }
    }
}

async fn dispatch_whoami(
    server: &str,
    instance: Option<&str>,
    token: Option<&str>,
) -> Result<()> {
    // Instance + team discovery. "collab whoami" is the first thing a cold-
    // start worker (human or LLM) runs, so we do our best to show something
    // useful even when things are misconfigured — noting the missing pieces
    // instead of bailing on the first unset var.
    let cwd = std::env::current_dir().ok();
    let marker = cwd.as_ref().and_then(|p| team::TeamManagedMarker::read(p));

    println!("collab whoami");
    println!("  server:     {}", server);
    println!("  instance:   {}", instance.unwrap_or("<unset>"));

    match &marker {
        Some(m) => {
            println!("  team:       {}", m.team);
            println!("  team.yml:   {}", m.source);
        }
        None => println!("  team:       <legacy> (no .collab/team-managed marker in this repo)"),
    }

    // Token with kind detection. Prefixes were introduced exactly so the
    // human can tell admin from team at a glance, but old tokens may
    // predate prefixes so we don't assert on it.
    let admin_token_env = std::env::var("COLLAB_ADMIN_TOKEN").ok();
    match (&admin_token_env, token) {
        (Some(t), _) => {
            let fp: String = t.chars().take(8).collect();
            println!("  token:      {}… (COLLAB_ADMIN_TOKEN — admin secret)", fp);
        }
        (None, Some(t)) => {
            let fp: String = t.chars().take(8).collect();
            let kind = if t.starts_with("tm_") {
                "team token"
            } else if t.starts_with("adm_") {
                "admin token"
            } else {
                "token (unknown kind — pre-prefix)"
            };
            println!("  token:      {}… ({}) ", fp, kind);
        }
        (None, None) => println!("  token:      <unset>"),
    }

    // Probe the server with whatever token we have. We hit /roster (cheap,
    // team-scoped) and /admin/teams (admin-only). That tells the human
    // three things in one go: is the token valid at all, is it a team
    // token or admin token, and can they do admin ops.
    let probe_token = admin_token_env.as_deref().or(token);
    if let Some(t) = probe_token {
        let client = reqwest::Client::new();
        let roster = client
            .get(format!("{}/roster", server.trim_end_matches('/')))
            .header("Authorization", format!("Bearer {}", t))
            .send()
            .await;
        let admin_probe = client
            .get(format!("{}/admin/teams", server.trim_end_matches('/')))
            .header("Authorization", format!("Bearer {}", t))
            .send()
            .await;
        let roster_ok = matches!(&roster, Ok(r) if r.status().is_success());
        let admin_ok = matches!(&admin_probe, Ok(r) if r.status().is_success());
        match (roster_ok, admin_ok) {
            (true, true) => println!("  auth:       OK (admin — can create/rotate teams)"),
            (true, false) => println!("  auth:       OK (team member — cannot admin)"),
            (false, _) => {
                let status_hint = roster
                    .as_ref()
                    .map(|r| format!("HTTP {}", r.status()))
                    .unwrap_or_else(|e| format!("{}", e));
                println!("  auth:       FAILED ({})", status_hint);
            }
        }
    } else {
        println!("  auth:       <skipped — no token set>");
    }

    if let Some(cwd) = cwd {
        println!("  codebase:   {}", cwd.display());
    }
    if let Some(path) = local_config_path() {
        println!("  local cfg:  {}", path.display());
    }
    if let Some(path) = config_path() {
        if path.exists() {
            println!("  global cfg: {}", path.display());
        }
    }

    Ok(())
}

/// Dispatcher for `collab role show|edit`. Resolves the governing manifest
/// (team.yml via marker → workers.yml as fallback) and either prints the
/// worker's entry or opens the source file in $EDITOR. Deliberately NO
/// write semantics here — `edit` is a pure file-opener, and the human
/// re-runs `collab init` afterwards to regenerate AGENT.md. Keeping the
/// edit → regenerate step explicit so the tool never regenerates an
/// AGENT.md off a half-saved file.
fn dispatch_role(action: RoleAction, instance: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let governing = find_governing_manifest(&cwd)?;

    match action {
        RoleAction::Show => {
            let instance = instance.ok_or_else(|| anyhow::anyhow!(
                "Set $COLLAB_INSTANCE (or --instance) so we know which role entry to print"
            ))?;
            match &governing {
                GoverningManifest::Team(cfg, source) => {
                    let me = cfg.workers.iter().find(|w| w.name == instance).ok_or_else(|| {
                        anyhow::anyhow!(
                            "No entry for @{} in {}. Add it or check $COLLAB_INSTANCE.",
                            instance, source.display()
                        )
                    })?;
                    println!("role (from team.yml: {})", source.display());
                    println!("  team:            {}", cfg.team);
                    println!("  name:            {}", me.name);
                    println!("  role:            {}", me.role);
                    println!("  codebase_path:   {}", me.codebase_path);
                    if let Some(m) = cfg.resolved_model(me) {
                        println!("  model:           {}", m);
                    }
                    if let Some(t) = cfg.resolved_cli_template(me) {
                        println!("  cli_template:    {}", t);
                    }
                    if let Some(rt) = &me.reports_to {
                        println!("  reports_to:      @{}", rt);
                    }
                    if !me.works_with.is_empty() {
                        println!(
                            "  works_with:      {}",
                            me.works_with.iter().map(|n| format!("@{}", n)).collect::<Vec<_>>().join(", ")
                        );
                    }
                    if let Some(tasks) = &me.tasks {
                        println!("\ntasks:\n{}", tasks.trim());
                    }
                }
                GoverningManifest::Legacy(source) => {
                    let contents = std::fs::read_to_string(source)?;
                    let cfg: init::ProjectConfig = serde_yaml::from_str(&contents)
                        .map_err(|e| anyhow::anyhow!("parsing {}: {}", source.display(), e))?;
                    let me = cfg.workers.iter().find(|w| w.name == instance).ok_or_else(|| {
                        anyhow::anyhow!(
                            "No entry for @{} in {}.", instance, source.display()
                        )
                    })?;
                    println!("role (from workers.yml: {})", source.display());
                    println!("  name:            {}", me.name);
                    println!("  role:            {}", me.role);
                    if let Some(m) = me.model.as_deref().or(cfg.model.as_deref()) {
                        println!("  model:           {}", m);
                    }
                    if let Some(t) = me.cli_template.as_deref().or(cfg.cli_template.as_deref()) {
                        println!("  cli_template:    {}", t);
                    }
                    if !me.hands_off_to.is_empty() {
                        println!(
                            "  hands_off_to:    {}",
                            me.hands_off_to.iter().map(|n| format!("@{}", n)).collect::<Vec<_>>().join(", ")
                        );
                    }
                    if let Some(tasks) = &me.tasks {
                        println!("\ntasks:\n{}", tasks.trim());
                    }
                }
                GoverningManifest::None => {
                    anyhow::bail!(
                        "No governing manifest found. Looked for .collab/team-managed and workers.yml. \
                         Run `collab init` against a team.yml or workers.yml first."
                    );
                }
            }
        }
        RoleAction::Edit => {
            let source = match &governing {
                GoverningManifest::Team(_, path) | GoverningManifest::Legacy(path) => path.clone(),
                GoverningManifest::None => anyhow::bail!(
                    "Nothing to edit: no team.yml or workers.yml governs this codebase."
                ),
            };
            let editor = std::env::var("EDITOR")
                .or_else(|_| std::env::var("VISUAL"))
                .unwrap_or_else(|_| "vi".to_string());
            println!("Opening {} in {} …", source.display(), editor);
            let status = std::process::Command::new(&editor)
                .arg(&source)
                .status()
                .map_err(|e| anyhow::anyhow!("spawning {}: {}", editor, e))?;
            if !status.success() {
                anyhow::bail!("{} exited with non-success status", editor);
            }
            println!();
            println!("✓ Saved. To apply changes, run:");
            println!("     collab init {}", source.display());
            println!("   (then restart affected workers if their role/cli_template changed)");
        }
    }
    Ok(())
}

enum GoverningManifest {
    Team(team::TeamConfig, std::path::PathBuf),
    Legacy(std::path::PathBuf),
    None,
}

/// Walk up from `from` looking for whichever manifest governs this codebase.
/// Team marker wins (even if a stray workers.yml also exists — the mutex
/// should have caught that at init time, but defense-in-depth).
fn find_governing_manifest(from: &std::path::Path) -> Result<GoverningManifest> {
    if let Some(marker) = team::TeamManagedMarker::read(from) {
        let source = std::path::PathBuf::from(&marker.source);
        let cfg = team::TeamConfig::from_yaml_file(&source)
            .map_err(|e| anyhow::anyhow!("loading team.yml at {}: {}", source.display(), e))?;
        return Ok(GoverningManifest::Team(cfg, source));
    }
    // Fallback: look for workers.yml in cwd (not walking up — legacy mode
    // typically has workers.yml at the repo root and the human runs from
    // there).
    let workers_yml = from.join("workers.yml");
    if workers_yml.exists() {
        return Ok(GoverningManifest::Legacy(workers_yml));
    }
    Ok(GoverningManifest::None)
}

/// Best-effort hostname for lease diagnostics. Not security-sensitive —
/// it's only ever displayed back to the human so they know *which*
/// machine holds a conflicting lease. Falls back to "unknown" if the
/// platform doesn't expose a hostname cheaply.
fn hostname_best_effort() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.is_empty() {
            return h;
        }
    }
    #[cfg(unix)]
    {
        if let Ok(out) = std::process::Command::new("hostname").output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
        }
    }
    "unknown".to_string()
}

/// Resolved manifest bits for `collab worker`. `codebase_path` and `model`
/// are only populated when a manifest is present and has an entry matching
/// `instance_id` — the caller applies the flag-or-manifest fallback.
struct WorkerManifestLookup {
    reports_to: Option<String>,
    works_with: Vec<String>,
    teammates: Vec<(String, String)>,
    cli_template: Option<String>,
    codebase_path: Option<String>,
    model: Option<String>,
}

/// Resolve the worker's manifest bits from whichever source applies.
/// Team-managed codebases (team.yml) take precedence over legacy
/// `.collab/workers.json`. Returns empty defaults if neither is found —
/// the worker still runs, just without teammate awareness (which will
/// get every delegate caught by the hallucination guard).
fn resolve_worker_manifest(
    workdir: &std::path::Path,
    instance_id: &str,
) -> (Option<String>, Vec<String>, Vec<(String, String)>, Option<String>,
      Option<String>, Option<String>) {
    let lookup = resolve_worker_manifest_inner(workdir, instance_id);
    (
        lookup.reports_to,
        lookup.works_with,
        lookup.teammates,
        lookup.cli_template,
        lookup.codebase_path,
        lookup.model,
    )
}

fn resolve_worker_manifest_inner(
    workdir: &std::path::Path,
    instance_id: &str,
) -> WorkerManifestLookup {
    // First choice: team.yml via the .collab/team-managed marker.
    if let Some(marker) = team::TeamManagedMarker::read(workdir) {
        let yaml_path = std::path::PathBuf::from(&marker.source);
        match team::TeamConfig::from_yaml_file(&yaml_path) {
            Ok(cfg) => {
                let me = cfg.workers.iter().find(|w| w.name == instance_id);
                let reports_to = me.and_then(|w| w.reports_to.clone());
                let works_with = me.map(|w| w.works_with.clone()).unwrap_or_default();
                let tmpl = me.and_then(|w| cfg.resolved_cli_template(w));
                let codebase = me.map(|w| w.codebase_path.clone());
                let model = me.and_then(|w| cfg.resolved_model(w));
                let teammates: Vec<(String, String)> = cfg
                    .workers
                    .iter()
                    .map(|w| (w.name.clone(), w.role.clone()))
                    .collect();
                return WorkerManifestLookup {
                    reports_to,
                    works_with,
                    teammates,
                    cli_template: tmpl,
                    codebase_path: codebase,
                    model,
                };
            }
            Err(e) => {
                eprintln!(
                    "warning: .collab/team-managed points at {} but it failed to load: {}. \
                     Falling back to legacy manifest resolution.",
                    yaml_path.display(), e
                );
            }
        }
    }

    // Second choice: legacy workers.json. It never carried reports_to /
    // works_with, so we synthesize them from `hands_off_to` with the same
    // migration rule TeamConfig::from_yaml uses: first entry → reports_to,
    // rest → works_with.
    let empty = WorkerManifestLookup {
        reports_to: None,
        works_with: vec![],
        teammates: vec![],
        cli_template: None,
        codebase_path: None,
        model: None,
    };
    match find_manifest() {
        Ok(manifest_path) => match lifecycle::read_manifest(&manifest_path) {
            Ok(manifest) => {
                let entry = manifest.iter().find(|w| w.name == instance_id);
                let hands_off = entry.map(|w| w.hands_off_to.clone()).unwrap_or_default();
                let mut iter = hands_off.iter().cloned();
                let reports_to = iter.next();
                let works_with: Vec<String> = iter.collect();
                let tmpl = entry.and_then(|w| w.cli_template.clone());
                let codebase = entry.map(|w| w.codebase_path.clone());
                let model = entry.map(|w| w.model.clone()).filter(|s| !s.is_empty());
                let team: Vec<(String, String)> = manifest
                    .iter()
                    .map(|w| (w.name.clone(), w.role.clone()))
                    .collect();
                WorkerManifestLookup {
                    reports_to,
                    works_with,
                    teammates: team,
                    cli_template: tmpl,
                    codebase_path: codebase,
                    model,
                }
            }
            Err(_) => empty,
        },
        Err(_) => empty,
    }
}
