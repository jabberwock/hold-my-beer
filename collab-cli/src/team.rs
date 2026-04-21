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

    /// Who this worker reports completed work to. When a task is marked
    /// complete, the harness forwards a "Completed work from @me: …"
    /// message to exactly this teammate (or no one if unset). Singular on
    /// purpose — a previous design used `hands_off_to: [...]` which fanned
    /// identical completion messages out to every listed teammate.
    #[serde(default)]
    pub reports_to: Option<String>,

    /// Peers this worker actively coordinates with. Listed in the prompt's
    /// "Your team:" section so the model knows who it can @-mention,
    /// delegate to, and expect messages from. No auto-routing attached —
    /// messaging decisions are still the worker's.
    #[serde(default)]
    pub works_with: Vec<String>,

    /// Deprecated alias. Migrated at load time: first entry becomes
    /// `reports_to`; any remaining entries become `works_with`. Kept for
    /// back-compat with team.yml files written by older GUIs / CLIs.
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
        let mut cfg: TeamConfig = serde_yaml::from_str(contents)
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

            // Shape-check both legacy and new relationship fields. Reference
            // validation happens in a second pass below (needs all names).
            let name_pattern_ok = |s: &str| {
                !s.is_empty()
                    && s.len() <= 64
                    && s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            };
            for ho in &w.hands_off_to {
                if !name_pattern_ok(ho) {
                    anyhow::bail!("worker '{}' has invalid hands_off_to target '{}'", w.name, ho);
                }
            }
            if let Some(rt) = &w.reports_to {
                if !name_pattern_ok(rt) {
                    anyhow::bail!("worker '{}' has invalid reports_to target '{}'", w.name, rt);
                }
            }
            for ww in &w.works_with {
                if !name_pattern_ok(ww) {
                    anyhow::bail!("worker '{}' has invalid works_with entry '{}'", w.name, ww);
                }
            }
        }

        // Migrate `hands_off_to` → `reports_to` + `works_with` when the new
        // fields aren't already set. Back-compat for team.yml files written
        // before the schema split.
        //
        // Rule: first hands_off_to entry → reports_to (pipeline singular).
        //       any remaining entries → works_with (peer visibility).
        // The original hands_off_to vector is kept as-is so downstream code
        // that still reads it gets the same behavior until it's ported.
        for w in &mut cfg.workers {
            let already_has_new_fields = w.reports_to.is_some() || !w.works_with.is_empty();
            if already_has_new_fields || w.hands_off_to.is_empty() {
                continue;
            }
            let mut iter = w.hands_off_to.iter().cloned();
            w.reports_to = iter.next();
            w.works_with = iter.collect();
            if w.hands_off_to.len() > 1 {
                eprintln!(
                    "warning: worker '{}' uses deprecated `hands_off_to: [{}]` — \
                     migrated first entry '{}' to `reports_to` and the rest to `works_with`. \
                     Update team.yml to use the new fields directly.",
                    w.name,
                    w.hands_off_to.join(", "),
                    w.reports_to.as_deref().unwrap_or("")
                );
            }
        }

        // Validate relationship targets actually exist in the team + aren't
        // self-references. Applies to legacy `hands_off_to` too so an
        // already-migrated config still catches bad legacy values.
        for w in &cfg.workers {
            let check = |field: &str, target: &str| -> Result<()> {
                if target == w.name {
                    anyhow::bail!(
                        "worker '{}' {} itself — a worker can't hand off to itself",
                        w.name, field
                    );
                }
                if !seen_names.contains(target) {
                    anyhow::bail!(
                        "worker '{}' {} unknown teammate '{}' (not in team.yml)",
                        w.name, field, target
                    );
                }
                Ok(())
            };
            if let Some(rt) = &w.reports_to {
                check("reports_to", rt)?;
            }
            for ww in &w.works_with {
                check("works_with", ww)?;
            }
            for ho in &w.hands_off_to {
                check("hands_off_to", ho)?;
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
        // After the relationship-schema split, `hands_off_to: [a]` migrates
        // into `reports_to: a`, so the self-reference is caught by the
        // reports_to validator instead of a hands_off-specific message.
        // Loosened the assertion to match either wording.
        let err = TeamConfig::from_yaml(yaml).unwrap_err().to_string();
        assert!(
            err.contains("itself") || err.contains("self-loop"),
            "got: {}",
            err
        );
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

    // ── Relationship-schema (reports_to / works_with) ───────────────────

    #[test]
    fn parses_reports_to_and_works_with() {
        let yaml = r#"
team: t
workers:
  - name: a
    role: r
    codebase_path: /a
    reports_to: b
    works_with: [c]
  - name: b
    role: r
    codebase_path: /b
  - name: c
    role: r
    codebase_path: /c
"#;
        let cfg = TeamConfig::from_yaml(yaml).expect("parse");
        let a = cfg.workers.iter().find(|w| w.name == "a").unwrap();
        assert_eq!(a.reports_to.as_deref(), Some("b"));
        assert_eq!(a.works_with, vec!["c".to_string()]);
        // No legacy hands_off_to set → empty.
        assert!(a.hands_off_to.is_empty());
    }

    #[test]
    fn migrates_hands_off_to_singleton_to_reports_to() {
        // Regression: pre-schema-split team.yml files have only hands_off_to.
        // A single entry must migrate to reports_to (the pipeline target).
        let yaml = r#"
team: t
workers:
  - name: a
    role: r
    codebase_path: /a
    hands_off_to: [b]
  - name: b
    role: r
    codebase_path: /b
"#;
        let cfg = TeamConfig::from_yaml(yaml).expect("parse");
        let a = cfg.workers.iter().find(|w| w.name == "a").unwrap();
        assert_eq!(a.reports_to.as_deref(), Some("b"));
        assert!(a.works_with.is_empty());
    }

    #[test]
    fn migrates_hands_off_to_multiple_splits_across_fields() {
        // Multiple legacy entries: first becomes reports_to (to preserve
        // auto-handoff), remainder become works_with (peer visibility
        // without auto-routing).
        let yaml = r#"
team: t
workers:
  - name: a
    role: r
    codebase_path: /a
    hands_off_to: [b, c, d]
  - name: b
    role: r
    codebase_path: /b
  - name: c
    role: r
    codebase_path: /c
  - name: d
    role: r
    codebase_path: /d
"#;
        let cfg = TeamConfig::from_yaml(yaml).expect("parse");
        let a = cfg.workers.iter().find(|w| w.name == "a").unwrap();
        assert_eq!(a.reports_to.as_deref(), Some("b"));
        assert_eq!(a.works_with, vec!["c".to_string(), "d".to_string()]);
    }

    #[test]
    fn explicit_new_fields_skip_legacy_migration() {
        // If reports_to or works_with is already set, hands_off_to must not
        // overwrite them. The new fields are the source of truth.
        let yaml = r#"
team: t
workers:
  - name: a
    role: r
    codebase_path: /a
    reports_to: b
    hands_off_to: [c]
  - name: b
    role: r
    codebase_path: /b
  - name: c
    role: r
    codebase_path: /c
"#;
        let cfg = TeamConfig::from_yaml(yaml).expect("parse");
        let a = cfg.workers.iter().find(|w| w.name == "a").unwrap();
        assert_eq!(a.reports_to.as_deref(), Some("b"));
        assert!(a.works_with.is_empty());
        // Legacy hands_off_to is preserved verbatim for any still-legacy reader,
        // but reports_to wins for routing.
        assert_eq!(a.hands_off_to, vec!["c".to_string()]);
    }

    #[test]
    fn rejects_reports_to_self_reference() {
        let yaml = r#"
team: t
workers:
  - name: a
    role: r
    codebase_path: /a
    reports_to: a
"#;
        let err = TeamConfig::from_yaml(yaml).unwrap_err().to_string();
        assert!(err.contains("itself"), "got: {}", err);
    }

    #[test]
    fn rejects_works_with_unknown_teammate() {
        let yaml = r#"
team: t
workers:
  - name: a
    role: r
    codebase_path: /a
    works_with: [ghost]
"#;
        let err = TeamConfig::from_yaml(yaml).unwrap_err().to_string();
        assert!(err.contains("unknown teammate") && err.contains("ghost"), "got: {}", err);
    }

    #[test]
    fn rejects_reports_to_unknown_teammate() {
        let yaml = r#"
team: t
workers:
  - name: a
    role: r
    codebase_path: /a
    reports_to: ghost
"#;
        let err = TeamConfig::from_yaml(yaml).unwrap_err().to_string();
        assert!(err.contains("unknown teammate") && err.contains("ghost"), "got: {}", err);
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
