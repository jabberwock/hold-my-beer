use clap::{Parser, Subcommand};
use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

mod client;
mod init;
mod worker;
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
    #[serde(default)]
    recipients: Vec<String>,
}

fn load_config() -> Config {
    let local = local_config_path().and_then(|p| read_config(&p));
    let global = config_path().and_then(|p| read_config(&p));

    match (local, global) {
        (Some(l), Some(g)) => Config {
            host: l.host.or(g.host),
            instance: l.instance.or(g.instance),
            token: l.token.or(g.token),
            recipients: if l.recipients.is_empty() { g.recipients } else { l.recipients },
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

/// CLI for inter-instance communication between Claude Code workers
#[derive(Parser)]
#[command(name = "collab")]
#[command(about = "Collaboration tool for Claude Code instances", long_about = None)]
struct Cli {
    /// Server URL (overrides $COLLAB_SERVER and ~/.collab.toml)
    #[arg(long)]
    server: Option<String>,

    /// Instance identifier (overrides $COLLAB_INSTANCE and ~/.collab.toml)
    #[arg(short, long)]
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
    },

    /// Mark a task complete
    Done {
        /// Hash prefix of the task (at least 4 chars)
        #[arg(value_name = "HASH")]
        hash: String,
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
    Add {
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

    /// [DEPRECATED] Poll for new messages — use `collab stream` instead
    ///
    /// collab stream delivers messages instantly via SSE with zero polling and
    /// survives backgrounding. collab watch is kept for compatibility only.
    Watch {
        /// Polling interval in seconds (default: 10)
        #[arg(short, long, default_value = "10")]
        interval: u64,

        /// Describe what you're working on (shown in roster)
        #[arg(short, long, value_name = "DESCRIPTION")]
        role: Option<String>,
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

    /// Stream messages in real-time via SSE (zero-poll alternative to watch)
    Stream {
        /// Describe what you're working on (shown in roster)
        #[arg(short, long, value_name = "DESCRIPTION")]
        role: Option<String>,
    },

    /// Signal all running `collab watch` instances to exit gracefully
    StopAll,

    /// View message history including sent and received messages
    History {
        /// Filter by conversation partner (e.g., @other_instance)
        #[arg(value_name = "@INSTANCE")]
        filter: Option<String>,
    },

    /// Show active workers (who's heartbeating or has sent messages recently)
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

    /// Manage persistent task queue (survives context resets)
    Todo {
        #[command(subcommand)]
        action: TodoAction,
    },

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

        /// Enable trivial message auto-reply (default: true)
        #[arg(long)]
        auto_reply: Option<bool>,

        /// Wait this long (ms) after first message before spawning (default: 2000)
        #[arg(long, value_name = "MS")]
        batch_wait: Option<u64>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let file_config = load_config();

    // Priority: CLI flag > env var > config file > default
    let server = cli.server
        .or_else(|| std::env::var("COLLAB_SERVER").ok())
        .or(file_config.host.clone())
        .unwrap_or_else(|| "http://localhost:8000".to_string());

    let instance = cli.instance
        .or_else(|| std::env::var("COLLAB_INSTANCE").ok())
        .or(file_config.instance.clone());

    let token = std::env::var("COLLAB_TOKEN").ok().or(file_config.token.clone());

    let recipients = file_config.recipients;

    if let Commands::Init { file, output } = cli.command {
        match file {
            Some(path) => {
                init::run_from_yaml(&path, output.as_deref())?;
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

    if let Commands::Worker { workdir, model, auto_reply, batch_wait } = cli.command {
        let workdir = workdir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let model = model.unwrap_or_else(|| "haiku".to_string());
        let auto_reply = auto_reply.unwrap_or(true);
        let batch_wait = batch_wait.unwrap_or(2000);

        let instance_id = instance.ok_or_else(|| {
            anyhow::anyhow!(
                "Instance ID required. Set via --instance, $COLLAB_INSTANCE, or ~/.collab.toml"
            )
        })?;

        let harness = worker::WorkerHarness::new(
            CollabClient::new(&server, &instance_id, token.as_deref()),
            instance_id,
            workdir,
            model,
            auto_reply,
            batch_wait,
        );
        harness.run().await?;
        return Ok(());
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

    let instance_id = instance.ok_or_else(|| {
        anyhow::anyhow!(
            "Instance ID required. Set via --instance, $COLLAB_INSTANCE, or ~/.collab.toml\n\
             \n\
             Example ~/.collab.toml:\n\
             host = \"http://localhost:8000\"\n\
             instance = \"worker1\"\n\
             recipients = [\"worker2\", \"worker3\"]"
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
        Commands::Add { recipient, message, refs } => {
            let recipient = recipient.trim_start_matches('@');
            let ref_hashes = refs.map(|r| {
                r.split(',').map(|s| s.trim().to_string()).collect()
            });
            client.add_message(recipient, &message, ref_hashes).await?;
        }
        Commands::Watch { interval, role } => {
            client.watch_messages(interval, role, recipients).await?;
        }
        Commands::Stream { role } => {
            client.stream_messages(role).await?;
        }
        Commands::Broadcast { message, refs } => {
            let ref_hashes = refs.map(|r| {
                r.split(',').map(|s| s.trim().to_string()).collect()
            });
            client.broadcast(&message, ref_hashes).await?;
        }
        Commands::StopAll => {
            client.stop_all().await?;
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
            TodoAction::List { instance } => {
                let instance = instance.as_deref().map(|s| s.trim_start_matches('@'));
                client.todo_list(instance).await?;
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
        Commands::Roster | Commands::ConfigPath | Commands::Init { .. } => unreachable!(),
        #[allow(unreachable_patterns)]
        #[allow(unreachable_patterns)]
        _ => unreachable!(),
    }

    Ok(())
}
