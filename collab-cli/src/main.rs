use clap::{Parser, Subcommand};
use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

mod client;

use client::CollabClient;

#[derive(Debug, Deserialize, Default)]
struct Config {
    host: Option<String>,
    instance: Option<String>,
    #[serde(default)]
    recipients: Vec<String>,
}

fn load_config() -> Config {
    if let Some(path) = config_path() {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(config) = toml::from_str::<Config>(&contents) {
                return config;
            }
        }
    }
    Config::default()
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
enum Commands {
    /// List messages intended for this instance (last hour only)
    List,

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

    /// Poll for new messages, heartbeat presence, and watch for configured recipients
    Watch {
        /// Polling interval in seconds (default: 10)
        #[arg(short, long, default_value = "10")]
        interval: u64,

        /// Describe what you're working on (shown in roster)
        #[arg(short, long, value_name = "DESCRIPTION")]
        role: Option<String>,
    },

    /// View message history including sent and received messages
    History {
        /// Filter by conversation partner (e.g., @other_instance)
        #[arg(value_name = "@INSTANCE")]
        filter: Option<String>,
    },

    /// Show active workers (who's heartbeating or has sent messages recently)
    Roster,

    /// Print the path to the config file
    ConfigPath,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let file_config = load_config();

    // Priority: CLI flag > env var > config file > default
    let server = cli.server
        .or_else(|| std::env::var("COLLAB_SERVER").ok())
        .or(file_config.host)
        .unwrap_or_else(|| "http://localhost:8000".to_string());

    let instance = cli.instance
        .or_else(|| std::env::var("COLLAB_INSTANCE").ok())
        .or(file_config.instance);

    let recipients = file_config.recipients;

    if matches!(cli.command, Commands::Roster) {
        let client = CollabClient::new(&server, "");
        client.show_roster().await?;
        return Ok(());
    }

    if matches!(cli.command, Commands::ConfigPath) {
        match config_path() {
            Some(path) => println!("{}", path.display()),
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

    let client = CollabClient::new(&server, &instance_id);

    match cli.command {
        Commands::List => {
            client.list_messages().await?;
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
        Commands::History { filter } => {
            let filter_id = filter.as_deref().map(|s| s.trim_start_matches('@'));
            client.show_history(filter_id).await?;
        }
        Commands::Roster | Commands::ConfigPath => unreachable!(),
    }

    Ok(())
}
