//! team.yml — cross-codebase worker manifest.
//!
//! Workflow:
//!   1. Human writes a `team.yml` anywhere (home dir, NAS, git repo, wherever).
//!   2. `collab init path/to/team.yml` walks every worker and writes its
//!      AGENT.md into the worker's own `codebase_path` dir, while writing a
//!      marker file (`.collab/team-managed`) so that same codebase can't also
//!      be accidentally re-initialised from a stray `workers.yml`.
//!   3. Workers run `collab worker` from their own codebase, auth with the
//!      team token, and all the runtime plumbing (messages, todos, lease)
//!      stays scoped to that team on the server.
//!
//! Single source of truth. If a worker's role drifts it's one file to fix,
//! no matter how many codebases the team spans.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

fn default_server() -> String {
    "http://localhost:8000".to_string()
}

/// A team manifest. Owned outright by whoever holds the file — it replaces
/// per-repo `workers.yml` for any worker that belongs to a team.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TeamConfig {
    /// Human-facing team name (alphanumeric + dash/underscore, <=64 chars).
    /// Used in CLI output, AGENT.md, and as the uniqueness key on the server.
    pub team: String,

    #[serde(default = "default_server")]
    pub server: String,

    /// Shared data root for cross-worker file exchange. Optional.
    pub shared_data_dir: Option<String>,

    /// CLI command template inherited by every worker unless they override it.
    pub cli_template: Option<String>,
    /// Light-tier (cheap) variant of the template. Same inheritance rules.
    pub cli_template_light: Option<String>,
    /// Default model. Only meaningful when `cli_template` uses {model}.
    pub model: Option<String>,

    pub workers: Vec<TeamWorker>,
}

/// Per-worker entry. Unlike the legacy `WorkerConfig`, this one owns its own
/// `codebase_path` — the whole point of team.yml is that workers live in
/// different repos.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TeamWorker {
    pub name: String,
    pub role: String,

    /// Absolute path to the repo this worker runs from. `~` is expanded.
    /// This is the *worker's* working directory when `collab worker` spawns
    /// the CLI; it's also where the AGENT.md and `.collab/team-managed`
    /// marker get written.
    pub codebase_path: String,

    pub tasks: Option<String>,
    pub avatar: Option<String>,
    pub color: Option<u8>,
    pub model: Option<String>,
    pub cli_template: Option<String>,
    pub cli_template_light: Option<String>,

    /// Names of other workers in the same team to auto-handoff to.
    #[serde(default)]
    pub hands_off_to: Vec<String>,
}

/// Crude detection: does this YAML look like a team.yml or a workers.yml?
/// We decide before parsing so error messages can point the human at the
/// right schema. `team.yml` has a top-level `team:` scalar; `workers.yml`
/// doesn't. Anything else falls through to legacy parsing.
pub fn yaml_is_team_config(contents: &str) -> bool {
    // Look for a top-level `team:` key. Tolerates comments/blank lines above.
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Only a key name matters; the value can be anything.
        if let Some(key) = trimmed.split(':').next() {
            if key == "team" {
                return true;
            }
        }
        // Any other non-comment, non-blank line at the top disambiguates —
        // if `workers:` shows up first, we're in legacy mode.
        break;
    }
    false
}

/// Expand a leading `~` to the user's home directory. Leaves other paths
/// untouched so absolute and relative paths work as-is.
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(stripped) = p.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(stripped);
        }
    }
    if p == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    }
    PathBuf::from(p)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

impl TeamConfig {
    /// Parse + validate a team.yml. Catches the mistakes a human is most
    /// likely to make with their own hands: empty workers list, duplicate
    /// names, missing codebase_path, duplicate codebase_path (both workers
    /// pointing at the same repo is a guaranteed double-spawn), bogus team
    /// name characters.
    pub fn from_yaml(contents: &str) -> Result<Self> {
        let cfg: TeamConfig = serde_yaml::from_str(contents)
            .map_err(|e| anyhow!("Invalid team.yml: {}", e))?;

        if !is_valid_team_name(&cfg.team) {
            anyhow::bail!(
                "team name '{}' is invalid (must be 1–64 chars, alphanumeric + dash/underscore)",
                cfg.team
            );
        }
        if cfg.workers.is_empty() {
            anyhow::bail!("team.yml has no workers");
        }

        let mut seen_names = std::collections::HashSet::new();

        // Deliberately *don't* enforce codebase_path uniqueness: multiple
        // worker identities in one repo is the normal case for a small
        // single-repo team (see hold-my-beer's own coder/hacker/pm). The
        // runtime lease is what actually prevents duplicate-spawn.
        for w in &cfg.workers {
            if w.name.is_empty()
                || w.name.len() > 64
                || !w.name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            {
                anyhow::bail!(
                    "worker name '{}' is invalid (must be 1–64 chars, alphanumeric + dash/underscore)",
                    w.name
                );
            }
            if !seen_names.insert(w.name.clone()) {
                anyhow::bail!("duplicate worker name '{}' in team.yml", w.name);
            }

            if w.codebase_path.trim().is_empty() {
                anyhow::bail!("worker '{}' has empty codebase_path", w.name);
            }

            for ho in &w.hands_off_to {
                // hands_off_to is resolved later (can reference any teammate);
                // we only validate shape here.
                if ho.is_empty() || ho.len() > 64
                    || !ho.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                {
                    anyhow::bail!(
                        "worker '{}' has invalid hands_off_to target '{}'",
                        w.name, ho
                    );
                }
            }
        }

        // Validate hands_off_to references actually exist in the team.
        for w in &cfg.workers {
            for ho in &w.hands_off_to {
                if ho == &w.name {
                    anyhow::bail!(
                        "worker '{}' hands_off_to itself — pipelines can't self-loop",
                        w.name
                    );
                }
                if !seen_names.contains(ho) {
                    anyhow::bail!(
                        "worker '{}' hands_off_to unknown teammate '{}' (not in team.yml)",
                        w.name, ho
                    );
                }
            }
        }

        Ok(cfg)
    }

    pub fn from_yaml_file(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .map_err(|e| anyhow!("Cannot read '{}': {}", path.display(), e))?;
        Self::from_yaml(&contents)
    }

    /// Resolved per-worker CLI template with worker-level override taking
    /// precedence over the team-level default.
    pub fn resolved_cli_template(&self, worker: &TeamWorker) -> Option<String> {
        worker.cli_template.clone().or_else(|| self.cli_template.clone())
    }

    pub fn resolved_cli_template_light(&self, worker: &TeamWorker) -> Option<String> {
        worker.cli_template_light.clone().or_else(|| self.cli_template_light.clone())
    }

    pub fn resolved_model(&self, worker: &TeamWorker) -> Option<String> {
        worker.model.clone().or_else(|| self.model.clone())
    }
}

fn is_valid_team_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// `.collab/team-managed` marker written into each worker's codebase_path
/// when init runs. Subsequent `collab init workers.yml` runs in that repo
/// detect this file and refuse, so the human can't forget they migrated.
pub const TEAM_MANAGED_MARKER: &str = ".collab/team-managed";

#[derive(Debug, Serialize, Deserialize)]
pub struct TeamManagedMarker {
    pub team: String,
    pub source: String,
    pub generated_at: String,
}

impl TeamManagedMarker {
    pub fn write(codebase_path: &Path, team: &str, source: &Path) -> Result<()> {
        let dir = codebase_path.join(".collab");
        fs::create_dir_all(&dir)?;
        let path = dir.join("team-managed");
        let marker = TeamManagedMarker {
            team: team.to_string(),
            source: source.to_string_lossy().to_string(),
            generated_at: chrono::Utc::now().to_rfc3339(),
        };
        let content = serde_json::to_string_pretty(&marker)?;
        fs::write(&path, content)?;
        Ok(())
    }

    pub fn read(codebase_path: &Path) -> Option<Self> {
        let path = codebase_path.join(TEAM_MANAGED_MARKER);
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_team_yaml_by_leading_key() {
        let team = "team: blender\nworkers: []\n";
        let workers = "workers:\n  - name: foo\n";
        assert!(yaml_is_team_config(team));
        assert!(!yaml_is_team_config(workers));
        // Comments + blank lines don't confuse detection.
        let team_with_header = "# my team\n\nteam: blender\nworkers: []\n";
        assert!(yaml_is_team_config(team_with_header));
    }

    #[test]
    fn parses_minimal_team_yaml() {
        let yaml = r#"
team: blender
workers:
  - name: rigger
    role: "Rigs characters"
    codebase_path: /tmp/rig
  - name: shader
    role: "Writes shaders"
    codebase_path: /tmp/shade
"#;
        let cfg = TeamConfig::from_yaml(yaml).unwrap();
        assert_eq!(cfg.team, "blender");
        assert_eq!(cfg.workers.len(), 2);
    }

    #[test]
    fn rejects_duplicate_worker_names() {
        let yaml = r#"
team: t
workers:
  - name: dup
    role: r
    codebase_path: /a
  - name: dup
    role: r
    codebase_path: /b
"#;
        let err = TeamConfig::from_yaml(yaml).unwrap_err().to_string();
        assert!(err.contains("duplicate worker name"), "got: {}", err);
    }

    #[test]
    fn allows_multiple_workers_per_codebase() {
        // Small single-repo teams commonly run multiple workers out of the
        // same checkout (hold-my-beer's own coder/hacker/pm). The runtime
        // lease handles singleton enforcement per-identity, so shared
        // codebase_path is fine and expected.
        let yaml = r#"
team: t
workers:
  - name: a
    role: r
    codebase_path: /same
  - name: b
    role: r
    codebase_path: /same
"#;
        let cfg = TeamConfig::from_yaml(yaml).unwrap();
        assert_eq!(cfg.workers.len(), 2);
    }

    #[test]
    fn rejects_unknown_hands_off_to_target() {
        let yaml = r#"
team: t
workers:
  - name: a
    role: r
    codebase_path: /a
    hands_off_to: [ghost]
"#;
        let err = TeamConfig::from_yaml(yaml).unwrap_err().to_string();
        assert!(err.contains("unknown teammate"), "got: {}", err);
    }

    #[test]
    fn rejects_self_loop_hands_off_to() {
        let yaml = r#"
team: t
workers:
  - name: a
    role: r
    codebase_path: /a
    hands_off_to: [a]
"#;
        let err = TeamConfig::from_yaml(yaml).unwrap_err().to_string();
        assert!(err.contains("self-loop"), "got: {}", err);
    }

    #[test]
    fn rejects_invalid_team_name() {
        let yaml = r#"
team: "bad name with spaces"
workers:
  - name: a
    role: r
    codebase_path: /a
"#;
        let err = TeamConfig::from_yaml(yaml).unwrap_err().to_string();
        assert!(err.contains("team name"), "got: {}", err);
    }

    #[test]
    fn rejects_empty_workers() {
        let yaml = r#"
team: t
workers: []
"#;
        let err = TeamConfig::from_yaml(yaml).unwrap_err().to_string();
        assert!(err.contains("no workers"), "got: {}", err);
    }

    #[test]
    fn worker_cli_template_overrides_team_default() {
        let yaml = r#"
team: t
cli_template: "team-default {prompt}"
workers:
  - name: a
    role: r
    codebase_path: /a
    cli_template: "worker-override {prompt}"
  - name: b
    role: r
    codebase_path: /b
"#;
        let cfg = TeamConfig::from_yaml(yaml).unwrap();
        assert_eq!(
            cfg.resolved_cli_template(&cfg.workers[0]).unwrap(),
            "worker-override {prompt}"
        );
        assert_eq!(
            cfg.resolved_cli_template(&cfg.workers[1]).unwrap(),
            "team-default {prompt}"
        );
    }

    #[test]
    fn tilde_expansion_works() {
        // Smoke test: expand_tilde on `~/foo` must replace the tilde.
        let expanded = expand_tilde("~/foo");
        assert!(expanded.to_string_lossy().contains("foo"));
        assert!(!expanded.to_string_lossy().starts_with('~'));
        // Non-tilde paths are returned as-is.
        let as_is = expand_tilde("/absolute/path");
        assert_eq!(as_is.to_string_lossy(), "/absolute/path");
    }
}
