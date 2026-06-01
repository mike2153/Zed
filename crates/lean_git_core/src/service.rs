use crate::config::{clamp_git_workers, default_config_dir, load_config, save_config_atomic};
use crate::error::redact_sensitive_git_output;
use crate::fs::preview::read_worktree_file;
use crate::fs::search::search_worktree_files;
use crate::fs::tree::list_worktree_dir;
use crate::git::diff::{looks_binary, parse_unified_diff};
use crate::git::exec::{
    GitExec, GitExitStatus, GitOutput, GitRunOptions, safe_mutation_args, safe_read_only_args,
};
use crate::git::history::{decode_history_cursor, parse_history_page_with_cursor};
use crate::git::status::parse_status_z;
use crate::git::tree::parse_ls_tree_z;
use crate::limits::{
    CommandLimits, DEFAULT_HISTORY_LIMIT, MAX_COMMIT_MESSAGE_LEN, MAX_DIRECTORY_ENTRIES,
    MAX_FILE_PREVIEW_BYTES, MAX_IPC_PATH_LEN, MAX_IPC_PATHS,
};
use crate::models::{
    AppConfig, Diagnostics, DiffResult, DiffTarget, DirectoryListing, FilePreview,
    FileSearchResults, HistoryPage, OperationLogEntry, RepoConfig, RepoStatus, RepoSummary,
};
use crate::repo::canonical_repo_id;
use crate::scheduler::RuntimeScheduler;
use crate::state::AppState;
use crate::{AppError, AppResult};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_LOCAL_GIT_CONFIG_BYTES: u64 = 256 * 1024;

pub struct AppService {
    config_dir: PathBuf,
    git: GitExec,
    scheduler: Arc<RuntimeScheduler>,
    config_load_error: Option<String>,
    inner: Arc<Mutex<AppState>>,
    config_write_lock: Arc<Mutex<()>>,
}

impl Clone for AppService {
    fn clone(&self) -> Self {
        Self {
            config_dir: self.config_dir.clone(),
            git: self.git.clone(),
            scheduler: Arc::clone(&self.scheduler),
            config_load_error: self.config_load_error.clone(),
            inner: Arc::clone(&self.inner),
            config_write_lock: Arc::clone(&self.config_write_lock),
        }
    }
}

impl AppService {
    pub fn load(app_name: &str) -> Self {
        let config_dir = default_config_dir(app_name);
        let (config, config_load_error) = match load_config(&config_dir) {
            Ok(config) => (config, None),
            Err(err) => (AppConfig::default(), Some(err.message)),
        };
        Self::new(config_dir, GitExec::default(), config, config_load_error)
    }

    pub fn new(
        config_dir: PathBuf,
        git: GitExec,
        config: AppConfig,
        config_load_error: Option<String>,
    ) -> Self {
        Self {
            config_dir,
            git,
            scheduler: Arc::new(RuntimeScheduler::default()),
            config_load_error,
            inner: Arc::new(Mutex::new(AppState::new(config))),
            config_write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn state_handle(&self) -> Arc<Mutex<AppState>> {
        Arc::clone(&self.inner)
    }

    pub fn scheduler(&self) -> Arc<RuntimeScheduler> {
        Arc::clone(&self.scheduler)
    }

    pub fn git(&self) -> GitExec {
        self.git.clone()
    }

    pub fn git_version(&self) -> AppResult<String> {
        let output = self.run_git(
            None,
            false,
            GitRunOptions {
                repo: None,
                args: vec!["--version".to_string()],
                limits: CommandLimits {
                    timeout: std::time::Duration::from_secs(10),
                    max_stdout_bytes: 16 * 1024,
                    max_stderr_bytes: 16 * 1024,
                },
            },
        )?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(AppError::git(
                "git --version failed",
                &String::from_utf8_lossy(&output.stderr),
            ))
        }
    }

    pub fn diagnostics(&self) -> AppResult<Diagnostics> {
        let mut diagnostics = self.lock_state()?.diagnostics();
        diagnostics.git_workers_in_use = self.scheduler.running_workers();
        diagnostics.git_executable = self.git.executable().display().to_string();
        diagnostics.config_load_error = self.config_load_error.clone();
        Ok(diagnostics)
    }

    pub fn operation_log(&self) -> AppResult<Vec<OperationLogEntry>> {
        Ok(self
            .lock_state()?
            .operation_log
            .iter()
            .cloned()
            .rev()
            .collect())
    }

    pub fn list_repos(&self) -> AppResult<Vec<RepoSummary>> {
        Ok(self.lock_state()?.registry.list())
    }

    pub fn repo_config(&self, repo_id: &str) -> AppResult<RepoConfig> {
        self.repo_for(repo_id)
    }

    pub fn active_repo_config(&self) -> AppResult<Option<RepoConfig>> {
        let state = self.lock_state()?;
        if let Some(active_id) = &state.active_repo {
            return Ok(state.registry.get_config(active_id).cloned());
        }
        let first_id = state.registry.list().first().map(|repo| repo.id.clone());
        Ok(first_id.and_then(|id| state.registry.get_config(&id).cloned()))
    }

    pub fn active_status(&self) -> AppResult<Option<RepoStatus>> {
        Ok(self.lock_state()?.active_status.clone())
    }

    pub fn add_repo(&self, path: &Path) -> AppResult<RepoSummary> {
        let (root, is_git_repo) = match self.validate_repo_root(path) {
            Ok(root) => (root, true),
            Err(_) if path.is_dir() => (
                path.canonicalize()
                    .map_err(|err| AppError::io("canonicalize folder", err))?,
                false,
            ),
            Err(_) => {
                return Err(AppError::new(
                    "invalid_repo",
                    "selected path is not a folder",
                ));
            }
        };
        let _config_guard = self.lock_config_write()?;
        let (summary, config) = {
            let mut app = self.lock_state()?;
            let mut summary = app.registry.add_repo_path(root);
            if !is_git_repo {
                app.registry.mark_error(
                    &summary.id,
                    "Not a Git repository. Initialize Git to enable history and status.",
                );
                summary = app.registry.summary(&summary.id).unwrap_or(summary);
            }
            app.config.repos = app.registry.configs();
            (summary, app.config.clone())
        };
        self.persist_config(&config)?;
        Ok(summary)
    }

    pub fn init_repo(&self, repo_id: String) -> AppResult<RepoSummary> {
        let repo = self.repo_for(&repo_id)?;
        let start = Instant::now();
        let output = self.run_git(
            Some(&repo.id),
            true,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_mutation_args(vec![
                    "init".to_string(),
                    "--initial-branch=main".to_string(),
                ]),
                limits: CommandLimits::standard(),
            },
        )?;
        if !output.status.success() {
            self.record_log(Some(repo.id), "init", false, start, &output.stderr)?;
            return Err(git_output_error("git init failed", &output));
        }
        self.record_log(Some(repo.id.clone()), "init", true, start, &output.stdout)?;
        let status = self.refresh_repo_inner(repo.id.clone())?;
        Ok(self
            .lock_state()?
            .registry
            .summary(&status.repo_id)
            .ok_or_else(|| AppError::new("repo_missing", "repository is not registered"))?)
    }

    pub fn remove_repo(&self, repo_id: &str) -> AppResult<Vec<RepoSummary>> {
        let _config_guard = self.lock_config_write()?;
        let (repos, config) = {
            let mut app = self.lock_state()?;
            app.registry.remove_repo(repo_id);
            app.config.repos = app.registry.configs();
            if app.active_repo.as_deref() == Some(repo_id) {
                app.set_active_repo(None);
            }
            (app.registry.list(), app.config.clone())
        };
        self.persist_config(&config)?;
        Ok(repos)
    }

    pub fn set_active_repo(&self, repo_id: &str) -> AppResult<RepoSummary> {
        let _config_guard = self.lock_config_write()?;
        let (summary, config) = {
            let mut app = self.lock_state()?;
            let summary = app
                .registry
                .summary(repo_id)
                .ok_or_else(|| AppError::new("repo_missing", "repository is not registered"))?;
            app.set_active_repo(Some(repo_id.to_string()));
            (summary, app.config.clone())
        };
        self.persist_config(&config)?;
        Ok(summary)
    }

    pub fn trim_caches(&self) -> AppResult<Diagnostics> {
        {
            let mut app = self.lock_state()?;
            app.trim_caches();
        }
        self.diagnostics()
    }

    pub fn set_repo_trusted(&self, repo_id: &str, trusted: bool) -> AppResult<RepoSummary> {
        let repo = self.repo_for(repo_id)?;
        if trusted {
            self.ensure_repo_identity_current(&repo)?;
            ensure_repo_local_git_config_safe(&repo)?;
        }
        let _config_guard = self.lock_config_write()?;
        let (summary, config) = {
            let mut app = self.lock_state()?;
            let summary = app
                .registry
                .set_trusted(repo_id, trusted)
                .ok_or_else(|| AppError::new("repo_missing", "repository is not registered"))?;
            app.config.repos = app.registry.configs();
            (summary, app.config.clone())
        };
        self.persist_config(&config)?;
        Ok(summary)
    }

    pub fn refresh_repo(&self, repo_id: String) -> AppResult<RepoStatus> {
        self.refresh_repo_inner(repo_id)
    }

    pub fn stage_paths(&self, repo_id: String, paths: Vec<String>) -> AppResult<RepoStatus> {
        self.run_paths_mutation(repo_id, paths, "stage", |paths| {
            let mut args = vec!["add".to_string(), "--".to_string()];
            args.extend(paths.iter().cloned());
            safe_mutation_args(args)
        })
    }

    pub fn unstage_paths(&self, repo_id: String, paths: Vec<String>) -> AppResult<RepoStatus> {
        self.run_paths_mutation(repo_id, paths, "unstage", |paths| {
            let mut args = vec![
                "restore".to_string(),
                "--staged".to_string(),
                "--".to_string(),
            ];
            args.extend(paths.iter().cloned());
            safe_mutation_args(args)
        })
    }

    pub fn commit(&self, repo_id: String, message: String) -> AppResult<RepoStatus> {
        if message.trim().is_empty() {
            return Err(AppError::new(
                "invalid_commit",
                "commit message is required",
            ));
        }
        if message.len() > MAX_COMMIT_MESSAGE_LEN {
            return Err(AppError::new(
                "invalid_commit",
                "commit message exceeds the configured length cap",
            ));
        }
        let repo = self.repo_for(&repo_id)?;
        self.require_current_trusted_repo(&repo)?;
        let start = Instant::now();
        let output = self.run_git(
            Some(&repo.id),
            true,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_mutation_args(vec![
                    "commit".to_string(),
                    "--no-verify".to_string(),
                    "--no-gpg-sign".to_string(),
                    "-m".to_string(),
                    message,
                ]),
                limits: CommandLimits::standard(),
            },
        )?;
        if !output.status.success() {
            self.record_log(Some(repo.id), "commit", false, start, &output.stderr)?;
            return Err(AppError::git(
                "commit failed",
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        self.record_log(Some(repo.id.clone()), "commit", true, start, &output.stdout)?;
        self.refresh_repo_inner(repo.id)
    }

    pub fn fetch_repo(&self, repo_id: String) -> AppResult<RepoStatus> {
        self.run_git_then_refresh(
            repo_id,
            "fetch",
            vec!["fetch", "--prune"],
            CommandLimits::network(),
        )
    }

    pub fn pull_ff_only(&self, repo_id: String) -> AppResult<RepoStatus> {
        self.run_git_then_refresh(
            repo_id,
            "pull",
            vec!["pull", "--ff-only", "--no-verify"],
            CommandLimits::network(),
        )
    }

    pub fn push_repo(&self, repo_id: String, remote_url: Option<String>) -> AppResult<RepoStatus> {
        let repo = self.repo_for(&repo_id)?;
        self.require_current_trusted_repo(&repo)?;
        if let Some(remote_url) = remote_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            validate_remote_url(remote_url)?;
            self.configure_origin_remote(&repo, remote_url)?;
            self.run_git_then_refresh(
                repo.id,
                "push",
                vec!["push", "--no-verify", "-u", "origin", "HEAD"],
                CommandLimits::network(),
            )
        } else {
            self.run_git_then_refresh(
                repo.id,
                "push",
                vec!["push", "--no-verify"],
                CommandLimits::network(),
            )
        }
    }

    fn configure_origin_remote(&self, repo: &RepoConfig, remote_url: &str) -> AppResult<()> {
        let probe = self.run_git(
            Some(&repo.id),
            false,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_read_only_args(vec![
                    "remote".to_string(),
                    "get-url".to_string(),
                    "origin".to_string(),
                ]),
                limits: CommandLimits::standard(),
            },
        )?;
        let action = if probe.status.success() {
            "remote set-url"
        } else {
            "remote add"
        };
        let args = if probe.status.success() {
            vec![
                "remote".to_string(),
                "set-url".to_string(),
                "origin".to_string(),
                remote_url.to_string(),
            ]
        } else {
            vec![
                "remote".to_string(),
                "add".to_string(),
                "origin".to_string(),
                remote_url.to_string(),
            ]
        };
        let start = Instant::now();
        let output = self.run_git(
            Some(&repo.id),
            true,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_mutation_args(args),
                limits: CommandLimits::standard(),
            },
        )?;
        self.record_log(
            Some(repo.id.clone()),
            action,
            output.status.success(),
            start,
            if output.status.success() {
                &output.stdout
            } else {
                &output.stderr
            },
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(git_output_error(&format!("{action} failed"), &output))
        }
    }

    pub fn get_diff(&self, repo_id: String, path: String, staged: bool) -> AppResult<DiffResult> {
        let repo = self.repo_for(&repo_id)?;
        validate_git_pathspecs(std::slice::from_ref(&path))?;
        let mut args = vec![
            "diff".to_string(),
            "--no-ext-diff".to_string(),
            "--no-textconv".to_string(),
            "--unified=3".to_string(),
        ];
        if staged {
            args.push("--cached".to_string());
        }
        args.push("--".to_string());
        args.push(path.clone());
        let output = self.run_git(
            Some(&repo.id),
            false,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_read_only_args(args),
                limits: CommandLimits::diff(),
            },
        )?;
        if !output_succeeded_or_output_limited(&output) {
            return Err(AppError::git(
                "diff failed",
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        let target = DiffTarget {
            repo_id,
            path,
            staged,
            commit: None,
        };
        let diff = parse_unified_diff(target, &output.stdout, output.truncated_stdout)?;
        let mut app = self.lock_state()?;
        app.selected_diff = Some(diff.clone());
        Ok(diff)
    }

    pub fn get_commit_diff(
        &self,
        repo_id: String,
        commit: String,
        path: Option<String>,
    ) -> AppResult<DiffResult> {
        validate_commit_id(&commit)?;
        let repo = self.repo_for(&repo_id)?;
        let path = path.unwrap_or_default();
        if !path.is_empty() {
            validate_git_pathspecs(std::slice::from_ref(&path))?;
        }
        let mut args = vec![
            "show".to_string(),
            "--format=".to_string(),
            "--no-ext-diff".to_string(),
            "--no-textconv".to_string(),
            "--find-renames".to_string(),
            "--unified=3".to_string(),
            commit.clone(),
        ];
        if !path.is_empty() {
            args.push("--".to_string());
            args.push(path.clone());
        }
        let output = self.run_git(
            Some(&repo.id),
            false,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_read_only_args(args),
                limits: CommandLimits::diff(),
            },
        )?;
        if !output_succeeded_or_output_limited(&output) {
            return Err(AppError::git(
                "commit diff failed",
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        let target = DiffTarget {
            repo_id,
            path,
            staged: false,
            commit: Some(commit),
        };
        let diff = parse_unified_diff(target, &output.stdout, output.truncated_stdout)?;
        let mut app = self.lock_state()?;
        app.selected_diff = Some(diff.clone());
        Ok(diff)
    }

    pub fn get_history(
        &self,
        repo_id: String,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> AppResult<HistoryPage> {
        let repo = self.repo_for(&repo_id)?;
        let limit = limit.unwrap_or(DEFAULT_HISTORY_LIMIT).clamp(1, 1000);
        let cursor = decode_history_cursor(cursor.as_deref());
        let skip = cursor.offset;
        let mut args = vec![
            "log".to_string(),
            "--topo-order".to_string(),
            "--decorate=short".to_string(),
            "--parents".to_string(),
            format!("--max-count={}", limit + 1),
            format!("--skip={skip}"),
            "--format=%H%x1f%P%x1f%an%x1f%at%x1f%D%x1f%s".to_string(),
        ];
        if let Some(anchor) = &cursor.anchor {
            validate_commit_id(anchor)?;
            args.push(anchor.clone());
        }
        let output = self.run_git(
            Some(&repo.id),
            false,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_read_only_args(args),
                limits: CommandLimits::standard(),
            },
        )?;
        if !output.status.success() {
            return Err(AppError::git(
                "history failed",
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        let page = parse_history_page_with_cursor(&repo.id, &output.stdout, limit, cursor)?;
        let mut app = self.lock_state()?;
        app.active_history = Some(page.clone());
        Ok(page)
    }

    pub fn list_dir(
        &self,
        repo_id: String,
        path: String,
        cursor: Option<usize>,
        limit: Option<usize>,
    ) -> AppResult<DirectoryListing> {
        let repo = self.repo_for(&repo_id)?;
        let listing = list_worktree_dir(
            repo.id.clone(),
            Path::new(&repo.path),
            &path,
            cursor.unwrap_or(0),
            limit.unwrap_or(MAX_DIRECTORY_ENTRIES),
        )?;
        let mut app = self.lock_state()?;
        app.cache_directory(format!("{}:{}", repo.id, path), listing.clone());
        Ok(listing)
    }

    pub fn read_file_preview(&self, repo_id: String, path: String) -> AppResult<FilePreview> {
        let repo = self.repo_for(&repo_id)?;
        let preview = read_worktree_file(repo.id.clone(), Path::new(&repo.path), &path)?;
        let mut app = self.lock_state()?;
        app.selected_preview = Some(preview.clone());
        Ok(preview)
    }

    pub fn search_files(
        &self,
        repo_id: String,
        query: String,
        limit: Option<usize>,
    ) -> AppResult<FileSearchResults> {
        let repo = self.repo_for(&repo_id)?;
        search_worktree_files(repo.id.clone(), Path::new(&repo.path), &query, limit)
    }

    pub fn list_commit_dir(
        &self,
        repo_id: String,
        commit: String,
        path: String,
        cursor: Option<usize>,
        limit: Option<usize>,
    ) -> AppResult<DirectoryListing> {
        validate_commit_id(&commit)?;
        let path = normalize_git_tree_path(&path)?;
        let repo = self.repo_for(&repo_id)?;
        let output = self.run_git(
            Some(&repo.id),
            false,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_read_only_args(vec![
                    "ls-tree".to_string(),
                    "-z".to_string(),
                    "-l".to_string(),
                    commit_treeish(&commit, &path),
                ]),
                limits: CommandLimits::tree(),
            },
        )?;
        if !output.status.success() {
            return Err(AppError::git(
                "commit tree listing failed",
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        if output.truncated_stdout {
            return Err(AppError::new(
                "tree_truncated",
                "git tree output exceeded the configured memory cap",
            ));
        }
        let listing = parse_ls_tree_z(
            repo.id.clone(),
            &path,
            &output.stdout,
            cursor.unwrap_or(0),
            limit.unwrap_or(MAX_DIRECTORY_ENTRIES),
        )?;
        let mut app = self.lock_state()?;
        app.cache_directory(format!("{}:{}:{}", repo.id, commit, path), listing.clone());
        Ok(listing)
    }

    pub fn read_commit_file_preview(
        &self,
        repo_id: String,
        commit: String,
        path: String,
    ) -> AppResult<FilePreview> {
        validate_commit_id(&commit)?;
        let path = normalize_git_tree_path(&path)?;
        if path.is_empty() {
            return Err(AppError::new("invalid_path", "file path is required"));
        }
        let repo = self.repo_for(&repo_id)?;
        let output = self.run_git(
            Some(&repo.id),
            false,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_read_only_args(vec![
                    "show".to_string(),
                    "--no-ext-diff".to_string(),
                    "--no-textconv".to_string(),
                    commit_treeish(&commit, &path),
                ]),
                limits: CommandLimits::file_preview(),
            },
        )?;
        if !output_succeeded_or_output_limited(&output) {
            return Err(AppError::git(
                "commit file preview failed",
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        let mut bytes = output.stdout;
        let truncated = output.truncated_stdout || bytes.len() > MAX_FILE_PREVIEW_BYTES;
        if bytes.len() > MAX_FILE_PREVIEW_BYTES {
            bytes.truncate(MAX_FILE_PREVIEW_BYTES);
        }
        let is_binary = looks_binary(&bytes);
        let preview = FilePreview {
            repo_id: repo.id.clone(),
            path,
            is_binary,
            truncated,
            bytes_read: bytes.len(),
            text: (!is_binary).then(|| String::from_utf8_lossy(&bytes).to_string()),
        };
        let mut app = self.lock_state()?;
        app.selected_preview = Some(preview.clone());
        Ok(preview)
    }

    fn refresh_repo_inner(&self, repo_id: String) -> AppResult<RepoStatus> {
        let repo = self.repo_for(&repo_id)?;
        let start = Instant::now();
        let status = self.load_repo_status(&repo);
        let mut app = self.lock_state()?;
        match status {
            Ok(status) => {
                app.registry.update_status_summary(&repo.id, &status);
                if app.active_repo.as_deref() == Some(repo.id.as_str()) {
                    app.active_status = Some(status.clone());
                }
                app.push_log(log_entry(
                    Some(repo.id.clone()),
                    "refresh",
                    true,
                    start.elapsed().as_millis(),
                    "status refreshed",
                ));
                Ok(status)
            }
            Err(err) => {
                app.registry.mark_error(&repo.id, err.message.clone());
                app.push_log(log_entry(
                    Some(repo.id.clone()),
                    "refresh",
                    false,
                    start.elapsed().as_millis(),
                    err.message.clone(),
                ));
                Err(err)
            }
        }
    }

    fn load_repo_status(&self, repo: &RepoConfig) -> AppResult<RepoStatus> {
        let output = self.run_git(
            Some(&repo.id),
            false,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_read_only_args(vec![
                    "status".to_string(),
                    "--porcelain=v2".to_string(),
                    "--branch".to_string(),
                    "-z".to_string(),
                ]),
                limits: CommandLimits::status(),
            },
        )?;
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

    fn run_paths_mutation<F>(
        &self,
        repo_id: String,
        paths: Vec<String>,
        action: &'static str,
        make_args: F,
    ) -> AppResult<RepoStatus>
    where
        F: FnOnce(&[String]) -> Vec<String>,
    {
        if paths.is_empty() {
            return Err(AppError::new(
                "invalid_paths",
                "at least one path is required",
            ));
        }
        let repo = self.repo_for(&repo_id)?;
        self.require_current_trusted_repo(&repo)?;
        let paths = validate_git_pathspecs(&paths)?;
        let start = Instant::now();
        let output = self.run_git(
            Some(&repo.id),
            true,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: make_args(&paths),
                limits: CommandLimits::standard(),
            },
        )?;
        if !output.status.success() {
            self.record_log(Some(repo.id), action, false, start, &output.stderr)?;
            return Err(git_output_error(&format!("{action} failed"), &output));
        }
        self.record_log(Some(repo.id.clone()), action, true, start, &output.stdout)?;
        self.refresh_repo_inner(repo.id)
    }

    fn run_git_then_refresh(
        &self,
        repo_id: String,
        action: &'static str,
        args: Vec<&str>,
        limits: CommandLimits,
    ) -> AppResult<RepoStatus> {
        let repo = self.repo_for(&repo_id)?;
        if remote_operation_requires_trust(action) {
            self.require_current_trusted_repo(&repo)?;
        }
        let start = Instant::now();
        let output = self.run_git(
            Some(&repo.id),
            true,
            GitRunOptions {
                repo: Some(PathBuf::from(&repo.path)),
                args: safe_mutation_args(args.into_iter().map(ToString::to_string).collect()),
                limits,
            },
        )?;
        if !output.status.success() {
            self.record_log(Some(repo.id), action, false, start, &output.stderr)?;
            return Err(git_output_error(&format!("{action} failed"), &output));
        }
        self.record_log(Some(repo.id.clone()), action, true, start, &output.stdout)?;
        self.refresh_repo_inner(repo.id)
    }

    fn validate_repo_root(&self, path: &Path) -> AppResult<PathBuf> {
        let output = self.run_git(
            None,
            false,
            GitRunOptions {
                repo: Some(path.to_path_buf()),
                args: safe_read_only_args(vec![
                    "rev-parse".to_string(),
                    "--show-toplevel".to_string(),
                ]),
                limits: CommandLimits::status(),
            },
        )?;
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

    fn repo_for(&self, repo_id: &str) -> AppResult<RepoConfig> {
        self.lock_state()?
            .registry
            .get_config(repo_id)
            .cloned()
            .ok_or_else(|| AppError::new("repo_missing", "repository is not registered"))
    }

    fn require_current_trusted_repo(&self, repo: &RepoConfig) -> AppResult<()> {
        require_trusted_repo(repo)?;
        self.ensure_repo_identity_current(repo)?;
        ensure_repo_local_git_config_safe(repo)
    }

    fn ensure_repo_identity_current(&self, repo: &RepoConfig) -> AppResult<()> {
        let root = self.validate_repo_root(Path::new(&repo.path))?;
        let current_id = canonical_repo_id(&root);
        if current_id == repo.id {
            return Ok(());
        }

        let config = {
            let mut app = self.lock_state()?;
            let _ = app.registry.set_trusted(&repo.id, false);
            app.config.repos = app.registry.configs();
            app.config.clone()
        };
        self.persist_config(&config)?;
        Err(AppError::new(
            "repo_identity_changed",
            "repository identity changed; trust was cleared before running Git mutations",
        ))
    }

    fn record_log(
        &self,
        repo_id: Option<String>,
        action: &'static str,
        ok: bool,
        start: Instant,
        output: &[u8],
    ) -> AppResult<()> {
        let mut app = self.lock_state()?;
        app.push_log(log_entry(
            repo_id,
            action,
            ok,
            start.elapsed().as_millis(),
            redact_sensitive_git_output(String::from_utf8_lossy(output).trim()),
        ));
        Ok(())
    }

    fn run_git(
        &self,
        repo_id: Option<&str>,
        is_mutation: bool,
        options: GitRunOptions,
    ) -> AppResult<GitOutput> {
        let max_workers = self.max_git_workers()?;
        self.scheduler
            .run(repo_id, is_mutation, max_workers, || self.git.run(options))
    }

    fn max_git_workers(&self) -> AppResult<usize> {
        Ok(clamp_git_workers(
            self.lock_state()?.config.settings.max_git_workers,
        ))
    }

    fn persist_config(&self, config: &AppConfig) -> AppResult<()> {
        save_config_atomic(&self.config_dir, config)
    }

    fn lock_state(&self) -> AppResult<std::sync::MutexGuard<'_, AppState>> {
        self.inner
            .lock()
            .map_err(|_| AppError::new("state_lock", "app state lock poisoned"))
    }

    fn lock_config_write(&self) -> AppResult<std::sync::MutexGuard<'_, ()>> {
        self.config_write_lock
            .lock()
            .map_err(|_| AppError::new("config_write_lock", "config write lock poisoned"))
    }
}

pub fn require_trusted_repo(repo: &RepoConfig) -> AppResult<()> {
    if repo.trusted {
        Ok(())
    } else {
        Err(AppError::new(
            "repo_untrusted",
            "trust this repository before running Git mutations",
        ))
    }
}

pub fn remote_operation_requires_trust(action: &str) -> bool {
    matches!(action, "fetch" | "pull" | "push")
}

pub fn validate_remote_url(url: &str) -> AppResult<()> {
    if url.is_empty() || url.starts_with('-') || url.chars().any(char::is_control) {
        return Err(AppError::new(
            "invalid_remote",
            "remote URL must be non-empty and cannot start with '-'",
        ));
    }
    Ok(())
}

pub fn ensure_repo_local_git_config_safe(repo: &RepoConfig) -> AppResult<()> {
    let config = read_repo_local_git_config(Path::new(&repo.path))?;
    let Some(config) = config else {
        return Ok(());
    };
    let unsafe_keys = unsafe_local_git_config_keys(&config);
    if unsafe_keys.is_empty() {
        return Ok(());
    }
    Err(AppError::new(
        "unsafe_repo_config",
        format!(
            "repository local Git config contains executable helper/filter settings: {}; remove them or use system/global Git config",
            unsafe_keys.join(", ")
        ),
    ))
}

fn read_repo_local_git_config(repo_root: &Path) -> AppResult<Option<String>> {
    let Some(config_path) = repo_local_git_config_path(repo_root)? else {
        return Ok(None);
    };
    let mut configs = Vec::new();
    if let Some(config) = read_git_config_file(&config_path)? {
        configs.push(config);
    }
    if let Some(config) = read_git_config_file(&config_path.with_file_name("config.worktree"))? {
        configs.push(config);
    }
    if configs.is_empty() {
        Ok(None)
    } else {
        Ok(Some(configs.join("\n")))
    }
}

fn read_git_config_file(config_path: &Path) -> AppResult<Option<String>> {
    if !config_path.is_file() {
        return Ok(None);
    }
    let metadata =
        fs::metadata(&config_path).map_err(|err| AppError::io("read git config", err))?;
    if metadata.len() > MAX_LOCAL_GIT_CONFIG_BYTES {
        return Err(AppError::new(
            "unsafe_repo_config",
            "repository local Git config exceeds the safety scan size cap",
        ));
    }
    let bytes = fs::read(&config_path).map_err(|err| AppError::io("read git config", err))?;
    Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
}

fn repo_local_git_config_path(repo_root: &Path) -> AppResult<Option<PathBuf>> {
    let dot_git = repo_root.join(".git");
    if dot_git.is_dir() {
        return Ok(Some(dot_git.join("config")));
    }
    if !dot_git.is_file() {
        return Ok(None);
    }
    let metadata = fs::metadata(&dot_git).map_err(|err| AppError::io("read .git file", err))?;
    if metadata.len() > 16 * 1024 {
        return Err(AppError::new(
            "unsafe_repo_config",
            ".git file exceeds the safety scan size cap",
        ));
    }
    let text = fs::read_to_string(&dot_git).map_err(|err| AppError::io("read .git file", err))?;
    let Some(gitdir) = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))
    else {
        return Ok(None);
    };
    let gitdir = PathBuf::from(gitdir);
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        repo_root.join(gitdir)
    };
    Ok(Some(gitdir.join("config")))
}

fn unsafe_local_git_config_keys(config: &str) -> Vec<String> {
    let mut section = String::new();
    let mut unsafe_keys = Vec::new();
    for raw_line in config.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line
            .strip_prefix('[')
            .and_then(|line| line.find(']').map(|end| &line[..end]))
        {
            section = header.trim().to_ascii_lowercase();
            continue;
        }
        let key = line
            .split_once('=')
            .map(|(key, _)| key)
            .unwrap_or(line)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        if unsafe_local_git_config_key(&section, &key) {
            unsafe_keys.push(display_git_config_key(&section, &key));
        }
    }
    unsafe_keys.sort();
    unsafe_keys.dedup();
    unsafe_keys
}

fn unsafe_local_git_config_key(section: &str, key: &str) -> bool {
    (section == "core"
        && matches!(
            key,
            "sshcommand" | "askpass" | "hookspath" | "gitproxy" | "fsmonitor"
        ))
        || (section == "commit" && key == "gpgsign")
        || (section == "gpg" && key == "program")
        || (section.starts_with("gpg ") && key == "program")
        || (section == "credential" && key == "helper")
        || (section.starts_with("credential ") && key == "helper")
        || (section == "include" && key == "path")
        || (section.starts_with("includeif ") && key == "path")
        || (section.starts_with("filter ") && matches!(key, "clean" | "process" | "smudge"))
}

fn display_git_config_key(section: &str, key: &str) -> String {
    let display_key = match key {
        "sshcommand" => "sshCommand",
        "askpass" => "askPass",
        "hookspath" => "hooksPath",
        "gitproxy" => "gitProxy",
        "fsmonitor" => "fsmonitor",
        "gpgsign" => "gpgSign",
        other => other,
    };
    if section.starts_with("filter ") {
        format!("filter.*.{display_key}")
    } else if section.starts_with("credential ") {
        format!("credential.*.{display_key}")
    } else if section.starts_with("includeif ") {
        format!("includeIf.*.{display_key}")
    } else if section.starts_with("gpg ") {
        "gpg.ssh.program".to_string()
    } else {
        format!("{section}.{display_key}")
    }
}

pub fn output_succeeded_or_output_limited(output: &GitOutput) -> bool {
    output.status.success() || matches!(output.status, GitExitStatus::OutputLimitExceeded)
}

fn git_output_error(context: &str, output: &GitOutput) -> AppError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        return AppError::git(context, &stderr);
    }

    let detail = match output.status {
        GitExitStatus::TimedOut => format!(
            "git command timed out after {} seconds with no stderr",
            output.duration.as_secs()
        ),
        GitExitStatus::OutputLimitExceeded => {
            "git output exceeded the configured limit with no stderr".to_string()
        }
        GitExitStatus::Failed(Some(code)) => {
            format!("git exited with code {code} and wrote no stderr")
        }
        GitExitStatus::Failed(None) => "git failed and wrote no stderr".to_string(),
        GitExitStatus::Success => "git reported success unexpectedly".to_string(),
    };
    AppError::git(context, &detail)
}

pub fn validate_git_pathspecs(paths: &[String]) -> AppResult<Vec<String>> {
    if paths.len() > MAX_IPC_PATHS {
        return Err(AppError::new(
            "invalid_paths",
            "too many paths in one operation",
        ));
    }
    let mut clean = Vec::with_capacity(paths.len());
    let mut seen = BTreeSet::new();
    for path in paths {
        validate_git_pathspec(path)?;
        if seen.insert(path.clone()) {
            clean.push(path.clone());
        }
    }
    Ok(clean)
}

pub fn validate_git_pathspec(path: &str) -> AppResult<()> {
    if path.is_empty() || path.len() > MAX_IPC_PATH_LEN {
        return Err(AppError::new("invalid_path", "path length is invalid"));
    }
    if path.contains('\0') || path.contains(':') || path.contains('*') || path.contains('?') {
        return Err(AppError::new(
            "invalid_path",
            "pathspec magic and glob characters are not allowed",
        ));
    }
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(AppError::new(
            "invalid_path",
            "absolute paths are not allowed",
        ));
    }
    for part in normalized.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(AppError::new(
                "invalid_path",
                "path escapes or targets the repository root",
            ));
        }
    }
    Ok(())
}

pub fn validate_commit_id(commit: &str) -> AppResult<()> {
    if !(7..=64).contains(&commit.len()) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::new(
            "invalid_commit",
            "commit id must be a hex object id",
        ));
    }
    Ok(())
}

pub fn normalize_git_tree_path(path: &str) -> AppResult<String> {
    if path.len() > MAX_IPC_PATH_LEN || path.contains('\0') || path.contains(':') {
        return Err(AppError::new("invalid_path", "path is invalid"));
    }
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains('*') || normalized.contains('?') {
        return Err(AppError::new(
            "invalid_path",
            "absolute paths and glob characters are not allowed",
        ));
    }
    let mut clean = Vec::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(AppError::new(
                "invalid_path",
                "path escapes the commit tree",
            ));
        }
        clean.push(part);
    }
    Ok(clean.join("/"))
}

pub fn commit_treeish(commit: &str, path: &str) -> String {
    if path.is_empty() {
        commit.to_string()
    } else {
        format!("{commit}:{path}")
    }
}

fn log_entry(
    repo_id: Option<String>,
    action: &'static str,
    ok: bool,
    duration_ms: u128,
    message: impl Into<String>,
) -> OperationLogEntry {
    OperationLogEntry {
        epoch_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0),
        repo_id,
        action: action.to_string(),
        ok,
        duration_ms: duration_ms.min(u64::MAX as u128) as u64,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::WatchMode;
    use std::time::Duration;

    fn test_repo(trusted: bool) -> RepoConfig {
        RepoConfig {
            id: "repo".to_string(),
            path: "D:/repo".to_string(),
            label: "repo".to_string(),
            favorite: false,
            trusted,
            watch: WatchMode::Manual,
        }
    }

    #[test]
    fn git_pathspec_validation_allows_deleted_file_paths() {
        let paths = validate_git_pathspecs(&["src/deleted.rs".to_string()]).unwrap();
        assert_eq!(paths, vec!["src/deleted.rs".to_string()]);
    }

    #[test]
    fn git_pathspec_validation_rejects_escape_paths() {
        let err = validate_git_pathspecs(&["../outside.txt".to_string()]).unwrap_err();
        assert_eq!(err.code, "invalid_path");
    }

    #[test]
    fn git_pathspec_validation_rejects_repo_root_targets() {
        for path in [".", "./file.txt", "src//lib.rs"] {
            let err = validate_git_pathspecs(&[path.to_string()]).unwrap_err();
            assert_eq!(err.code, "invalid_path");
        }
    }

    #[test]
    fn git_pathspec_validation_dedupes_paths() {
        let paths =
            validate_git_pathspecs(&["src/lib.rs".to_string(), "src/lib.rs".to_string()]).unwrap();
        assert_eq!(paths, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn commit_id_validation_accepts_hex_prefix() {
        assert!(validate_commit_id("abcdef0").is_ok());
        assert_eq!(
            validate_commit_id("not-hex").unwrap_err().code,
            "invalid_commit"
        );
    }

    #[test]
    fn tree_path_normalization_rejects_escape_paths() {
        let err = normalize_git_tree_path("../outside.txt").unwrap_err();
        assert_eq!(err.code, "invalid_path");
        assert_eq!(
            normalize_git_tree_path("./src//main.rs").unwrap(),
            "src/main.rs"
        );
    }

    #[test]
    fn commit_treeish_formats_root_and_nested_paths() {
        assert_eq!(commit_treeish("abcdef0", ""), "abcdef0");
        assert_eq!(
            commit_treeish("abcdef0", "src/main.rs"),
            "abcdef0:src/main.rs"
        );
    }

    #[test]
    fn require_trusted_repo_blocks_untrusted() {
        let err = require_trusted_repo(&test_repo(false)).unwrap_err();
        assert_eq!(err.code, "repo_untrusted");
        assert!(require_trusted_repo(&test_repo(true)).is_ok());
    }

    #[test]
    fn local_git_config_scan_finds_repo_owned_execution_settings() {
        let config = r#"
            [credential]
              helper = !echo helper
            [credential "https://example.invalid"]
              helper = manager
            [core]
              sshCommand = ssh -i repo.key
              askPass = helper
              hooksPath = .git/hooks
              gitProxy = proxy-command
              fsmonitor = marker-helper
            [commit]
              gpgSign = true
            [gpg]
              program = repo-gpg
            [gpg "ssh"]
              program = repo-ssh-gpg
            [filter "proof"]
              clean = proof clean
              process = proof process
              smudge = proof smudge
            [include]
              path = ../other.conf
            [includeIf "gitdir:../"]
              path = ../conditional.conf
        "#;

        let keys = unsafe_local_git_config_keys(config);
        assert_eq!(
            keys,
            vec![
                "commit.gpgSign",
                "core.askPass",
                "core.fsmonitor",
                "core.gitProxy",
                "core.hooksPath",
                "core.sshCommand",
                "credential.*.helper",
                "credential.helper",
                "filter.*.clean",
                "filter.*.process",
                "filter.*.smudge",
                "gpg.program",
                "gpg.ssh.program",
                "include.path",
                "includeIf.*.path",
            ]
        );
    }

    #[test]
    fn local_git_config_scan_ignores_non_execution_settings() {
        let config = r#"
            [user]
              email = test@example.invalid
            [remote "origin"]
              url = https://example.invalid/repo.git
            [branch "main"]
              merge = refs/heads/main
            [http]
              sslVerify = true
        "#;

        assert!(unsafe_local_git_config_keys(config).is_empty());
    }

    #[test]
    fn fetch_requires_trusted_repo() {
        assert!(remote_operation_requires_trust("fetch"));
        assert!(!remote_operation_requires_trust("refresh"));
    }

    #[test]
    fn git_output_error_includes_status_when_stderr_is_empty() {
        let output = GitOutput {
            status: GitExitStatus::Failed(Some(129)),
            stdout: Vec::new(),
            stderr: Vec::new(),
            duration: Duration::from_secs(1),
            truncated_stdout: false,
            truncated_stderr: false,
        };
        let err = git_output_error("fetch failed", &output);
        assert_eq!(
            err.message,
            "fetch failed: git exited with code 129 and wrote no stderr"
        );
    }

    #[test]
    fn git_output_error_includes_timeout_when_stderr_is_empty() {
        let output = GitOutput {
            status: GitExitStatus::TimedOut,
            stdout: Vec::new(),
            stderr: Vec::new(),
            duration: Duration::from_secs(300),
            truncated_stdout: false,
            truncated_stderr: false,
        };
        let err = git_output_error("fetch failed", &output);
        assert_eq!(
            err.message,
            "fetch failed: git command timed out after 300 seconds with no stderr"
        );
    }
}
