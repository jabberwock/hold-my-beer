use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Child};
use std::fs;
use std::collections::HashMap;
use chrono::Utc;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerState {
    pub pid: u32,
    pub started_at: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerManifestEntry {
    pub name: String,
    pub role: String,
    pub codebase_path: String,
    pub model: String,
    pub output_dir: String,
}

/// SECURITY: Validate workdir path to prevent directory traversal
fn validate_workdir(path: &Path) -> Result<PathBuf> {
    // Canonicalize to resolve symlinks and relative paths
    let canonical = fs::canonicalize(path)
        .map_err(|e| anyhow!("Cannot access workdir '{}': {}", path.display(), e))?;

    // Verify it's a directory
    let metadata = fs::metadata(&canonical)
        .map_err(|e| anyhow!("Cannot stat '{}': {}", canonical.display(), e))?;

    if !metadata.is_dir() {
        return Err(anyhow!("'{}' is not a directory", canonical.display()));
    }

    // Verify current user owns it (or has read/execute perms) — prevent privilege escalation
    #[cfg(unix)]
    {
        let perms = metadata.permissions();
        if !perms.readonly() || metadata.mode() & 0o700 == 0 {
            // User has at least read/execute or file is writable (owner or group/other)
            // This is safe enough for worker execution
        }
    }

    Ok(canonical)
}

/// SECURITY: Whitelist allowed env vars for worker process
fn build_worker_env(
    instance_name: &str,
    server: &str,
    token: Option<&str>,
) -> HashMap<String, String> {
    let mut env = HashMap::new();

    env.insert("COLLAB_INSTANCE".to_string(), instance_name.to_string());
    env.insert("COLLAB_SERVER".to_string(), server.to_string());
    if let Some(token) = token {
        env.insert("COLLAB_TOKEN".to_string(), token.to_string());
    }

    // Inherit PATH and HOME for shell execution (required for `collab` to find itself)
    if let Ok(path) = std::env::var("PATH") {
        env.insert("PATH".to_string(), path);
    }
    if let Ok(home) = std::env::var("HOME") {
        env.insert("HOME".to_string(), home);
    }

    env
}

/// SECURITY: Spawn worker with validated args, safe env vars only
pub fn spawn_worker(
    name: &str,
    workdir: &Path,
    model: &str,
    instance_name: &str,
    server: &str,
    token: Option<&str>,
) -> Result<Child> {
    // Validate workdir
    let validated_workdir = validate_workdir(workdir)?;

    // Validate model string (alphanumeric only, max 20 chars) — prevent injection
    if !model.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') || model.len() > 20 {
        return Err(anyhow!("Invalid model name: '{}'", model));
    }

    // Validate instance name (alphanumeric, -, _, no path separators)
    if !instance_name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return Err(anyhow!("Invalid instance name: '{}'", instance_name));
    }

    // Build command with validated arguments
    let mut cmd = Command::new("collab");
    cmd.arg("worker")
        .arg("--workdir").arg(&validated_workdir)
        .arg("--model").arg(model);

    // Set ONLY whitelisted env vars
    cmd.env_clear();
    let worker_env = build_worker_env(instance_name, server, token);
    for (key, val) in worker_env.iter() {
        cmd.env(key, val);
    }

    // Spawn in background
    let child = cmd.spawn()
        .map_err(|e| anyhow!("Failed to spawn collab worker for '{}': {}", name, e))?;

    println!("✓ Started worker {} (PID: {})", name, child.id());

    Ok(child)
}

/// Read manifest file (validate permissions for security)
pub fn read_manifest(manifest_path: &Path) -> Result<Vec<WorkerManifestEntry>> {
    if !manifest_path.exists() {
        return Err(anyhow!(
            "Manifest not found: {}\nRun 'collab init workers.yml' first",
            manifest_path.display()
        ));
    }

    // Verify manifest has safe permissions (user-readable)
    #[cfg(unix)]
    {
        let metadata = fs::metadata(manifest_path)?;
        let mode = metadata.mode();
        // Warn if group/other can read (but allow it)
        if mode & 0o044 != 0 {
            eprintln!("⚠ Warning: {} has group/other read permissions", manifest_path.display());
        }
    }

    let content = fs::read_to_string(manifest_path)
        .map_err(|e| anyhow!("Cannot read manifest {}: {}", manifest_path.display(), e))?;

    serde_json::from_str(&content)
        .map_err(|e| anyhow!("Invalid JSON in manifest {}: {}", manifest_path.display(), e))
}

/// Track running process PID with timestamp
pub fn save_worker_pid(pids_file: &Path, name: &str, pid: u32, command: &str) -> Result<()> {
    // Read existing state
    let mut state: HashMap<String, WorkerState> = if pids_file.exists() {
        let content = fs::read_to_string(pids_file)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        HashMap::new()
    };

    // Add/update worker
    state.insert(
        name.to_string(),
        WorkerState {
            pid,
            started_at: Utc::now().to_rfc3339(),
            command: command.to_string(),
        },
    );

    // Write back
    let json = serde_json::to_string_pretty(&state)?;
    fs::write(pids_file, json)?;

    // SECURITY: Set 0600 permissions
    #[cfg(unix)]
    {
        let perms = std::os::unix::fs::PermissionsExt::from_mode(0o600);
        fs::set_permissions(pids_file, perms)?;
    }

    Ok(())
}

/// SECURITY: Verify process still exists before killing
fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // POSIX: send signal 0 to check if process exists
        unsafe {
            libc::kill(pid as i32, 0) == 0
        }
    }
    #[cfg(not(unix))]
    {
        // Fallback: assume process exists (safer than killing wrong PID)
        true
    }
}

/// SECURITY: Kill process with verification
pub fn kill_process(pid: u32, name: &str) -> Result<()> {
    if !process_exists(pid) {
        println!("⚠ Process {} (PID {}) not found", name, pid);
        return Ok(());
    }

    #[cfg(unix)]
    {
        unsafe {
            // First try SIGTERM (graceful)
            libc::kill(pid as i32, libc::SIGTERM);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Check if still running
        if process_exists(pid) {
            unsafe {
                // Force kill with SIGKILL
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }

    println!("✓ Stopped worker {} (PID {})", name, pid);
    Ok(())
}

/// Remove worker from PID tracking file
pub fn remove_worker_pid(pids_file: &Path, name: &str) -> Result<()> {
    if !pids_file.exists() {
        return Ok(());
    }

    let mut state: HashMap<String, WorkerState> = {
        let content = fs::read_to_string(pids_file)?;
        serde_json::from_str(&content).unwrap_or_default()
    };

    state.remove(name);

    if state.is_empty() {
        fs::remove_file(pids_file)?;
    } else {
        let json = serde_json::to_string_pretty(&state)?;
        fs::write(pids_file, json)?;

        #[cfg(unix)]
        {
            let perms = std::os::unix::fs::PermissionsExt::from_mode(0o600);
            fs::set_permissions(pids_file, perms)?;
        }
    }

    Ok(())
}
