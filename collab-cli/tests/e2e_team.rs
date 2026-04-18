//! End-to-end smoke test covering the happy path for a multi-codebase team:
//!   admin creates team → init writes AGENT.md + markers → workers acquire
//!   leases → messages flow between them → duplicate spawn is rejected →
//!   workers.yml in team-managed repo is refused.
//!
//! Runs the real HTTP server in-process (random port) and uses the CLI's
//! own client code to drive it, so the whole stack (middleware, lease
//! endpoint, team routing, marker files) is exercised. No binary-spawning
//! — keeps the test deterministic and quota-free.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use collab_server::{create_app, db, AppState, BroadcastMsg};
use holdmybeer_cli::client::CollabClient;
use holdmybeer_cli::team::TeamManagedMarker;
use holdmybeer_cli::team_init;
use tempfile::TempDir;
use tokio::sync::broadcast;

const ADMIN_TOKEN: &str = "admin-secret";

async fn start_server_in_process() -> String {
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
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{}", addr)
}

async fn admin_create_team(server: &str, name: &str) -> String {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/admin/teams", server))
        .header("Authorization", format!("Bearer {}", ADMIN_TOKEN))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "admin create team must succeed");
    let body: serde_json::Value = resp.json().await.unwrap();
    body["token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn team_happy_path_init_messages_and_lease() {
    let server = start_server_in_process().await;

    // 1. Admin mints a team.
    let team_token = admin_create_team(&server, "blender").await;

    // 2. Prepare two fake codebases for two workers.
    let tmp = TempDir::new().unwrap();
    let codebase_a = tmp.path().join("rigger-repo");
    let codebase_b = tmp.path().join("shader-repo");
    fs::create_dir_all(&codebase_a).unwrap();
    fs::create_dir_all(&codebase_b).unwrap();

    let team_yml = tmp.path().join("blender.yml");
    fs::write(
        &team_yml,
        format!(
            "team: blender\nserver: {}\nworkers:\n  - name: rigger\n    role: rigs\n    codebase_path: {}\n    hands_off_to: [shader]\n  - name: shader\n    role: shades\n    codebase_path: {}\n",
            server,
            codebase_a.display(),
            codebase_b.display()
        ),
    )
    .unwrap();

    // 3. Run init — must write AGENT.md + markers into both codebases.
    team_init::run(&team_yml).expect("team init should succeed");
    assert!(codebase_a.join("rigger/AGENT.md").exists());
    assert!(codebase_b.join("shader/AGENT.md").exists());
    let marker_a = TeamManagedMarker::read(&codebase_a).expect("marker at codebase_a");
    assert_eq!(marker_a.team, "blender");
    assert!(codebase_b.join(".collab/team-managed").exists());

    // 4. Both workers acquire leases — both succeed (different identities).
    let client_rigger = CollabClient::new(&server, "rigger", Some(&team_token));
    let client_shader = CollabClient::new(&server, "shader", Some(&team_token));
    let _ = client_rigger.acquire_lease(100, "host-a").await.unwrap();
    let _ = client_shader.acquire_lease(200, "host-b").await.unwrap();

    // 5. Duplicate spawn: second process claiming rigger's slot from a
    //    different pid must get a conflict.
    match client_rigger.acquire_lease(999, "other-host").await.unwrap() {
        holdmybeer_cli::client::LeaseOutcome::Conflict { holder_pid, .. } => {
            assert_eq!(holder_pid, 100, "conflict must report the real holder");
        }
        other => panic!("expected Conflict, got {:?}", other),
    }

    // 6. Rigger sends a message to shader.
    client_rigger.add_message("shader", "rig ready for shading", None).await.unwrap();

    // 7. Shader's pending-message list shows it.
    let pending = client_shader.fetch_pending_messages().await.unwrap();
    assert!(
        pending.iter().any(|m| m.content == "rig ready for shading"),
        "shader should see the message from rigger; got: {:?}",
        pending.iter().map(|m| &m.content).collect::<Vec<_>>()
    );

    // 8. Init mutex: placing a workers.yml in a team-managed codebase and
    //    trying to re-init it must fail loudly (guards the double-spawn
    //    scenario we engineered the marker file to prevent).
    let stray_workers_yml = codebase_a.join("workers.yml");
    fs::write(&stray_workers_yml, "workers:\n  - name: stray\n    role: r\n").unwrap();
    // Re-running team_init::run picks up the stray workers.yml and refuses.
    let err = team_init::run(&team_yml).unwrap_err();
    let err_str = format!("{:#}", err);
    assert!(
        err_str.contains("workers.yml"),
        "init must refuse when a stray workers.yml haunts a team-managed repo; got: {}",
        err_str
    );
}

#[tokio::test]
async fn cross_team_messages_stay_isolated() {
    let server = start_server_in_process().await;
    let token_blender = admin_create_team(&server, "blender").await;
    let token_bambu = admin_create_team(&server, "bambu").await;

    // Both teams happen to have a worker named "printer" — should not
    // leak between them.
    let c_blender = CollabClient::new(&server, "printer", Some(&token_blender));
    let c_bambu = CollabClient::new(&server, "printer", Some(&token_bambu));

    c_blender.add_message("printer", "from blender team", None).await.unwrap();
    c_bambu.add_message("printer", "from bambu team", None).await.unwrap();

    let blender_inbox = c_blender.fetch_pending_messages().await.unwrap();
    let bambu_inbox = c_bambu.fetch_pending_messages().await.unwrap();
    assert_eq!(blender_inbox.len(), 1);
    assert_eq!(bambu_inbox.len(), 1);
    assert_eq!(blender_inbox[0].content, "from blender team");
    assert_eq!(bambu_inbox[0].content, "from bambu team");
}
