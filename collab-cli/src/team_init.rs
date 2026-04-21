//! Init flow for team.yml. Separate from `init.rs` (which still handles the
//! legacy `workers.yml` single-codebase mode) so the two paths can diverge
//! without each other's flags bleeding over.
//!
//! Responsibilities:
//!   1. Refuse to overwrite a repo that belongs to a different team, or that
//!      still has a leftover `workers.yml` — catching the foot-guns we
//!      identified during design (the d4dataminer drift problem).
//!   2. Write a team-aware AGENT.md into each worker's codebase_path/<name>/.
//!      The AGENT.md lists all teammates with their codebase paths so workers
//!      have a correct picture of who they can delegate to.
//!   3. Drop a `.collab/team-managed` marker in each codebase so subsequent
//!      mis-runs of `collab init workers.yml` fail loudly.
//!
//! What we explicitly don't do here:
//!   - Mint tokens or talk to the server. That happens via `collab team create`
//!     / `collab team rotate-token`. Keeping init offline makes it safe to run
//!     before the server is even up.

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::team::{expand_tilde, TeamConfig, TeamManagedMarker, TeamWorker};

/// Run `collab init <team.yml>`.
///
/// `yaml_path` should be the *absolute* path the human typed (or an absolute
/// form of it) — it's embedded in every AGENT.md as the source-of-truth
/// pointer, so relative paths would rot the moment the human cd's elsewhere.
pub fn run(yaml_path: &Path) -> Result<()> {
    let yaml_path = fs::canonicalize(yaml_path)
        .unwrap_or_else(|_| yaml_path.to_path_buf());

    let cfg = TeamConfig::from_yaml_file(&yaml_path)
        .with_context(|| format!("parsing {}", yaml_path.display()))?;

    println!("Team '{}' — {} worker(s)", cfg.team, cfg.workers.len());

    // First pass: validate every codebase_path is usable. We check all
    // workers before touching anything so a late-worker failure doesn't
    // leave AGENT.md files half-written across the file system.
    let mut resolved: Vec<(TeamWorker, PathBuf)> = Vec::with_capacity(cfg.workers.len());
    for w in &cfg.workers {
        let path = resolve_codebase_path(&w.codebase_path)
            .with_context(|| format!("worker '{}'", w.name))?;
        ensure_no_competing_manifest(&path, &cfg.team, &w.name)?;
        resolved.push((w.clone(), path));
    }

    // Second pass: write the files.
    for (worker, path) in &resolved {
        let agent_md = render_agent_md(&cfg, worker, &yaml_path);
        let worker_dir = path.join(&worker.name);
        fs::create_dir_all(&worker_dir)
            .with_context(|| format!("creating {}", worker_dir.display()))?;
        let agent_path = worker_dir.join("AGENT.md");
        fs::write(&agent_path, agent_md)
            .with_context(|| format!("writing {}", agent_path.display()))?;
        println!("  ✓  {}", agent_path.display());

        TeamManagedMarker::write(path, &cfg.team, &yaml_path)
            .with_context(|| format!("writing team-managed marker at {}", path.display()))?;
    }

    println!();
    println!("Team manifest: {}", yaml_path.display());
    println!("Next steps:");
    println!("  1. Start / ensure the collab server is running");
    println!("  2. Create the team on the server:");
    println!("       collab team create {}", cfg.team);
    println!("     (copies the token to clipboard; distribute to each worker)");
    println!("  3. Each worker sets COLLAB_TOKEN and runs `collab worker` from their codebase");

    Ok(())
}

/// Expand `~` and canonicalize. The directory must exist — we don't create
/// it on behalf of the human, since a bogus `codebase_path` is almost
/// always a typo we'd rather surface than paper over.
fn resolve_codebase_path(raw: &str) -> Result<PathBuf> {
    let expanded = expand_tilde(raw);
    let canon = fs::canonicalize(&expanded)
        .map_err(|e| anyhow!("codebase_path '{}' does not exist or isn't accessible: {}", raw, e))?;
    if !canon.is_dir() {
        anyhow::bail!("codebase_path '{}' is not a directory", raw);
    }
    Ok(canon)
}

/// Refuse to overwrite a repo that's already owned by a different team, or
/// that still has a leftover `workers.yml`. Mix-and-match is the exact
/// scenario that spawned duplicate workers and burned quota in the design
/// conversation — treat both as hard errors with a specific migration hint.
fn ensure_no_competing_manifest(codebase: &Path, team: &str, worker_name: &str) -> Result<()> {
    let workers_yml = codebase.join("workers.yml");
    if workers_yml.exists() {
        anyhow::bail!(
            "{} already has a workers.yml. Two manifests for one codebase will \
             double-spawn workers and burn quota. Run `collab team adopt {} {}` \
             to fold it into the team manifest, or delete it manually.",
            codebase.display(), workers_yml.display(), "<team.yml>"
        );
    }

    if let Some(marker) = TeamManagedMarker::read(codebase) {
        if marker.team != team {
            anyhow::bail!(
                "{} is managed by team '{}' (source: {}). \
                 Worker '{}' can't belong to two teams. Remove the existing marker \
                 (rm {}/.collab/team-managed) if you're deliberately retargeting.",
                codebase.display(), marker.team, marker.source,
                worker_name, codebase.display()
            );
        }
    }

    Ok(())
}

/// Render a team-aware AGENT.md. Same shape as the legacy template (so
/// workers don't have to learn a new layout), with three key differences:
///   - teammate list includes absolute codebase paths (because the team
///     spans multiple repos — relative paths don't mean anything across
///     them);
///   - a top-of-file provenance header tells tooling + humans which team
///     owns this file and where to edit it;
///   - a "Your team:" line near the identity block so a fresh cold-start
///     worker can orient on day one without reading the marker file.
fn render_agent_md(cfg: &TeamConfig, worker: &TeamWorker, source: &Path) -> String {
    let teammates: Vec<&TeamWorker> = cfg
        .workers
        .iter()
        .filter(|w| w.name != worker.name)
        .collect();

    let team_table = if teammates.is_empty() {
        "_(you are the only worker on this team)_\n".to_string()
    } else {
        let rows: String = teammates
            .iter()
            .map(|w| format!("| `{}` | {} | `{}` |\n", w.name, w.role, w.codebase_path))
            .collect();
        format!(
            "| Instance | Role | Codebase |\n|----------|------|----------|\n{}",
            rows
        )
    };

    let team_list = if teammates.is_empty() {
        "_(solo on team)_".to_string()
    } else {
        teammates
            .iter()
            .map(|w| format!("`{}`", w.name))
            .collect::<Vec<_>>()
            .join(", ")
    };

    // AGENT.md auto-handoff hint is driven by reports_to (the post-migration
    // singular field). Legacy hands_off_to gets flattened into reports_to by
    // TeamConfig::from_yaml, so by the time we get here it's already the
    // source of truth. No dupe-message surprises for the reader.
    let hands_off_hint = match &worker.reports_to {
        Some(target) => format!(
            "\n## Auto-Handoff\n\nWhen you finish a task, the harness will automatically dispatch downstream to: `@{}`\n",
            target
        ),
        None => String::new(),
    };

    let tasks_section = match &worker.tasks {
        Some(t) => format!("## Your Tasks\n\n{}\n\n", t.trim()),
        None => String::new(),
    };

    let model = cfg.resolved_model(worker).unwrap_or_default();
    let workdir_line = format!(
        "collab worker --workdir {} --model {}",
        worker.codebase_path, model
    );

    let shared_data = cfg.shared_data_dir.as_deref().unwrap_or("");
    let shared_line = if shared_data.is_empty() {
        String::new()
    } else {
        format!("\nShared data root: `{}`\n", shared_data)
    };

    let generated_at = chrono::Utc::now().to_rfc3339();

    format!(
        "<!-- managed-by: team={team}, source={source}, generated={generated} -->\n\
         <!-- DO NOT EDIT THIS FILE. Edit the team manifest and re-run `collab init`. -->\n\
         \n\
         # {name} — Collab Worker\n\
         \n\
         ## Identity\n\
         \n\
         You are **{name}**, a worker on team **{team}**.\n\
         \n\
         **Your role:** {role}\n\
         \n\
         **Your codebase:** `{codebase}`\n\
         \n\
         **Your teammates:** {team_list}\n\
         \n\
         ## Setup (COPY-PASTE THIS AT SESSION START)\n\
         \n\
         ```bash\n\
         export COLLAB_INSTANCE={name}\n\
         export COLLAB_SERVER={server}\n\
         export COLLAB_TOKEN=\"<your-team-token-from-human>\"\n\
         ```\n\
         \n\
         💡 **Where to get `COLLAB_TOKEN`:** ask your team lead or run \
         `collab team show {team}` (requires admin). The token authenticates \
         you into this team's namespace on the server — messages, todos, \
         and the roster are all scoped per-team so you only ever see \
         traffic for **{team}**.\n\
         \n\
         ## Team\n\
         \n\
         {team_table}\
         {shared_line}\n\
         ## Session Start\n\
         \n\
         Run these in order every session:\n\
         \n\
         **1. Orient.**\n\
         ```bash\n\
         collab whoami              # confirm you're authenticated into team {team}\n\
         collab status              # unread messages + roster\n\
         collab todo list           # pending tasks assigned to you\n\
         ```\n\
         \n\
         **2. Start the headless worker.** This is what actually spawns your \
         CLI tool when messages arrive, acquires a singleton lease so you \
         can't accidentally run two copies of yourself, and heartbeats the \
         server. Run **after** setting the env vars in step 1:\n\
         \n\
         ```bash\n\
         {workdir_line}\n\
         ```\n\
         \n\
         If the lease fails with a 409, another worker with your instance \
         ID is already running somewhere — either another shell on this \
         machine, or another machine entirely. Hunt it down before starting \
         a second one; two processes on the same identity compete for \
         messages and burn quota.\n\
         \n\
         **3. Stream presence (optional, for the dashboard).**\n\
         ```bash\n\
         collab stream --status \"working on <short description>\"\n\
         ```\n\
         \n\
         ## Output JSON — STRICT RULES\n\
         \n\
         Every CLI invocation you produce must emit a JSON object as its \
         final output. The harness delivers messages/todos/handoffs from \
         that JSON; do NOT call `collab send` / `collab todo add` / \
         `collab broadcast` yourself — those would create duplicates.\n\
         \n\
         - **`response`**: reply to the sender only if they asked a direct \
           question. Otherwise `null`.\n\
         - **`delegate`**: `[{{\"to\": \"@name\", \"task\": \"self-contained description\"}}]`. \
           Valid targets are your teammates, `@human`, `@all`, or yourself. \
           Invented worker names are rejected.\n\
         - **`messages`**: **always `null`.** Never narrate or send status \
           updates — the harness logs what you did.\n\
         - **`completed_tasks`**: hashes of any todos you finished this turn. \
           Leaving tasks open piles them up.\n\
         - **`continue`**: `true` if you have more work queued, `false` if \
           done or blocked.\n\
         - **`state_update`**: one-line status for the dashboard, e.g. \
           `{{\"status\": \"assigned routing task to @webdev\"}}`.\n\
         \n\
         {tasks_section}\
         {hands_off}\
         \n\
         ## Updating Your Role\n\
         \n\
         Your role lives in the team manifest (**not** in any file under \
         this repo). To update it:\n\
         \n\
         ```bash\n\
         collab role edit            # opens the team manifest in $EDITOR\n\
         collab init {source}\n\
         ```\n\
         \n\
         Editing `workers.json` or this AGENT.md directly won't stick — \
         they're generated. The manifest is the source of truth.\n\
         \n\
         ## Data\n\
         \n\
         **Check the filesystem before asking a teammate.** Large artifacts \
         live on disk — messages are for coordination only (\"I finished X\", \
         \"blocked on Y\").\n\
         \n\
         Your working directory: `{codebase}/{name}/`\n\
         \n\
         Teammate codebases (absolute paths — this team spans multiple repos):\n\
         {sibling_list}\n\
         \n\
         ## Rules (no exceptions)\n\
         \n\
         1. **Only act on explicit instructions.** Don't invent work. Only \
            delegate tasks you were directly told to delegate.\n\
         2. **One delegate entry per task.** Never send the same task twice.\n\
         3. **`messages` is always `null`.** Ever.\n\
         4. **`continue` is `true` while you have more work, `false` when done.**\n\
         5. **`response` is `null` unless the sender asked a direct question.**\n\
         6. **Be specific when delegating.** File paths + exact requirements, not vague hints.\n\
         7. **Finish one task before starting the next.**\n\
         8. **Mask PII.** Redact names / emails / IDs with `[NAME]`, `[EMAIL]`, `[ID]`.\n",
        team = cfg.team,
        source = source.display(),
        generated = generated_at,
        name = worker.name,
        role = worker.role,
        codebase = worker.codebase_path,
        team_list = team_list,
        team_table = team_table,
        server = cfg.server,
        workdir_line = workdir_line,
        tasks_section = tasks_section,
        hands_off = hands_off_hint,
        shared_line = shared_line,
        sibling_list = if teammates.is_empty() {
            "  _(none)_".to_string()
        } else {
            teammates
                .iter()
                .map(|w| format!("  - `{}` at `{}/{}/`", w.name, w.codebase_path, w.name))
                .collect::<Vec<_>>()
                .join("\n")
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_team_yaml(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join("team.yml");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn init_writes_agent_md_and_marker_into_each_codebase() {
        let tmp = TempDir::new().unwrap();
        let codebase_a = tmp.path().join("repo-a");
        let codebase_b = tmp.path().join("repo-b");
        fs::create_dir_all(&codebase_a).unwrap();
        fs::create_dir_all(&codebase_b).unwrap();

        let yaml_path = write_team_yaml(
            tmp.path(),
            &format!(
                "team: t\nworkers:\n  - name: a\n    role: role-a\n    codebase_path: {}\n  - name: b\n    role: role-b\n    codebase_path: {}\n",
                codebase_a.display(),
                codebase_b.display()
            ),
        );

        run(&yaml_path).unwrap();

        assert!(codebase_a.join("a/AGENT.md").exists(), "AGENT.md for a");
        assert!(codebase_b.join("b/AGENT.md").exists(), "AGENT.md for b");

        let marker_a = TeamManagedMarker::read(&codebase_a).unwrap();
        assert_eq!(marker_a.team, "t");
        let marker_b = TeamManagedMarker::read(&codebase_b).unwrap();
        assert_eq!(marker_b.team, "t");

        let agent_a = fs::read_to_string(codebase_a.join("a/AGENT.md")).unwrap();
        assert!(agent_a.contains("You are **a**"));
        assert!(agent_a.contains("team **t**"));
        assert!(agent_a.contains("`b`"), "teammate listed");
        assert!(agent_a.contains("managed-by: team=t"), "provenance header present");
    }

    #[test]
    fn init_refuses_when_workers_yml_exists_in_codebase() {
        let tmp = TempDir::new().unwrap();
        let codebase = tmp.path().join("repo");
        fs::create_dir_all(&codebase).unwrap();
        fs::write(codebase.join("workers.yml"), "workers: []\n").unwrap();

        let yaml_path = write_team_yaml(
            tmp.path(),
            &format!(
                "team: t\nworkers:\n  - name: a\n    role: r\n    codebase_path: {}\n",
                codebase.display()
            ),
        );
        let err = run(&yaml_path).unwrap_err().to_string();
        assert!(err.contains("workers.yml"), "got: {}", err);
    }

    #[test]
    fn init_refuses_when_codebase_already_managed_by_different_team() {
        let tmp = TempDir::new().unwrap();
        let codebase = tmp.path().join("repo");
        fs::create_dir_all(&codebase).unwrap();
        TeamManagedMarker::write(&codebase, "other-team", Path::new("/other/team.yml")).unwrap();

        let yaml_path = write_team_yaml(
            tmp.path(),
            &format!(
                "team: new-team\nworkers:\n  - name: a\n    role: r\n    codebase_path: {}\n",
                codebase.display()
            ),
        );
        let err = run(&yaml_path).unwrap_err().to_string();
        assert!(err.contains("managed by team 'other-team'"), "got: {}", err);
    }

    #[test]
    fn init_is_idempotent_when_same_team_reinits() {
        let tmp = TempDir::new().unwrap();
        let codebase = tmp.path().join("repo");
        fs::create_dir_all(&codebase).unwrap();

        let yaml_path = write_team_yaml(
            tmp.path(),
            &format!(
                "team: t\nworkers:\n  - name: a\n    role: r\n    codebase_path: {}\n",
                codebase.display()
            ),
        );
        run(&yaml_path).unwrap();
        // Re-running with the same team succeeds (marker already matches).
        run(&yaml_path).unwrap();
        assert!(codebase.join("a/AGENT.md").exists());
    }

    #[test]
    fn init_refuses_when_codebase_path_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let yaml_path = write_team_yaml(
            tmp.path(),
            "team: t\nworkers:\n  - name: a\n    role: r\n    codebase_path: /no/such/dir\n",
        );
        // Use {:#} to render the full anyhow error chain (top context + source).
        let err = format!("{:#}", run(&yaml_path).unwrap_err());
        assert!(err.contains("does not exist") || err.contains("isn't accessible"),
            "got: {}", err);
    }

    #[test]
    fn agent_md_includes_cross_codebase_sibling_paths() {
        let tmp = TempDir::new().unwrap();
        let codebase_a = tmp.path().join("a-repo");
        let codebase_b = tmp.path().join("b-repo");
        fs::create_dir_all(&codebase_a).unwrap();
        fs::create_dir_all(&codebase_b).unwrap();

        let yaml_path = write_team_yaml(
            tmp.path(),
            &format!(
                "team: multi\nworkers:\n  - name: alpha\n    role: alpha-role\n    codebase_path: {}\n  - name: beta\n    role: beta-role\n    codebase_path: {}\n",
                codebase_a.display(), codebase_b.display()
            ),
        );
        run(&yaml_path).unwrap();

        let agent_alpha = fs::read_to_string(codebase_a.join("alpha/AGENT.md")).unwrap();
        // Alpha's AGENT.md must reference beta's actual codebase path.
        assert!(agent_alpha.contains(&codebase_b.display().to_string()),
            "alpha's AGENT.md must show beta's absolute codebase path");
    }
}
