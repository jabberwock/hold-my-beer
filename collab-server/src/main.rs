use clap::Parser;
use collab_server::{AppState, db};
use std::sync::{Arc, atomic::AtomicUsize};
use std::path::PathBuf;
use std::time::Instant;

/// Load .env file from cwd or parent directories.
fn load_dotenv() {
    let home = std::env::var("HOME").ok().map(PathBuf::from);
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
                        if std::env::var(key).is_err() {
                            unsafe { std::env::set_var(key, val); }
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

fn token_from_config() -> Option<String> {
    #[derive(serde::Deserialize, Default)]
    struct Cfg { token: Option<String> }
    let home = std::env::var("HOME").ok()?;
    let contents = std::fs::read_to_string(format!("{}/.collab.toml", home)).ok()?;
    toml::from_str::<Cfg>(&contents).ok()?.token
}

#[derive(Parser)]
#[command(name = "collab-server", version)]
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

    /// Print a cryptographically-random admin token and exit. Useful for
    /// seeding a .env file without keyboard-mashing:
    ///     echo "COLLAB_TOKEN=$(collab-server --generate-token)" > .env
    #[arg(long)]
    generate_token: bool,

    /// Write a .env file in the current directory with a freshly generated
    /// COLLAB_TOKEN, then exit. Refuses to overwrite an existing .env so
    /// you don't clobber your other vars.
    #[arg(long)]
    init_env: bool,
}

/// Cryptographically random admin token, prefixed so it's visually
/// distinguishable from team tokens (which are `tm_…`).
fn generate_admin_token() -> String {
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    // 32 random bytes → simple hex. No need for a fancy encoding — the
    // string is opaque to the human and never typed by hand.
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(a.as_bytes());
    bytes.extend_from_slice(b.as_bytes());
    format!("adm_{}", bytes.iter().map(|b| format!("{:02x}", b)).collect::<String>())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    load_dotenv();
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // --generate-token: print a fresh admin token to stdout and exit.
    // Composable: users shell this into their .env / keychain / vault.
    if args.generate_token {
        println!("{}", generate_admin_token());
        return Ok(());
    }

    // --init-env: scaffold a .env with a freshly-generated token. Refuses
    // to overwrite an existing .env so the user's other env vars survive.
    if args.init_env {
        let env_path = std::path::PathBuf::from(".env");
        if env_path.exists() {
            anyhow::bail!(
                ".env already exists in {}. Not overwriting. If you want a new \
                 token, either delete the file or add COLLAB_TOKEN= manually with \
                 `collab-server --generate-token`.",
                std::env::current_dir().unwrap_or_default().display()
            );
        }
        let tok = generate_admin_token();
        std::fs::write(&env_path, format!("COLLAB_ADMIN_TOKEN={}\n", tok))?;
        println!("✓ Wrote .env with a fresh admin token.");
        println!("  path:  {}", env_path.canonicalize().unwrap_or(env_path).display());
        println!("  token: {}", tok);
        println!();
        println!("This is your ADMIN token (for `collab team create` etc.).");
        println!("Team tokens from `collab team create` go in COLLAB_TOKEN —");
        println!("they're separate secrets and can't clobber this one.");
        return Ok(());
    }

    let db = db::init_db().await?;
    let (tx, _) = tokio::sync::broadcast::channel(256);
    // Admin token priority: COLLAB_ADMIN_TOKEN env > COLLAB_TOKEN env > ~/.collab.toml
    // COLLAB_TOKEN fallback is back-compat only — new setups should use the
    // unambiguous COLLAB_ADMIN_TOKEN so worker team tokens can live in
    // COLLAB_TOKEN without clobbering the admin secret.
    let token = std::env::var("COLLAB_ADMIN_TOKEN").ok()
        .or_else(|| std::env::var("COLLAB_TOKEN").ok())
        .or_else(token_from_config);
    let token = token.ok_or_else(|| {
        anyhow::anyhow!(
            "Admin token required. Set it via:\n\
             1. COLLAB_ADMIN_TOKEN environment variable (or .env file)  ← preferred\n\
             2. COLLAB_TOKEN environment variable                         ← back-compat\n\
             3. token = \"...\" in ~/.collab.toml\n\
             \n\
             Quick start (no keyboard-walking):\n\
             $ collab-server --init-env        # writes .env with a random admin token\n\
             $ collab-server --generate-token  # prints one for manual use"
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
