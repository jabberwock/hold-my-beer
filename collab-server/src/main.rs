use clap::Parser;
use collab_server::{AppState, db};

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

    /// Shared secret token for authentication (if unset, auth is disabled)
    #[arg(long, env = "COLLAB_TOKEN")]
    token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let db = db::init_db().await?;
    let state = AppState { db, token: args.token.clone() };
    let app = collab_server::create_app(state);

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    if args.token.is_some() {
        tracing::info!("Auth enabled — token required on all requests");
    } else {
        tracing::warn!("Auth disabled — set --token or COLLAB_TOKEN to enable");
    }
    tracing::info!("Server listening on http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}
