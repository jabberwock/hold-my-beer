//! Human-facing `collab team …` subcommands. These are thin orchestration
//! over the server's /admin endpoints + some local file manipulation for
//! `adopt`. All of them require either the legacy server env token to be
//! set as COLLAB_TOKEN (that's the admin path) or a server running without
//! auth at all (in which case anyone with network access can admin — the
//! server logs a loud warning in that case).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::team::{TeamConfig, TeamManagedMarker, TeamWorker};

#[derive(Debug, Deserialize)]
struct CreateTeamResponse {
    pub team_id: String,
    pub name: String,
    pub token: String,
}

#[derive(Debug, Deserialize)]
struct TeamInfo {
    pub team_id: String,
    pub name: String,
    pub created_at: String,
    pub active_token_count: i64,
}

#[derive(Debug, Deserialize)]
struct MintTokenResponse {
    pub token: String,
    #[allow(dead_code)]
    pub token_prefix: String,
}

/// POST /admin/teams. Mints a team + initial token in one atomic call.
pub async fn create(server: &str, admin_token: Option<&str>, name: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/admin/teams", server.trim_end_matches('/'));
    let mut req = client.post(&url).json(&serde_json::json!({ "name": name }));
    if let Some(t) = admin_token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }
    let resp = req.send().await?;
    match resp.status().as_u16() {
        200 => {
            let created: CreateTeamResponse = resp.json().await?;
            println!("Team '{}' created.", created.name);
            println!();
            println!("  team_id: {}", created.team_id);
            println!("  token:   {}", created.token);
            println!();
            println!("⚠  The token is shown only once. Save it somewhere safe, then");
            println!("   distribute it to each worker (set as COLLAB_TOKEN).");
            Ok(())
        }
        401 => Err(anyhow!(
            "Unauthorized. Set COLLAB_TOKEN to the server's admin token before creating a team."
        )),
        403 => Err(anyhow!(
            "Forbidden. The token you supplied is a team token — admin operations \
             require the server's legacy/admin token, not a team token."
        )),
        409 => Err(anyhow!("A team named '{}' already exists.", name)),
        code => {
            let body = resp.text().await.unwrap_or_default();
            Err(anyhow!("create team failed: HTTP {} — {}", code, body))
        }
    }
}

/// GET /admin/teams. Prints a plain table of teams the admin can see.
pub async fn list(server: &str, admin_token: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/admin/teams", server.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(t) = admin_token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("list teams failed: HTTP {}", resp.status());
    }
    let teams: Vec<TeamInfo> = resp.json().await?;
    if teams.is_empty() {
        println!("No teams found on {}.", server);
        return Ok(());
    }
    println!("{:<24} {:<40} {:>6}", "Name", "ID", "Tokens");
    println!("{}", "─".repeat(72));
    for t in teams {
        println!(
            "{:<24} {:<40} {:>6}",
            t.name, t.team_id, t.active_token_count
        );
    }
    Ok(())
}

/// Show team details. If `from` is supplied, load the team.yml and print the
/// roster — useful for "what workers are in this team?" without a live DB
/// query (which would leak tokens via list).
pub async fn show(
    server: &str,
    admin_token: Option<&str>,
    name: &str,
    from: Option<&Path>,
) -> Result<()> {
    // Look up the team on the server first (confirms existence + token count).
    let client = reqwest::Client::new();
    let url = format!("{}/admin/teams", server.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(t) = admin_token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        if status.as_u16() == 403 {
            let hint = if admin_token.map_or(false, |t| t.starts_with("tm_")) {
                "Your COLLAB_TOKEN looks like a team token (tm_…), but `collab team show` needs an admin token. \
                 Set COLLAB_ADMIN_TOKEN (adm_…) to the admin secret and retry."
            } else {
                "`collab team show` requires an admin token. Set COLLAB_ADMIN_TOKEN (adm_…) to the admin secret and retry."
            };
            anyhow::bail!("HTTP 403 Forbidden — {}", hint);
        }
        anyhow::bail!("lookup failed: HTTP {}", status);
    }
    let teams: Vec<TeamInfo> = resp.json().await?;
    let team = teams
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| anyhow!("team '{}' not found on {}", name, server))?;

    println!("Team: {}", team.name);
    println!("  id:               {}", team.team_id);
    println!("  created_at:       {}", team.created_at);
    println!("  active tokens:    {}", team.active_token_count);

    if let Some(path) = from {
        let cfg = TeamConfig::from_yaml_file(path)
            .with_context(|| format!("reading {}", path.display()))?;
        if cfg.team != name {
            anyhow::bail!(
                "team.yml at {} declares team '{}', but you asked about '{}'",
                path.display(), cfg.team, name
            );
        }
        println!();
        println!("Workers ({} from {}):", cfg.workers.len(), path.display());
        for w in &cfg.workers {
            let handoff = if w.hands_off_to.is_empty() {
                String::new()
            } else {
                format!(" → {}", w.hands_off_to.iter().map(|n| format!("@{}", n)).collect::<Vec<_>>().join(", "))
            };
            println!("  @{}  {}  [{}]{}", w.name, w.role, w.codebase_path, handoff);
        }
    }
    Ok(())
}

/// Rotate a team's token. Mints a new one and prints it; the old tokens stay
/// valid until a human explicitly revokes them. v1 doesn't auto-revoke — too
/// easy to lock the team out mid-sprint.
pub async fn rotate_token(server: &str, admin_token: Option<&str>, name: &str) -> Result<()> {
    // First, look up the team_id.
    let client = reqwest::Client::new();
    let list_url = format!("{}/admin/teams", server.trim_end_matches('/'));
    let mut req = client.get(&list_url);
    if let Some(t) = admin_token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }
    let teams: Vec<TeamInfo> = req.send().await?.json().await?;
    let team = teams
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| anyhow!("team '{}' not found on {}", name, server))?;

    // Mint the new token.
    let mint_url = format!(
        "{}/admin/teams/{}/tokens",
        server.trim_end_matches('/'),
        team.team_id
    );
    let mut req = client.post(&mint_url);
    if let Some(t) = admin_token {
        req = req.header("Authorization", format!("Bearer {}", t));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("mint token failed: HTTP {}", resp.status());
    }
    let minted: MintTokenResponse = resp.json().await?;

    println!("New token for team '{}':", name);
    println!();
    println!("  {}", minted.token);
    println!();
    println!("⚠  The token is shown only once. Distribute it to the team.");
    println!("   Old tokens still work — revoke them with the server's /admin/teams/{}/tokens/<hash-prefix>", team.team_id);
    println!("   endpoint once everyone has rotated. (A nicer CLI for this is coming.)");
    Ok(())
}

/// `collab team adopt <workers.yml> <team.yml>` — take a legacy single-repo
/// manifest and fold it into a team.yml. This is the sanctioned path out of
/// the workers.yml era; doing it by hand + re-running init risks the
/// "two manifests, same repo, double-spawn" footgun we've been guarding.
///
/// Behavior:
///   - Read workers.yml (must parse as legacy ProjectConfig)
///   - Read team.yml (create if missing, using repo name as team name)
///   - Append workers to team.yml, with codebase_path = directory of workers.yml
///   - Enforce uniqueness: refuse if any worker name already present in team.yml
///   - Delete the original workers.yml
///   - Re-run the init flow so AGENT.md regenerates from team.yml
///
/// Note: adopt is pure file-manipulation; it does NOT mint a team token on
/// the server. After adopt, the human still needs to run `collab team create`
/// (or call `maybe_mint_team_token`) so the team exists on the server with
/// a token that workers can authenticate with.
pub fn adopt(workers_yml: &Path, team_yml_path: &Path) -> Result<()> {
    // Parse the legacy manifest.
    let workers_content = fs::read_to_string(workers_yml)
        .with_context(|| format!("reading {}", workers_yml.display()))?;
    if crate::team::yaml_is_team_config(&workers_content) {
        anyhow::bail!(
            "{} looks like a team.yml (has a top-level 'team:' key). Point adopt at a legacy workers.yml instead.",
            workers_yml.display()
        );
    }
    let legacy: crate::init::ProjectConfig = serde_yaml::from_str(&workers_content)
        .with_context(|| format!("parsing {} as workers.yml", workers_yml.display()))?;

    let codebase_path = workers_yml
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", workers_yml.display()))?
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", workers_yml.display()))?
        .to_string_lossy()
        .to_string();

    // Load (or initialize) the target team.yml.
    let mut team_cfg: TeamConfigMut = if team_yml_path.exists() {
        let content = fs::read_to_string(team_yml_path)
            .with_context(|| format!("reading {}", team_yml_path.display()))?;
        serde_yaml::from_str(&content)
            .with_context(|| format!("parsing {}", team_yml_path.display()))?
    } else {
        // No team.yml yet: seed one. Name defaults to the workers.yml's
        // parent directory basename, which is almost always what the human
        // would have typed anyway.
        let seed_name = Path::new(&codebase_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("team")
            .to_string();
        TeamConfigMut {
            team: seed_name,
            server: legacy.server.clone(),
            shared_data_dir: legacy.shared_data_dir.clone(),
            cli_template: legacy.cli_template.clone(),
            model: legacy.model.clone(),
            workers: Vec::new(),
        }
    };

    // Name uniqueness — refuse if any legacy worker name already exists.
    // Codebase uniqueness is *not* enforced here: a team.yml can have
    // multiple workers rooted at the same repo (coder + hacker pattern),
    // and the runtime lease prevents same-identity duplication regardless.
    for lw in &legacy.workers {
        if team_cfg.workers.iter().any(|w| w.name == lw.name) {
            anyhow::bail!(
                "team.yml already has a worker named '{}'. Rename one before adopting.",
                lw.name
            );
        }
    }

    // Append — keep fields explicit so a workers.yml field we don't carry
    // over gets noticed at code-review time rather than silently dropped.
    for lw in &legacy.workers {
        team_cfg.workers.push(TeamWorker {
            name: lw.name.clone(),
            role: lw.role.clone(),
            codebase_path: codebase_path.clone(),
            tasks: lw.tasks.clone(),
            avatar: lw.avatar.clone(),
            color: lw.color,
            model: lw.model.clone(),
            cli_template: lw.cli_template.clone(),
            // Legacy workers.yml never exposed reports_to / works_with;
            // leave them unset here and let TeamConfig::from_yaml's
            // migration split hands_off_to into them on the next load.
            reports_to: None,
            works_with: Vec::new(),
            hands_off_to: lw.hands_off_to.clone(),
        });
    }

    // Serialize + validate by round-tripping through TeamConfig::from_yaml —
    // this catches the full rule set (unknown fields, duplicate names, etc.)
    // before we touch the disk.
    let new_yaml = serde_yaml::to_string(&team_cfg)
        .with_context(|| "serializing new team.yml")?;
    let _validated = TeamConfig::from_yaml(&new_yaml)
        .with_context(|| "validating merged team.yml")?;

    // Backup: write the merged YAML to a temp, then swap. If the swap
    // fails after we've deleted workers.yml, the human still has both
    // the backup and the original (workers.yml isn't deleted yet).
    let tmp = team_yml_path.with_extension("yml.tmp");
    fs::write(&tmp, &new_yaml)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, team_yml_path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), team_yml_path.display()))?;

    // Delete the legacy workers.yml and any stale .collab/ artifacts in the
    // same repo that would otherwise trick the manifest lookup into going
    // back to the legacy path.
    fs::remove_file(workers_yml).with_context(|| format!("removing {}", workers_yml.display()))?;
    let collab_dir = Path::new(&codebase_path).join(".collab");
    let workers_json = collab_dir.join("workers.json");
    if workers_json.exists() {
        let _ = fs::remove_file(&workers_json);
    }

    // Write the team-managed marker so the next `collab init workers.yml`
    // in this repo fails loudly.
    let team_source = team_yml_path.canonicalize().unwrap_or_else(|_| team_yml_path.to_path_buf());
    TeamManagedMarker::write(Path::new(&codebase_path), &team_cfg.team, &team_source)?;

    println!("Adopted {} into {}:", workers_yml.display(), team_yml_path.display());
    for lw in &legacy.workers {
        println!("  + @{}  ({})", lw.name, lw.role);
    }
    println!();
    println!("Next steps (in order):");
    println!("  1. Regenerate AGENT.md files:");
    println!("       collab init {}", team_yml_path.display());
    println!("  2. Mint a team token on the server (requires COLLAB_TOKEN=admin-token):");
    println!("       collab team create {}", team_cfg.team);
    println!("  3. Set COLLAB_TOKEN=<team-token> on each worker machine.");
    println!();
    println!("Skipping step 2 will leave the team nonexistent on the server — workers");
    println!("will fail auth until it's created.");

    Ok(())
}

/// Variant of `adopt` that additionally tries to mint a team token on the
/// server. Best-effort: if the server is unreachable or the admin token
/// isn't set, we still do the file work and print the manual instructions.
pub async fn adopt_with_token_mint(
    workers_yml: &Path,
    team_yml_path: &Path,
    server: &str,
    admin_token: Option<&str>,
) -> Result<()> {
    // Do the file migration first. If this fails, no server call happens.
    adopt(workers_yml, team_yml_path)?;

    // Read the freshly-written team.yml to get its name.
    let team_name = {
        let content = fs::read_to_string(team_yml_path)?;
        let cfg = TeamConfig::from_yaml(&content)?;
        cfg.team
    };

    println!();
    println!("Attempting to mint a token for team '{}'…", team_name);
    match create(server, admin_token, &team_name).await {
        Ok(_) => {
            // `create` already prints the token + next steps.
            Ok(())
        }
        Err(e) => {
            let msg = format!("{}", e);
            // Already-exists is a soft failure — the team is on the server
            // already, the human just needs to rotate if they lost the token.
            if msg.contains("already exists") {
                println!("Team '{}' already exists on the server.", team_name);
                println!("If you need a fresh token, run:");
                println!("    collab team rotate-token {}", team_name);
                Ok(())
            } else {
                println!("Could not mint token automatically: {}", msg);
                println!("Mint manually once the server is reachable:");
                println!("    collab team create {}", team_name);
                // Not a hard error — the file migration succeeded.
                Ok(())
            }
        }
    }
}

/// Mirrors TeamConfig but serialisable. Needed because TeamConfig carries
/// Serialize + Deserialize, but serde_yaml::to_string prefers a type that
/// doesn't have `deny_unknown_fields` (which blocks serde's default naming).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct TeamConfigMut {
    pub team: String,
    #[serde(default = "default_server")]
    pub server: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_data_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub workers: Vec<TeamWorker>,
}

fn default_server() -> String {
    "http://localhost:8000".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_workers_yml(path: &Path) {
        fs::write(
            path,
            r#"
server: http://localhost:8000
workers:
  - name: coder
    role: Software engineer
  - name: hacker
    role: Security reviewer
    hands_off_to: [coder]
"#,
        )
        .unwrap();
    }

    #[test]
    fn adopt_creates_team_yml_from_workers_yml_when_missing() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("my-project");
        fs::create_dir_all(&repo).unwrap();
        let workers_yml = repo.join("workers.yml");
        sample_workers_yml(&workers_yml);
        let team_yml = tmp.path().join("team.yml");

        adopt(&workers_yml, &team_yml).unwrap();

        assert!(team_yml.exists(), "team.yml created");
        assert!(!workers_yml.exists(), "workers.yml deleted");
        assert!(TeamManagedMarker::read(&repo).is_some(), "marker written");

        let cfg = TeamConfig::from_yaml_file(&team_yml).unwrap();
        assert_eq!(cfg.workers.len(), 2);
        assert!(cfg.workers.iter().all(|w| !w.codebase_path.is_empty()));
    }

    #[test]
    fn adopt_refuses_when_worker_name_collides() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let workers_yml = repo.join("workers.yml");
        sample_workers_yml(&workers_yml);

        // Pre-existing team.yml already has a `coder`.
        let team_yml = tmp.path().join("team.yml");
        fs::write(
            &team_yml,
            format!(
                "team: t\nworkers:\n  - name: coder\n    role: other role\n    codebase_path: {}\n",
                tmp.path().join("somewhere-else").display()
            ),
        )
        .unwrap();

        let err = adopt(&workers_yml, &team_yml).unwrap_err().to_string();
        assert!(err.contains("already has a worker named 'coder'"), "got: {}", err);
        // Safety: original workers.yml preserved on refusal.
        assert!(workers_yml.exists(), "workers.yml preserved when adopt fails");
    }

    #[test]
    fn adopt_refuses_team_yml_masquerading_as_workers_yml() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let bad = repo.join("workers.yml");
        fs::write(
            &bad,
            "team: t\nworkers:\n  - name: a\n    role: r\n    codebase_path: /a\n",
        )
        .unwrap();
        let team_yml = tmp.path().join("team.yml");
        let err = adopt(&bad, &team_yml).unwrap_err().to_string();
        assert!(err.contains("looks like a team.yml"), "got: {}", err);
    }
}
