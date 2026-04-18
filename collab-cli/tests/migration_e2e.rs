//! Binary-level end-to-end tests for the team.yml migration path.
//!
//! These deliberately spawn the actual `collab` binary (not library calls)
//! because the earlier library-level e2e completely missed the UX holes a
//! real migrator hits — adopt not minting tokens, `collab start all` not
//! finding team-managed manifests, config/token resolution across shells.
//! A regression here means the human typing commands into their terminal
//! is about to hit a wall.

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

/// Reproduce the exact user-reported bug:
///   1. Start from a legacy workers.yml
///   2. `collab team adopt` it into a team.yml
///   3. `collab team create` (manual, since adopt without --mint-token
///      doesn't do it — documented behaviour)
///   4. From the team-managed codebase dir, `collab start all` must find
///      the manifest via the .collab/team-managed marker, not error with
///      "Manifest not found".
///
/// This is the test that would have caught "collab start all → Manifest
/// not found" before shipping it to a user.
#[tokio::test]
async fn migration_adopt_then_start_finds_team_manifest() {
    let server = start_server().await;

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("my-repo");
    fs::create_dir_all(&repo).unwrap();

    // Seed a legacy workers.yml in the repo.
    fs::write(
        repo.join("workers.yml"),
        "workers:\n  - name: coder\n    role: software engineer\n",
    )
    .unwrap();

    let team_yml = tmp.path().join("d4builder.yml");

    // Step 1: adopt. This is a pure-local op, no server call.
    let adopt = collab()
        .args(["team", "adopt",
            repo.join("workers.yml").to_str().unwrap(),
            team_yml.to_str().unwrap()])
        .env("HOME", tmp.path()) // isolate ~/.collab.toml lookup
        .env_remove("COLLAB_TOKEN")
        .assert()
        .success();
    let adopt_out = String::from_utf8_lossy(&adopt.get_output().stdout).to_string();
    assert!(adopt_out.contains("Next steps"),
        "adopt must print migration guidance; got: {}", adopt_out);
    assert!(adopt_out.contains("collab team create"),
        "adopt must tell the human to mint a token; got: {}", adopt_out);
    assert!(team_yml.exists(), "team.yml created");
    assert!(!repo.join("workers.yml").exists(), "legacy workers.yml removed");
    assert!(repo.join(".collab/team-managed").exists(), "marker written");

    // Step 2: init regenerates AGENT.md files.
    collab()
        .args(["init", team_yml.to_str().unwrap()])
        .env("HOME", tmp.path())
        .env_remove("COLLAB_TOKEN")
        .assert()
        .success();
    assert!(repo.join("coder/AGENT.md").exists(), "AGENT.md generated");

    // Step 3: mint a team token on the server.
    let create_out = collab()
        .args(["team", "create", "my-repo", "--server", &server])
        .env("COLLAB_TOKEN", ADMIN_TOKEN)
        .env("HOME", tmp.path())
        .assert()
        .success();
    let create_stdout = String::from_utf8_lossy(&create_out.get_output().stdout);
    assert!(create_stdout.contains("token:"), "create prints a token");

    // Step 4 — the regression that bit the user. `collab ps` from the
    // team-managed codebase must read the team.yml via the marker and
    // NOT error with "Manifest not found".
    let ps = collab()
        .args(["ps"])
        .current_dir(&repo)
        .env("HOME", tmp.path())
        .env("COLLAB_SERVER", &server)
        .env_remove("COLLAB_TOKEN")
        .output()
        .unwrap();
    let ps_stderr = String::from_utf8_lossy(&ps.stderr);
    assert!(
        !ps_stderr.contains("Manifest not found"),
        "collab ps from team-managed dir must NOT say 'Manifest not found' — regression: {}",
        ps_stderr
    );
    assert!(ps.status.success(), "collab ps must exit 0 when no workers running; stderr: {}", ps_stderr);
}

/// Sibling test: `collab start all` from a team-managed codebase. We don't
/// actually let it spawn long-running workers (no real CLI tool available
/// in the test env), but we assert that manifest resolution succeeds —
/// i.e., we get past the "Manifest not found" check.
#[tokio::test]
async fn start_all_in_team_managed_dir_resolves_manifest() {
    let server = start_server().await;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let team_yml = tmp.path().join("t.yml");
    fs::write(
        &team_yml,
        format!(
            "team: starttest\nworkers:\n  - name: solo\n    role: r\n    codebase_path: {}\n",
            repo.display()
        ),
    )
    .unwrap();

    // Init so the marker gets written.
    collab()
        .args(["init", team_yml.to_str().unwrap()])
        .env("HOME", tmp.path())
        .env_remove("COLLAB_TOKEN")
        .assert()
        .success();

    // Create the team server-side so a future `collab start all` could
    // actually auth.
    collab()
        .args(["team", "create", "starttest", "--server", &server])
        .env("COLLAB_TOKEN", ADMIN_TOKEN)
        .env("HOME", tmp.path())
        .assert()
        .success();

    // The actual regression check: the manifest-not-found error must not
    // appear when we run `start all` from inside the codebase. We expect
    // the command might fail later (because spawning `collab worker`
    // requires more setup), but the failure must NOT be at the manifest
    // lookup step.
    let start = collab()
        .args(["start", "all"])
        .current_dir(&repo)
        .env("HOME", tmp.path())
        .env("COLLAB_SERVER", &server)
        .env_remove("COLLAB_TOKEN")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&start.stderr);
    let stdout = String::from_utf8_lossy(&start.stdout);
    assert!(
        !stderr.contains("Manifest not found") && !stdout.contains("Manifest not found"),
        "start all from team-managed dir must not hit 'Manifest not found' — regression bit user 2026-04-17. \
         stdout: {}\nstderr: {}", stdout, stderr
    );
}

/// Verify `collab team adopt --mint-token` does the round-trip in one go,
/// so a user migrating doesn't have to remember the two-step dance that
/// tripped up the original migration.
#[tokio::test]
async fn adopt_with_mint_token_creates_team_on_server() {
    let server = start_server().await;

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("my-project");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("workers.yml"),
        "workers:\n  - name: worker\n    role: r\n",
    )
    .unwrap();
    let team_yml = tmp.path().join("oneshot.yml");

    let out = collab()
        .args([
            "team", "adopt",
            repo.join("workers.yml").to_str().unwrap(),
            team_yml.to_str().unwrap(),
            "--mint-token",
            "--server", &server,
        ])
        .env("COLLAB_TOKEN", ADMIN_TOKEN)
        .env("HOME", tmp.path())
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(stdout.contains("team_id"), "mint step printed team_id; got: {}", stdout);
    assert!(stdout.contains("token:"), "mint step printed token; got: {}", stdout);
}
