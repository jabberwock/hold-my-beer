//! Regression test: a spawned `collab worker` must not inherit its parent's
//! stdin/stdout/stderr. If it does, grandchildren of a short-lived parent
//! like `collab start` keep those FDs open indefinitely, and any process
//! reading the parent's stdout pipe hangs forever waiting for EOF. This is
//! exactly what wedged the GUI's "Starting workers" step.
//!
//! We can't spawn a real `collab worker` in a unit test (no server, no
//! project, no binary on PATH), so we exercise the same helper that
//! configures the command. The contract is:
//!   `configure_detached_stdio(cmd)` must set stdin/stdout/stderr to null.
//!
//! To prove that by observation rather than trusting std::process::Command's
//! private fields, we spawn `/bin/cat` with the helper applied. `cat` with
//! an inherited stdin would block on the test runner's tty; with a null
//! stdin it sees EOF immediately and exits 0 within milliseconds.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use holdmybeer_cli::lifecycle::{configure_detached_stdio, spawn_worker};

// Guards PATH mutation in `spawn_worker_detaches_stdio_end_to_end`.
static PATH_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn detached_stdio_makes_cat_exit_immediately() {
    let mut cmd = Command::new("cat");
    configure_detached_stdio(&mut cmd);

    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn cat");
    let status = child.wait().expect("wait cat");
    let elapsed = start.elapsed();

    assert!(status.success(), "cat should exit 0 with null stdin");
    assert!(
        elapsed < Duration::from_secs(2),
        "cat took {:?} to exit — stdin is not detached, parent pipe never closed",
        elapsed
    );
}

#[test]
fn detached_stdio_grandchild_does_not_hold_parent_pipe() {
    // The real-world shape: parent spawns a child that forks a long-lived
    // grandchild and exits. If stdio is inherited, the grandchild keeps the
    // pipe open and a reader on the parent's stdout blocks forever. With
    // detached stdio, the grandchild's FDs 0/1/2 point at /dev/null and the
    // parent's pipe closes the moment the parent exits.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("( sleep 10 ) & echo spawned; exit 0");
    configure_detached_stdio(&mut cmd);

    let start = Instant::now();
    let status = cmd
        .spawn()
        .expect("spawn sh")
        .wait()
        .expect("wait sh");
    let elapsed = start.elapsed();

    assert!(status.success());
    assert!(
        elapsed < Duration::from_secs(2),
        "sh parent took {:?} to exit — grandchild is holding an FD",
        elapsed
    );

    // The orphaned `sleep` reparents to init and dies on its own after 10s.
    // We deliberately don't pkill it here — a broad pattern match could hit
    // unrelated user processes and the test is short enough to tolerate it.
}

/// End-to-end guard: exercises `spawn_worker` itself (not just the helper) so
/// that deleting the `configure_detached_stdio` call from `lifecycle.rs` would
/// fail this test. We override the worker binary via COLLAB_WORKER_BIN (a
/// test-only escape hatch that bypasses the normal `current_exe()` self-spawn)
/// with a script that asserts FDs 0/1/2 point at /dev/null — if stdio is
/// detached it exits 0 in milliseconds; if inherited, the FD comparison fails.
#[test]
fn spawn_worker_detaches_stdio_end_to_end() {
    // PATH_LOCK still serializes env-var mutation across tests in this file.
    let _guard = PATH_LOCK.lock().unwrap();

    let tmp = tempfile::tempdir().expect("tempdir");
    let fake_collab = tmp.path().join("collab");
    let script = r#"#!/usr/bin/env python3
import os, sys
null = os.stat('/dev/null')
for fd in (0, 1, 2):
    st = os.fstat(fd)
    if (st.st_dev, st.st_ino) != (null.st_dev, null.st_ino):
        sys.exit(10 + fd)
sys.exit(0)
"#;
    fs::write(&fake_collab, script).expect("write fake");
    fs::set_permissions(&fake_collab, fs::Permissions::from_mode(0o755))
        .expect("chmod fake");

    // SAFETY: serialized by PATH_LOCK within this test file.
    unsafe {
        std::env::set_var("COLLAB_WORKER_BIN", &fake_collab);
    }

    let result = spawn_worker(
        "t",
        tmp.path(),
        "sonnet",
        "tester",
        "http://localhost:0",
        None,
        None,
    );

    unsafe {
        std::env::remove_var("COLLAB_WORKER_BIN");
    }

    let mut child = result.expect("spawn_worker");
    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(
                    status.success(),
                    "fake worker exited non-zero: {:?}",
                    status
                );
                return;
            }
            None if start.elapsed() > Duration::from_secs(2) => {
                let _ = child.kill();
                panic!(
                    "spawn_worker did not detach stdio — fake worker blocked {:?}",
                    start.elapsed()
                );
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
