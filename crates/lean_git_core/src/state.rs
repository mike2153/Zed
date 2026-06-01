use crate::limits::{MAX_DIRECTORY_CACHE, MAX_OPERATION_LOG_MESSAGE_BYTES};
use crate::models::{
    AppConfig, Diagnostics, DiffResult, DirectoryListing, FilePreview, HistoryPage,
    OperationLogEntry, RepoId, RepoInvalidatedEvent, RepoRefreshFailedEvent, RepoStatus,
    RepoStatusUpdatedEvent, WatchDiagnostics, WatcherStateChangedEvent,
};
use crate::repo::RepoRegistry;
use std::collections::{BTreeMap, VecDeque};

pub struct AppState {
    pub config: AppConfig,
    pub registry: RepoRegistry,
    pub active_repo: Option<RepoId>,
    pub active_status: Option<RepoStatus>,
    pub active_history: Option<HistoryPage>,
    pub selected_diff: Option<DiffResult>,
    pub selected_preview: Option<FilePreview>,
    pub directory_cache: BTreeMap<String, DirectoryListing>,
    pub operation_log: VecDeque<OperationLogEntry>,
    pub watch: WatchDiagnostics,
    watch_generation: u64,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let registry = RepoRegistry::new(config.repos.clone());
        let watch_mode = config.settings.watch_mode;
        Self {
            active_repo: config.last_active_repo.clone(),
            config,
            registry,
            active_status: None,
            active_history: None,
            selected_diff: None,
            selected_preview: None,
            directory_cache: BTreeMap::new(),
            operation_log: VecDeque::new(),
            watch: WatchDiagnostics {
                mode: watch_mode,
                ..WatchDiagnostics::default()
            },
            watch_generation: 0,
        }
    }

    pub fn set_active_repo(&mut self, repo_id: Option<RepoId>) {
        if self.active_repo != repo_id {
            self.active_status = None;
            self.active_history = None;
            self.selected_diff = None;
            self.selected_preview = None;
            self.directory_cache.clear();
        }
        self.active_repo = repo_id.clone();
        self.config.last_active_repo = repo_id;
    }

    pub fn push_log(&mut self, entry: OperationLogEntry) {
        let entry = truncate_log_entry(entry);
        while self.operation_log.len() >= crate::limits::MAX_OPERATION_LOG {
            self.operation_log.pop_front();
        }
        self.operation_log.push_back(entry);
    }

    pub fn cache_directory(&mut self, key: String, listing: DirectoryListing) {
        while self.directory_cache.len() >= MAX_DIRECTORY_CACHE
            && !self.directory_cache.contains_key(&key)
        {
            let Some(oldest_key) = self.directory_cache.keys().next().cloned() else {
                break;
            };
            self.directory_cache.remove(&oldest_key);
        }
        self.directory_cache.insert(key, listing);
    }

    pub fn trim_caches(&mut self) {
        self.active_history = None;
        self.selected_diff = None;
        self.selected_preview = None;
        self.directory_cache.clear();
    }

    pub fn set_watcher_state(
        &mut self,
        backend: impl Into<String>,
        active_repo_id: Option<RepoId>,
        active_watcher_count: usize,
        degraded: bool,
    ) -> WatcherStateChangedEvent {
        self.watch.mode = self.config.settings.watch_mode;
        self.watch.backend = backend.into();
        self.watch.active_repo_id = active_repo_id;
        self.watch.active_watcher_count = active_watcher_count;
        self.watch.degraded = degraded;
        if !degraded {
            self.watch.last_error = None;
        }
        WatcherStateChangedEvent {
            diagnostics: self.watch.clone(),
        }
    }

    pub fn mark_repo_stale(
        &mut self,
        repo_id: &str,
        reason: impl Into<String>,
    ) -> RepoInvalidatedEvent {
        self.watch_generation = self.watch_generation.saturating_add(1);
        self.watch.event_count = self.watch.event_count.saturating_add(1);
        self.watch.pending_refresh = true;
        self.watch.last_event_epoch_ms = Some(now_epoch_ms());
        if self.active_repo.as_deref() == Some(repo_id) {
            self.active_history = None;
            self.selected_diff = None;
            self.selected_preview = None;
        }
        self.directory_cache
            .retain(|key, _| !key.starts_with(&format!("{repo_id}:")));
        let summary = self.registry.mark_stale(repo_id, None);
        RepoInvalidatedEvent {
            repo_id: repo_id.to_string(),
            generation: self.watch_generation,
            reason: reason.into(),
            event_count: self.watch.event_count,
            summary,
        }
    }

    pub fn apply_background_status(
        &mut self,
        repo_id: &str,
        status: RepoStatus,
        duration_ms: u64,
    ) -> Option<RepoStatusUpdatedEvent> {
        self.registry.update_status_summary(repo_id, &status);
        self.watch.refresh_count = self.watch.refresh_count.saturating_add(1);
        self.watch.pending_refresh = false;
        self.watch.last_error = None;
        self.watch.last_refresh_duration_ms = Some(duration_ms);
        let active = self.active_repo.as_deref() == Some(repo_id);
        if active {
            self.active_status = Some(status.clone());
        }
        Some(RepoStatusUpdatedEvent {
            repo_id: repo_id.to_string(),
            generation: self.watch_generation,
            summary: self.registry.summary(repo_id)?,
            status: active.then_some(status),
            duration_ms,
            source: "watcher".to_string(),
        })
    }

    pub fn mark_background_refresh_failed(
        &mut self,
        repo_id: &str,
        message: impl Into<String>,
    ) -> RepoRefreshFailedEvent {
        let message = message.into();
        self.registry.mark_error(repo_id, message.clone());
        self.watch.pending_refresh = false;
        self.watch.last_error = Some(message.clone());
        RepoRefreshFailedEvent {
            repo_id: repo_id.to_string(),
            generation: self.watch_generation,
            message,
        }
    }

    pub fn diagnostics(&self) -> Diagnostics {
        let repos = self.registry.list();
        let trusted_repo_count = repos.iter().filter(|repo| repo.trusted).count();
        Diagnostics {
            repo_count: repos.len(),
            trusted_repo_count,
            safe_mode_repo_count: repos.len().saturating_sub(trusted_repo_count),
            operation_log_count: self.operation_log.len(),
            max_git_workers: self.config.settings.max_git_workers,
            git_workers_in_use: 0,
            git_executable: String::new(),
            config_load_error: None,
            low_memory_mode: self.config.settings.low_memory_mode,
            watch: self.watch.clone(),
        }
    }
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn truncate_log_entry(mut entry: OperationLogEntry) -> OperationLogEntry {
    if entry.message.len() <= MAX_OPERATION_LOG_MESSAGE_BYTES {
        return entry;
    }
    let suffix = "...";
    let mut end = MAX_OPERATION_LOG_MESSAGE_BYTES.saturating_sub(suffix.len());
    while !entry.message.is_char_boundary(end) {
        end -= 1;
    }
    entry.message.truncate(end);
    entry.message.push_str(suffix);
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DiffTarget, RepoConfig, WatchMode};

    #[test]
    fn switching_active_repo_drops_large_active_data() {
        let mut state = AppState::new(AppConfig::default());
        state.set_active_repo(Some("a".to_string()));
        state.selected_preview = Some(FilePreview {
            repo_id: "a".to_string(),
            path: "file.txt".to_string(),
            is_binary: false,
            truncated: false,
            bytes_read: 4,
            text: Some("test".to_string()),
        });
        state.selected_diff = Some(DiffResult {
            target: DiffTarget {
                repo_id: "a".to_string(),
                path: "file.txt".to_string(),
                staged: false,
                commit: None,
            },
            files: Vec::new(),
            truncated: false,
            raw_bytes: 0,
        });

        state.set_active_repo(Some("b".to_string()));
        assert!(state.selected_preview.is_none());
        assert!(state.selected_diff.is_none());
    }

    #[test]
    fn operation_log_message_is_byte_capped() {
        let mut state = AppState::new(AppConfig::default());
        state.push_log(OperationLogEntry {
            epoch_ms: 0,
            repo_id: None,
            action: "test".to_string(),
            ok: false,
            duration_ms: 0,
            message: "x".repeat(MAX_OPERATION_LOG_MESSAGE_BYTES + 100),
        });
        assert!(state.operation_log[0].message.len() <= MAX_OPERATION_LOG_MESSAGE_BYTES);
    }

    #[test]
    fn diagnostics_counts_trusted_and_safe_mode_repos() {
        let mut config = AppConfig::default();
        config.repos.push(RepoConfig {
            id: "trusted".to_string(),
            path: "D:/trusted".to_string(),
            label: "trusted".to_string(),
            favorite: false,
            trusted: true,
            watch: WatchMode::Manual,
        });
        config.repos.push(RepoConfig {
            id: "safe".to_string(),
            path: "D:/safe".to_string(),
            label: "safe".to_string(),
            favorite: false,
            trusted: false,
            watch: WatchMode::Manual,
        });
        let state = AppState::new(config);
        let diagnostics = state.diagnostics();
        assert_eq!(diagnostics.repo_count, 2);
        assert_eq!(diagnostics.trusted_repo_count, 1);
        assert_eq!(diagnostics.safe_mode_repo_count, 1);
    }

    #[test]
    fn directory_cache_is_capped() {
        let mut state = AppState::new(AppConfig::default());
        for index in 0..(MAX_DIRECTORY_CACHE + 2) {
            state.cache_directory(
                format!("key-{index:03}"),
                DirectoryListing {
                    repo_id: "repo".to_string(),
                    path: index.to_string(),
                    entries: Vec::new(),
                    next_cursor: None,
                    truncated: false,
                },
            );
        }
        assert_eq!(state.directory_cache.len(), MAX_DIRECTORY_CACHE);
    }

    #[test]
    fn mark_repo_stale_clears_repo_scoped_caches() {
        let mut config = AppConfig::default();
        config.repos.push(RepoConfig {
            id: "repo".to_string(),
            path: "D:/repo".to_string(),
            label: "repo".to_string(),
            favorite: false,
            trusted: false,
            watch: WatchMode::ActiveRepo,
        });
        let mut state = AppState::new(config);
        state.set_active_repo(Some("repo".to_string()));
        state.cache_directory(
            "repo:src".to_string(),
            DirectoryListing {
                repo_id: "repo".to_string(),
                path: "src".to_string(),
                entries: Vec::new(),
                next_cursor: None,
                truncated: false,
            },
        );
        state.selected_preview = Some(FilePreview {
            repo_id: "repo".to_string(),
            path: "README.md".to_string(),
            is_binary: false,
            truncated: false,
            bytes_read: 0,
            text: Some(String::new()),
        });
        let event = state.mark_repo_stale("repo", "filesystem");
        assert_eq!(event.repo_id, "repo");
        assert!(event.summary.unwrap().stale);
        assert!(state.directory_cache.is_empty());
        assert!(state.selected_preview.is_none());
    }

    #[test]
    fn trim_caches_drops_heavy_view_state() {
        let mut state = AppState::new(AppConfig::default());
        state.selected_preview = Some(FilePreview {
            repo_id: "repo".to_string(),
            path: "README.md".to_string(),
            is_binary: false,
            truncated: false,
            bytes_read: 0,
            text: Some(String::new()),
        });
        state.cache_directory(
            "repo:".to_string(),
            DirectoryListing {
                repo_id: "repo".to_string(),
                path: String::new(),
                entries: Vec::new(),
                next_cursor: None,
                truncated: false,
            },
        );
        state.trim_caches();
        assert!(state.selected_preview.is_none());
        assert!(state.directory_cache.is_empty());
    }
}
