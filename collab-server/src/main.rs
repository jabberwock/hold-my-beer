use clap::Parser;
use collab_server::{AppState, db};
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::Instant;

fn token_from_config() -> Option<String> {
    #[derive(serde::Deserialize, Default)]
    struct Cfg { token: Option<String> }
    let home = std::env::var("HOME").ok()?;
    let contents = std::fs::read_to_string(format!("{}/.collab.toml", home)).ok()?;
    toml::from_str::<Cfg>(&contents).ok()?.token
}

#[derive(Parser)]
#[command(name = "collab-server")]
#[command(about = "Collaboration server for Claude Code instances")]
struct Args {
    /// Host to bind to
    #[arg(long, default_value = "0.0.0.0", env = "COLLAB_HOST")]
    host: String,

    /// Port to listen on
    #[arg(long, default_value = "8000", env = "COLLAB_PORT")]
    port: u16,

    /// Audit log mode — disables message deletion and stamps read_at on delivery
    #[arg(long, env = "COLLAB_AUDIT")]
    audit: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let db = db::init_db().await?;
    let (tx, _) = tokio::sync::broadcast::channel(256);
    // Priority: COLLAB_TOKEN env > ~/.collab.toml
    let token = std::env::var("COLLAB_TOKEN").ok().or_else(token_from_config);
    let token = token.ok_or_else(|| {
        anyhow::anyhow!(
            "Token required. Set it via:\n\
             1. COLLAB_TOKEN environment variable (or .env file)\n\
             2. token = \"...\" in ~/.collab.toml"
        )
    })?;

    let state = AppState {
        db,
        token: Some(token.clone()),
        audit: args.audit,
        tx,
        sse_subscribers: Arc::new(AtomicUsize::new(0)),
        started_at: Instant::now(),
    };
    let app = collab_server::create_app(state);

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!("Auth enabled — token required on all requests");
    if args.audit {
        tracing::info!("Audit log mode enabled — messages retained indefinitely, read_at stamped on delivery");
    }
    tracing::info!("Server listening on http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}
