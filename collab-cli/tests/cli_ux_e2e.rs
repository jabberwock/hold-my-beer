//! User-facing E2E tests. Each test corresponds to a specific UX failure
//! a real migrator hit in practice; the assertion is on what a human
//! actually sees (stdout, stderr, exit code, error message content), not
//! on internal library invariants.
//!
//! Ground rules for tests in this file:
//!   1. Spawn the actual `collab` and `collab-server` behaviour via the
//!      CLI binary (via assert_cmd) — library-level testing missed every
//!      one of the bugs these tests exist to catch.
//!   2. Isolate each test in a TempDir with a custom `$HOME` so the local
//!      ~/.collab.toml isn't polluted between runs.
//!   3. Assert on the *message* the user sees, not just the exit code. A
//!      403 is useless to a human; "this is a team token, you need admin"
//!      is the thing that actually unblocks them.
//!   4. Each test has a one-line comment naming the specific incident or
//!      report it guards against. When a test fails in CI, that comment
//!      tells whoever sees it *why it matters*.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use assert_cmd::Command;
use collab_server::{create_app, db, AppState, BroadcastMsg};
use tempfile::TempDir;
use tokio::sync::broadcast;

const ADMIN_TOKEN: &str = "admin-test-secret";

async fn start_server() -> String {
    let pool = db::init_test_db().await.unwrap();
    let (tx, _) = broadcast::channel::<Arc<BroadcastMsg>>(256);
    let state = AppState {
        db: pool,
        token: Some(ADMIN_TOKEN.to_string()),
        audit: false,
        tx,
        sse_subscribers: Arc::new(AtomicUsize::new(0)),
        started_at: Instant::now(),
    };
    let app = create_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    format!("http://{}", addr)
}

fn collab() -> Command {
    Command::cargo_bin("collab").expect("collab binary")
}

// ─── Pain point #1 ────────────────────────────────────────────────────────
// Incident: user ran `collab team create` with an admin token, got back a
// team token, then put that team token into `~/.collab.toml` overwriting
// the admin token. `collab team show` then 403'd, and the error didn't
// explain why or how to recover.

#[tokio::test]
async fn team_show_with_team_token_returns_helpful_admin_hint() {
    let server = start_server().await;
    let tmp = TempDir::new().unwrap();

    // Mint a real team token by calling create first.
    let create_out = collab()
        .args(["team", "create", "d4builder", "--server", &server])
        .env("COLLAB_ADMIN_TOKEN", ADMIN_TOKEN)
        .env("HOME", tmp.path())
        .env_remove("COLLAB_TOKEN")
        .assert().success();
    let stdout = String::from_utf8_lossy(&create_out.get_output().stdout).to_string();
    let team_token = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("token:").map(|v| v.trim().to_string()))
        .or_else(|| {
            // alternate format — just scan for tm_
            stdout.split_whitespace().find(|w| w.starts_with("tm_")).map(|s| s.to_string())
        })
        .expect("create should print team token");

    // Now hit an admin endpoint with ONLY the team token in env.
    // This is the "I put my team token in ~/.collab.toml and now admin
    // commands are broken" scenario.
    let out = collab()
        .args(["team", "show", "d4builder", "--server", &server])
        .env("COLLAB_TOKEN", &team_token)
        .env_remove("COLLAB_ADMIN_TOKEN")
        .env("HOME", tmp.path())
        .output()
        .unwrap();

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The user sees a real hint, not a raw 403 or cryptic "Forbidden."
    assert!(
        combined.to_lowercase().contains("team token")
            || combined.to_lowercase().contains("admin"),
        "403 on admin endpoint must mention team vs admin token. Got:\n{}",
        combined
    );
}

// ─── Pain point #2 ────────────────────────────────────────────────────────
// Incident: `collab whoami` with no $COLLAB_INSTANCE set said
// "auth: skipped — no instance set" when a human admin was trying to
// diagnose their token. Humans-as-admins don't have an instance.

#[tokio::test]
async fn whoami_probes_auth_even_without_instance() {
    let server = start_server().await;
    let tmp = TempDir::new().unwrap();

    let out = collab()
        .args(["whoami"])
        .env("COLLAB_ADMIN_TOKEN", ADMIN_TOKEN)
        .env("COLLAB_SERVER", &server)
        .env("HOME", tmp.path())
        .env_remove("COLLAB_INSTANCE")
        .env_remove("COLLAB_TOKEN")
        .output()
        .unwrap();

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("skipped"),
        "whoami must probe auth even when no instance is set — humans are admins too. Got:\n{}",
        combined
    );
    assert!(
        combined.to_lowercase().contains("ok")
            && (combined.to_lowercase().contains("admin") || combined.to_lowercase().contains("team")),
        "whoami must report auth status + token role. Got:\n{}",
        combined
    );
}

// ─── Pain point #3 ────────────────────────────────────────────────────────
// Incident: the user couldn't tell if their $COLLAB_TOKEN was an admin or
// team token. `collab whoami` should label it.

#[tokio::test]
async fn whoami_labels_token_as_team_when_token_is_team_token() {
    let server = start_server().await;
    let tmp = TempDir::new().unwrap();

    // mint a team token
    let create_out = collab()
        .args(["team", "create", "blendteam", "--server", &server])
        .env("COLLAB_ADMIN_TOKEN", ADMIN_TOKEN)
        .env("HOME", tmp.path())
        .assert().success();
    let stdout = String::from_utf8_lossy(&create_out.get_output().stdout).to_string();
    let team_token = stdout.split_whitespace().find(|w| w.starts_with("tm_"))
        .expect("team token").to_string();

    let out = collab()
        .args(["whoami"])
        .env("COLLAB_TOKEN", &team_token)
        .env("COLLAB_SERVER", &server)
        .env("HOME", tmp.path())
        .env_remove("COLLAB_ADMIN_TOKEN")
        .env_remove("COLLAB_INSTANCE")
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.to_lowercase().contains("team token")
            || combined.to_lowercase().contains("team member"),
        "whoami must label a tm_ token as a team token so humans don't confuse it with admin. Got:\n{}",
        combined
    );
}

// ─── Pain point #4 ────────────────────────────────────────────────────────
// Incident: `.env` file silently ignored because $COLLAB_TOKEN was already
// set in the shell from a previous session. User wasted 10 minutes editing
// .env with no effect, then discovered `eval $(cat .env)` was needed.

#[tokio::test]
async fn dotenv_shadow_prints_warning_when_shell_shadows_env_file() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("proj");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join(".env"), "COLLAB_TOKEN=value-from-env-file\n").unwrap();

    // Run ANY collab command — whoami is harmless and local. Pre-set
    // COLLAB_TOKEN in shell to a different value; the loader must warn.
    let out = collab()
        .args(["whoami"])
        .current_dir(&repo)
        .env("COLLAB_TOKEN", "value-from-shell")
        .env("HOME", tmp.path())
        .env_remove("COLLAB_ADMIN_TOKEN")
        .env_remove("COLLAB_INSTANCE")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("shadow")
            || stderr.to_lowercase().contains("overrides")
            || stderr.to_lowercase().contains("already set"),
        ".env shadow must surface a warning, not silently ignore the file. Got stderr:\n{}",
        stderr
    );
}

// ─── Pain point #5 ────────────────────────────────────────────────────────
// Incident: `collab start all` in a team-managed folder returned
// "Manifest not found. Run 'collab init workers.yml' ..." because the
// lifecycle commands only knew about the legacy workers.json manifest.

#[tokio::test]
async fn lifecycle_commands_find_team_yaml_via_marker() {
    let server = start_server().await;
    let tmp = TempDir::new().unwrap();

    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let team_yml = tmp.path().join("t.yml");
    fs::write(
        &team_yml,
        format!(
            "team: lifecycle-test\nworkers:\n  - name: solo\n    role: r\n    codebase_path: {}\n",
            repo.display()
        ),
    ).unwrap();

    collab().args(["init", team_yml.to_str().unwrap()])
        .env("HOME", tmp.path())
        .env_remove("COLLAB_TOKEN")
        .assert().success();
    collab().args(["team", "create", "lifecycle-test", "--server", &server])
        .env("COLLAB_ADMIN_TOKEN", ADMIN_TOKEN)
        .env("HOME", tmp.path())
        .assert().success();

    let ps = collab().args(["ps"])
        .current_dir(&repo)
        .env("HOME", tmp.path())
        .env_remove("COLLAB_TOKEN")
        .output().unwrap();
    let stderr = String::from_utf8_lossy(&ps.stderr);
    let stdout = String::from_utf8_lossy(&ps.stdout);
    assert!(
        !stderr.contains("Manifest not found") && !stdout.contains("Manifest not found"),
        "collab ps from team-managed dir must not hit 'Manifest not found'. stderr:\n{}\nstdout:\n{}",
        stderr, stdout
    );
    assert!(ps.status.success(), "collab ps must exit 0 when no workers running");
}

// ─── Pain point #6 ────────────────────────────────────────────────────────
// Incident: `collab team adopt` wrote team.yml, deleted workers.yml,
// wrote the marker — and then left the user with no way to actually
// auth, because no team token exists on the server. The command's output
// didn't tell them to run `collab team create` next.

#[tokio::test]
async fn adopt_output_names_the_next_command_to_run() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("adoptee");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("workers.yml"),
        "workers:\n  - name: w\n    role: r\n",
    ).unwrap();
    let team_yml = tmp.path().join("t.yml");

    let out = collab().args([
        "team", "adopt",
        repo.join("workers.yml").to_str().unwrap(),
        team_yml.to_str().unwrap(),
    ])
    .env("HOME", tmp.path())
    .env_remove("COLLAB_TOKEN")
    .env_remove("COLLAB_ADMIN_TOKEN")
    .assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(
        stdout.contains("collab team create"),
        "adopt must tell the user the literal next command to run, not leave them stranded. Got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("collab init"),
        "adopt must also name the init step so AGENT.md files get regenerated. Got:\n{}",
        stdout
    );
}

// ─── Pain point #7 ────────────────────────────────────────────────────────
// Incident: admin + team tokens rode the same env var, so putting a team
// token in ~/.collab.toml silently clobbered the admin token. The root-
// cause fix was to introduce COLLAB_ADMIN_TOKEN and have admin commands
// prefer it. Assert both exist side-by-side without conflict.

#[tokio::test]
async fn admin_token_and_team_token_coexist_via_separate_env_vars() {
    let server = start_server().await;
    let tmp = TempDir::new().unwrap();

    // Mint team token.
    let create = collab()
        .args(["team", "create", "coexist", "--server", &server])
        .env("COLLAB_ADMIN_TOKEN", ADMIN_TOKEN)
        .env("HOME", tmp.path())
        .assert().success();
    let team_tok = String::from_utf8_lossy(&create.get_output().stdout)
        .split_whitespace()
        .find(|w| w.starts_with("tm_"))
        .map(|s| s.to_string())
        .expect("team token");

    // Now have BOTH set. Admin command should still work.
    collab()
        .args(["team", "list", "--server", &server])
        .env("COLLAB_ADMIN_TOKEN", ADMIN_TOKEN)
        .env("COLLAB_TOKEN", &team_tok)
        .env("HOME", tmp.path())
        .assert()
        .success();
}

// ─── Pain point #8 ────────────────────────────────────────────────────────
// Incident: server starting without a token errored but the error didn't
// mention the --init-env / --generate-token escape hatches that land the
// user unstuck in 10 seconds.

#[tokio::test]
async fn server_no_token_error_mentions_init_env() {
    let tmp = TempDir::new().unwrap();
    let out = Command::cargo_bin("collab-server")
        .expect("binary")
        .env_remove("COLLAB_TOKEN")
        .env_remove("COLLAB_ADMIN_TOKEN")
        .env("HOME", tmp.path()) // no ~/.collab.toml
        .current_dir(tmp.path()) // no .env in cwd
        .output()
        .unwrap();
    assert!(!out.status.success(), "server should refuse to start without a token");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--init-env") || stderr.contains("init-env"),
        "no-token error must mention --init-env as the escape hatch. Got:\n{}",
        stderr
    );
}

// ─── Pain point #9 ────────────────────────────────────────────────────────
// Incident: --init-env used to write COLLAB_TOKEN=..., which is exactly
// the variable that would be clobbered by `collab team create`. It now
// writes COLLAB_ADMIN_TOKEN=... so the two secrets are structurally
// separated at file level.

#[tokio::test]
async fn init_env_writes_admin_token_env_var_not_plain_token() {
    let tmp = TempDir::new().unwrap();
    let out = Command::cargo_bin("collab-server")
        .expect("binary")
        .args(["--init-env"])
        .env("HOME", tmp.path())
        .current_dir(tmp.path())
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    let env_content = fs::read_to_string(tmp.path().join(".env"))
        .expect(".env should be written");

    assert!(
        env_content.contains("COLLAB_ADMIN_TOKEN="),
        "init-env must write COLLAB_ADMIN_TOKEN (unambiguous). Got:\n{}\nStdout:\n{}",
        env_content, stdout
    );
    assert!(
        !env_content.trim().starts_with("COLLAB_TOKEN="),
        "init-env must NOT write COLLAB_TOKEN (collides with team token). Got:\n{}",
        env_content
    );
}
