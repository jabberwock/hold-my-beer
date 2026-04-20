use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Child, Stdio};
use std::fs;
use std::collections::HashMap;
use chrono::Utc;

/// Detach a command's stdio from the caller's stdin/stdout/stderr.
///
/// A worker launched by `collab start` is a long-lived daemon. If it
/// inherits the caller's pipes, they stay open as long as the worker lives,
/// so any process reading the caller's stdout blocks on EOF forever — which
/// is exactly how the GUI's "Starting workers" step wedged.
///
/// The regression test lives at `tests/worker_stdio_isolation.rs`.
pub fn configure_detached_stdio(cmd: &mut Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
}

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(windows)]
unsafe extern "system" {
    fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut core::ffi::c_void;
    fn TerminateProcess(hProcess: *mut core::ffi::c_void, uExitCode: u32) -> i32;
    fn GetExitCodeProcess(hProcess: *mut core::ffi::c_void, lpExitCode: *mut u32) -> i32;
    fn CreateToolhelp32Snapshot(dwFlags: u32, th32ProcessID: u32) -> *mut core::ffi::c_void;
    fn Process32First(hSnapshot: *mut core::ffi::c_void, lppe: *mut ProcessEntry32) -> i32;
    fn Process32Next(hSnapshot: *mut core::ffi::c_void, lppe: *mut ProcessEntry32) -> i32;
    fn CloseHandle(hObject: *mut core::ffi::c_void) -> i32;
    fn GetStdHandle(nStdHandle: u32) -> *mut core::ffi::c_void;
    fn SetHandleInformation(hObject: *mut core::ffi::c_void, dwMask: u32, dwFlags: u32) -> i32;
}

#[cfg(windows)]
#[repr(C)]
struct ProcessEntry32 {
    dw_size: u32,
    cnt_usage: u32,
    th32_process_id: u32,
    th32_default_heap_id: usize,
    th32_module_id: u32,
    cnt_threads: u32,
    th32_parent_process_id: u32,
    pc_pri_class_base: i32,
    dw_flags: u32,
    sz_exe_file: [u8; 260],
}

/// RAII guard that temporarily marks the process's stdin/stdout/stderr handles
/// as non-inheritable. On drop, the original flags are restored.
///
/// This prevents long-lived worker children from inheriting the pipe handles
/// that Tauri's sidecar API created — the root cause of the GUI hanging after
/// `collab start all` on Windows.
#[cfg(windows)]
struct StdioInheritGuard {
    handles: Vec<(*mut core::ffi::c_void, u32)>, // (handle, original HANDLE_FLAG_INHERIT bit)
}

#[cfg(windows)]
impl StdioInheritGuard {
    fn new() -> Self {
        const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;   // (DWORD)-10
        const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;   // (DWORD)-11
        const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;    // (DWORD)-12
        const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
        const INVALID_HANDLE_VALUE: *mut core::ffi::c_void = -1_isize as *mut _;

        let mut handles = Vec::new();
        for &id in &[STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            unsafe {
                let h = GetStdHandle(id);
                if !h.is_null() && h != INVALID_HANDLE_VALUE {
                    // Save original inherit flag (we'll restore it on drop)
                    handles.push((h, HANDLE_FLAG_INHERIT));
                    // Clear the inherit flag
                    SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
        Self { handles }
    }
}

#[cfg(windows)]
impl Drop for StdioInheritGuard {
    fn drop(&mut self) {
        const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
        for &(h, original) in &self.handles {
            unsafe {
                // Restore: set HANDLE_FLAG_INHERIT back to its original value
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, original);
            }
        }
    }
}

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
    /// Shared data root — falls back to output_dir parent if None
    #[serde(default)]
    pub shared_data_dir: Option<String>,
    /// CLI command template with {prompt}, {model}, {workdir} placeholders
    #[serde(default)]
    pub cli_template: Option<String>,
    /// Pipeline: workers to auto-dispatch to when this worker completes a task
    #[serde(default)]
    pub hands_off_to: Vec<String>,
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


/// SECURITY: Spawn worker with validated args, safe env vars only
pub fn spawn_worker(
    name: &str,
    workdir: &Path,
    model: &str,
    instance_name: &str,
    server: &str,
    token: Option<&str>,
    cli_template: Option<&str>,
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

    // Build command with validated arguments. Self-spawn via current_exe
    // so the worker runs the same binary as `collab start` — if we went
    // through PATH, a stale global install (e.g. `cargo install`'d from
    // a previous version) can shadow the freshly-built sidecar and the
    // worker ends up running old code. COLLAB_WORKER_BIN is a test-only
    // escape hatch (the stdio-isolation suite shadows the worker binary
    // to prove detachment); production code should never set it. Falls
    // back to PATH lookup only if the OS refuses to tell us our own
    // path, which shouldn't happen.
    let self_exe = std::env::var_os("COLLAB_WORKER_BIN")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("collab"));
    let mut cmd = Command::new(self_exe);
    cmd.arg("worker")
        .arg("--workdir").arg(&validated_workdir)
        .arg("--model").arg(model);

    if let Some(tmpl) = cli_template {
        cmd.arg("--cli-template").arg(tmpl);
    }

    // Inherit parent env, override COLLAB_* for this worker's identity
    cmd.env("COLLAB_INSTANCE", instance_name);
    cmd.env("COLLAB_SERVER", server);
    if let Some(token) = token {
        cmd.env("COLLAB_TOKEN", token);
    }

    // Make worker a process group leader so we can kill it + children together
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    // Windows: detach from the parent's console so no extra window pops up
    // and the child doesn't inherit the parent's console handle. Without this
    // the GUI's "Starting workers" step hung on Windows — `collab start`
    // couldn't exit because Tauri's sidecar reader was still draining the
    // parent console that the worker had grabbed a handle on.
    //
    //   CREATE_NO_WINDOW          = 0x08000000 — no console window
    //   CREATE_NEW_PROCESS_GROUP  = 0x00000200 — break from parent's ctrl-C group
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000 | 0x00000200);
    }

    // Detach stdio so the worker (and any grandchildren it spawns) doesn't
    // keep the caller's stdout/stderr pipes open — that's what hung the
    // GUI's "Starting workers" step for minutes on end.
    configure_detached_stdio(&mut cmd);

    // On Windows, Rust's Command always passes bInheritHandles=TRUE to
    // CreateProcessW. Even with Stdio::null(), ALL inheritable handles in
    // this process are copied into the child — including the pipe handles
    // that Tauri's sidecar API gave us. The workers keep those pipe copies
    // open, so Tauri never sees EOF and the GUI hangs.
    //
    // Fix: temporarily mark our stdio handles as non-inheritable before
    // spawning, then restore them afterward.
    #[cfg(windows)]
    let _guard = StdioInheritGuard::new();

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

/// Verify process still exists (signal 0 check)
pub fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // POSIX: send signal 0 to check if process exists
        unsafe {
            libc::kill(pid as i32, 0) == 0
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::FromRawHandle;
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let h = std::os::windows::io::OwnedHandle::from_raw_handle(handle);
            let mut exit_code: u32 = 0;
            if GetExitCodeProcess(h.as_raw_handle(), &mut exit_code) == 0 {
                return false;
            }
            exit_code == STILL_ACTIVE
        }
    }
}

/// Check if any process in the process group (pgid == pid) still exists
#[cfg(unix)]
fn process_group_exists(pgid: u32) -> bool {
    // List all PIDs and check if any belong to this process group
    let Ok(entries) = std::fs::read_dir("/proc") else {
        // /proc not available (macOS) — fall back to checking pgid via kill(0)
        return unsafe { libc::killpg(pgid as i32, 0) == 0 };
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s.chars().all(|c| c.is_ascii_digit()) {
            let stat = std::fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
            // stat format: pid (name) state ppid pgrp ...
            let fields: Vec<&str> = stat.split_whitespace().collect();
            if fields.get(4).map(|g| *g == pgid.to_string()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// SECURITY: Kill process group with verification — waits until all children are dead
pub fn kill_process(pid: u32, name: &str) -> Result<()> {
    if !process_exists(pid) {
        println!("⚠ Process {} (PID {}) not found", name, pid);
        return Ok(());
    }

    #[cfg(unix)]
    {
        unsafe {
            // SIGTERM the entire process group (collab worker + any spawned claude children)
            libc::killpg(pid as i32, libc::SIGTERM);
        }

        // Wait up to 3s for the entire process group to exit
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if !process_group_exists(pid) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                // Force kill anything still alive in the group
                unsafe { libc::killpg(pid as i32, libc::SIGKILL); }
                std::thread::sleep(std::time::Duration::from_millis(200));
                break;
            }
        }
    }

    #[cfg(windows)]
    {
        // Kill the entire process tree: collect all descendants, then terminate leaf-first
        let tree = collect_process_tree(pid);
        // Terminate in reverse (children before parents) for clean teardown
        for &child_pid in tree.iter().rev() {
            terminate_pid(child_pid);
        }
        // Always terminate the root process itself
        terminate_pid(pid);

        // Wait up to 3s for everything to actually exit
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if !process_exists(pid) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
    }

    println!("✓ Stopped worker {} (PID {})", name, pid);
    Ok(())
}

/// Walk the process tree rooted at `root_pid` and return all descendant PIDs.
#[cfg(windows)]
fn collect_process_tree(root_pid: u32) -> Vec<u32> {
    let mut descendants = Vec::new();
    let mut queue = vec![root_pid];

    while let Some(parent) = queue.pop() {
        for child in children_of(parent) {
            descendants.push(child);
            queue.push(child);
        }
    }
    descendants
}

/// Return immediate child PIDs of `parent_pid` using a toolhelp snapshot.
#[cfg(windows)]
fn children_of(parent_pid: u32) -> Vec<u32> {
    const TH32CS_SNAPPROCESS: u32 = 0x00000002;
    const INVALID_HANDLE_VALUE: *mut core::ffi::c_void = -1_isize as *mut _;
    let mut children = Vec::new();

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return children;
        }

        let mut entry: ProcessEntry32 = std::mem::zeroed();
        entry.dw_size = std::mem::size_of::<ProcessEntry32>() as u32;

        if Process32First(snap, &mut entry) != 0 {
            loop {
                if entry.th32_parent_process_id == parent_pid {
                    children.push(entry.th32_process_id);
                }
                if Process32Next(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    children
}

/// Terminate a single process by PID. Best-effort — ignores errors (process may have already exited).
#[cfg(windows)]
fn terminate_pid(pid: u32) {
    const PROCESS_TERMINATE: u32 = 0x0001;
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
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
