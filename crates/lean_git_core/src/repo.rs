use crate::error::{AppError, AppResult};
use crate::git::exec::{GitExec, GitRunOptions, safe_read_only_args};
use crate::git::status::parse_status_z;
use crate::limits::CommandLimits;
use crate::models::{RepoConfig, RepoId, RepoStatus, RepoSummary, WatchMode};
use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct RepoRegistry {
    repos: BTreeMap<RepoId, RepoConfig>,
    summaries: BTreeMap<RepoId, RepoSummary>,
}

impl RepoRegistry {
    pub fn new(repos: Vec<RepoConfig>) -> Self {
        let mut registry = Self {
            repos: BTreeMap::new(),
            summaries: BTreeMap::new(),
        };
        for repo in repos {
            registry
                .summaries
                .insert(repo.id.clone(), RepoSummary::from_config(&repo));
            registry.repos.insert(repo.id.clone(), repo);
        }
        registry
    }

    pub fn configs(&self) -> Vec<RepoConfig> {
        self.repos.values().cloned().collect()
    }

    pub fn list(&self) -> Vec<RepoSummary> {
        self.summaries.values().cloned().collect()
    }

    pub fn get_config(&self, id: &str) -> Option<&RepoConfig> {
        self.repos.get(id)
    }

    pub fn add_repo(&mut self, git: &GitExec, path: &Path) -> AppResult<RepoSummary> {
        let root = validate_repo_root(git, path)?;
        Ok(self.add_repo_path(root))
    }

    pub fn add_repo_path(&mut self, root: PathBuf) -> RepoSummary {
        let id = canonical_repo_id(&root);
        if let Some(existing) = self.summaries.get(&id) {
            return existing.clone();
        }

        let label = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Repository")
            .to_string();
        let repo = RepoConfig {
            id: id.clone(),
            path: root.to_string_lossy().to_string(),
            label,
            favorite: false,
            trusted: false,
            watch: WatchMode::ActiveRepo,
        };
        let summary = RepoSummary::from_config(&repo);
        self.repos.insert(id.clone(), repo);
        self.summaries.insert(id, summary.clone());
        summary
    }

    pub fn remove_repo(&mut self, id: &str) -> bool {
        self.summaries.remove(id);
        self.repos.remove(id).is_some()
    }

    pub fn update_status_summary(&mut self, repo_id: &str, status: &RepoStatus) {
        if let Some(summary) = self.summaries.get_mut(repo_id) {
            summary.branch = status.branch.clone();
            summary.upstream = status.upstream.clone();
            summary.ahead = status.ahead;
            summary.behind = status.behind;
            summary.staged = status.staged.len();
            summary.unstaged = status.unstaged.len();
            summary.untracked = status.untracked.len();
            summary.conflicts = status.conflicted.len();
            summary.stale = false;
            summary.is_valid = true;
            summary.last_error = None;
            summary.last_refresh_epoch_ms = Some(now_epoch_ms());
        }
    }

    pub fn mark_stale(&mut self, repo_id: &str, message: Option<String>) -> Option<RepoSummary> {
        let summary = self.summaries.get_mut(repo_id)?;
        summary.stale = true;
        if let Some(message) = message {
            summary.last_error = Some(message);
        }
        Some(summary.clone())
    }

    pub fn summary(&self, repo_id: &str) -> Option<RepoSummary> {
        self.summaries.get(repo_id).cloned()
    }

    pub fn mark_error(&mut self, repo_id: &str, message: impl Into<String>) {
        if let Some(summary) = self.summaries.get_mut(repo_id) {
            summary.last_error = Some(message.into());
            summary.is_valid = false;
            summary.stale = true;
            summary.last_refresh_epoch_ms = Some(now_epoch_ms());
        }
    }

    pub fn set_trusted(&mut self, repo_id: &str, trusted: bool) -> Option<RepoSummary> {
        if let Some(repo) = self.repos.get_mut(repo_id) {
            repo.trusted = trusted;
        } else {
            return None;
        }
        if let Some(summary) = self.summaries.get_mut(repo_id) {
            summary.trusted = trusted;
            return Some(summary.clone());
        }
        self.repos.get(repo_id).map(RepoSummary::from_config)
    }
}

pub fn canonical_repo_id(path: &Path) -> RepoId {
    let canonical = path
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
        .to_string_lossy()
        .to_lowercase();
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn validate_repo_root(git: &GitExec, path: &Path) -> AppResult<PathBuf> {
    let output = git.run(GitRunOptions {
        repo: Some(path.to_path_buf()),
        args: safe_read_only_args(vec!["rev-parse".to_string(), "--show-toplevel".to_string()]),
        limits: CommandLimits::status(),
    })?;
    if !output.status.success() {
        return Err(AppError::git(
            "repository validation failed",
            &String::from_utf8_lossy(&output.stderr),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let root = stdout.trim();
    if root.is_empty() {
        return Err(AppError::new(
            "invalid_repo",
            "git returned an empty repo root",
        ));
    }
    Ok(PathBuf::from(root))
}

pub fn refresh_repo_status(git: &GitExec, repo: &RepoConfig) -> AppResult<RepoStatus> {
    let output = git.status_porcelain_v2(Path::new(&repo.path))?;
    if !output.status.success() {
        return Err(AppError::git(
            "status refresh failed",
            &String::from_utf8_lossy(&output.stderr),
        ));
    }
    if output.truncated_stdout {
        return Err(AppError::new(
            "status_truncated",
            "git status output exceeded the configured memory cap",
        ));
    }
    let mut status = parse_status_z(&repo.id, &output.stdout)?;
    status.repo_id = repo.id.clone();
    Ok(status)
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_id_is_stable_for_same_path() {
        let path = Path::new("D:/Example/Repo");
        assert_eq!(canonical_repo_id(path), canonical_repo_id(path));
    }

    #[test]
    fn new_repo_defaults_to_untrusted() {
        let mut registry = RepoRegistry::new(Vec::new());
        let summary = registry.add_repo_path(PathBuf::from("D:/Example/Repo"));
        assert!(!summary.trusted);
        assert!(!registry.get_config(&summary.id).unwrap().trusted);
    }

    #[test]
    fn set_trusted_updates_config_and_summary() {
        let mut registry = RepoRegistry::new(Vec::new());
        let summary = registry.add_repo_path(PathBuf::from("D:/Example/Repo"));
        let updated = registry.set_trusted(&summary.id, true).unwrap();
        assert!(updated.trusted);
        assert!(registry.get_config(&summary.id).unwrap().trusted);
        assert!(registry.list()[0].trusted);
    }

    #[test]
    fn remove_repo_preserves_others() {
        let repo_a = RepoConfig {
            id: "a".to_string(),
            path: "D:/a".to_string(),
            label: "a".to_string(),
            favorite: false,
            trusted: false,
            watch: WatchMode::Manual,
        };
        let repo_b = RepoConfig {
            id: "b".to_string(),
            path: "D:/b".to_string(),
            label: "b".to_string(),
            favorite: false,
            trusted: false,
            watch: WatchMode::Manual,
        };
        let mut registry = RepoRegistry::new(vec![repo_a, repo_b]);
        assert!(registry.remove_repo("a"));
        assert!(registry.get_config("a").is_none());
        assert!(registry.get_config("b").is_some());
    }
}
