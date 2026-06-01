use crate::limits::{CONFIG_VERSION, DEFAULT_MAX_GIT_WORKERS};
use serde::{Deserialize, Serialize};

pub type RepoId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub version: u32,
    pub repos: Vec<RepoConfig>,
    pub settings: AppSettings,
    pub last_active_repo: Option<RepoId>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            repos: Vec::new(),
            settings: AppSettings::default(),
            last_active_repo: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoConfig {
    pub id: RepoId,
    pub path: String,
    pub label: String,
    pub favorite: bool,
    #[serde(default)]
    pub trusted: bool,
    pub watch: WatchMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppSettings {
    pub max_git_workers: usize,
    pub watch_mode: WatchMode,
    pub low_memory_mode: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            max_git_workers: DEFAULT_MAX_GIT_WORKERS,
            watch_mode: WatchMode::ActiveRepo,
            low_memory_mode: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WatchMode {
    Manual,
    #[serde(alias = "favorites", alias = "all")]
    ActiveRepo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchDiagnostics {
    pub mode: WatchMode,
    pub backend: String,
    pub active_repo_id: Option<RepoId>,
    pub active_watcher_count: usize,
    pub event_count: u64,
    pub refresh_count: u64,
    pub pending_refresh: bool,
    pub last_event_epoch_ms: Option<u64>,
    pub last_refresh_duration_ms: Option<u64>,
    pub last_error: Option<String>,
    pub degraded: bool,
}

impl Default for WatchDiagnostics {
    fn default() -> Self {
        Self {
            mode: WatchMode::ActiveRepo,
            backend: "manual".to_string(),
            active_repo_id: None,
            active_watcher_count: 0,
            event_count: 0,
            refresh_count: 0,
            pending_refresh: false,
            last_event_epoch_ms: None,
            last_refresh_duration_ms: None,
            last_error: None,
            degraded: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoSummary {
    pub id: RepoId,
    pub label: String,
    pub path: String,
    pub favorite: bool,
    pub trusted: bool,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicts: usize,
    pub is_valid: bool,
    pub stale: bool,
    pub last_error: Option<String>,
    pub last_refresh_epoch_ms: Option<u64>,
}

impl RepoSummary {
    pub fn from_config(repo: &RepoConfig) -> Self {
        Self {
            id: repo.id.clone(),
            label: repo.label.clone(),
            path: repo.path.clone(),
            favorite: repo.favorite,
            trusted: repo.trusted,
            branch: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            staged: 0,
            unstaged: 0,
            untracked: 0,
            conflicts: 0,
            is_valid: true,
            stale: true,
            last_error: None,
            last_refresh_epoch_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoStatus {
    pub repo_id: RepoId,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: Vec<FileStatus>,
    pub unstaged: Vec<FileStatus>,
    pub untracked: Vec<FileStatus>,
    pub conflicted: Vec<FileStatus>,
    pub raw_entry_count: usize,
}

impl RepoStatus {
    pub fn empty(repo_id: RepoId) -> Self {
        Self {
            repo_id,
            branch: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            conflicted: Vec::new(),
            raw_entry_count: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileStatus {
    pub path: String,
    pub old_path: Option<String>,
    pub index_status: String,
    pub worktree_status: String,
    pub kind: StatusKind,
    pub bucket: StatusBucket,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    Ordinary,
    Renamed,
    Copied,
    Unmerged,
    Untracked,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusBucket {
    Staged,
    Unstaged,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffTarget {
    pub repo_id: RepoId,
    pub path: String,
    pub staged: bool,
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffResult {
    pub target: DiffTarget,
    pub files: Vec<DiffFile>,
    pub truncated: bool,
    pub raw_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffFile {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub binary: bool,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffHunk {
    pub old_start: u32,
    pub new_start: u32,
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
    Hunk,
    Meta,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryPage {
    pub repo_id: RepoId,
    pub commits: Vec<CommitSummary>,
    pub lane_count: usize,
    pub next_cursor: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitSummary {
    pub id: String,
    pub short_id: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_time: i64,
    pub refs: Vec<String>,
    pub subject: String,
    pub lane: usize,
    pub graph_edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphEdge {
    pub from_lane: usize,
    pub to_lane: usize,
    pub parent_index: Option<u8>,
    pub color_index: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified_epoch_ms: Option<u64>,
    pub heavy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryListing {
    pub repo_id: RepoId,
    pub path: String,
    pub entries: Vec<TreeEntry>,
    pub next_cursor: Option<usize>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSearchResult {
    pub path: String,
    pub score: i64,
    pub match_positions: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSearchResults {
    pub repo_id: RepoId,
    pub query: String,
    pub results: Vec<FileSearchResult>,
    pub scanned: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilePreview {
    pub repo_id: RepoId,
    pub path: String,
    pub is_binary: bool,
    pub truncated: bool,
    pub bytes_read: usize,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationLogEntry {
    pub epoch_ms: u64,
    pub repo_id: Option<RepoId>,
    pub action: String,
    pub ok: bool,
    pub duration_ms: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoInvalidatedEvent {
    pub repo_id: RepoId,
    pub generation: u64,
    pub reason: String,
    pub event_count: u64,
    pub summary: Option<RepoSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoStatusUpdatedEvent {
    pub repo_id: RepoId,
    pub generation: u64,
    pub summary: RepoSummary,
    pub status: Option<RepoStatus>,
    pub duration_ms: u64,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoRefreshFailedEvent {
    pub repo_id: RepoId,
    pub generation: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatcherStateChangedEvent {
    pub diagnostics: WatchDiagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostics {
    pub repo_count: usize,
    pub trusted_repo_count: usize,
    pub safe_mode_repo_count: usize,
    pub operation_log_count: usize,
    pub max_git_workers: usize,
    pub git_workers_in_use: usize,
    pub git_executable: String,
    pub config_load_error: Option<String>,
    pub low_memory_mode: bool,
    pub watch: WatchDiagnostics,
}
