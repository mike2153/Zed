use crate::models::{RepoConfig, WatchMode};

const WATCH_IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".venv",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTarget {
    pub repo_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebounceDecision {
    Waiting,
    Ready { event_count: u64 },
}

#[derive(Debug, Clone)]
pub struct Debouncer {
    delay_ms: u64,
    last_event_ms: Option<u64>,
    event_count: u64,
}

impl Debouncer {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            last_event_ms: None,
            event_count: 0,
        }
    }

    pub fn push_event(&mut self, now_ms: u64) {
        self.last_event_ms = Some(now_ms);
        self.event_count = self.event_count.saturating_add(1);
    }

    pub fn poll(&mut self, now_ms: u64) -> DebounceDecision {
        let Some(last_event_ms) = self.last_event_ms else {
            return DebounceDecision::Waiting;
        };
        if now_ms.saturating_sub(last_event_ms) < self.delay_ms {
            return DebounceDecision::Waiting;
        }
        let event_count = self.event_count;
        self.last_event_ms = None;
        self.event_count = 0;
        DebounceDecision::Ready { event_count }
    }
}

pub fn select_watch_target(
    mode: WatchMode,
    active_repo_id: Option<&str>,
    repos: &[RepoConfig],
) -> Option<WatchTarget> {
    if mode == WatchMode::Manual {
        return None;
    }
    let active_repo_id = active_repo_id?;
    let repo = repos.iter().find(|repo| repo.id == active_repo_id)?;
    if repo.watch == WatchMode::Manual {
        return None;
    }
    Some(WatchTarget {
        repo_id: repo.id.clone(),
        path: repo.path.clone(),
    })
}

pub fn is_ignored_watch_path(relative_path: &str) -> bool {
    relative_path
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty())
        .any(|part| {
            WATCH_IGNORED_DIRS
                .iter()
                .any(|ignored| part.eq_ignore_ascii_case(ignored))
        })
}

pub fn has_relevant_watch_path(paths: &[String]) -> bool {
    paths.is_empty() || paths.iter().any(|path| !is_ignored_watch_path(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(id: &str, watch: WatchMode) -> RepoConfig {
        RepoConfig {
            id: id.to_string(),
            path: format!("D:/{id}"),
            label: id.to_string(),
            favorite: false,
            trusted: false,
            watch,
        }
    }

    #[test]
    fn manual_mode_has_no_watch_target() {
        let repos = vec![repo("a", WatchMode::ActiveRepo)];
        assert!(select_watch_target(WatchMode::Manual, Some("a"), &repos).is_none());
    }

    #[test]
    fn active_repo_mode_selects_active_repo() {
        let repos = vec![
            repo("a", WatchMode::ActiveRepo),
            repo("b", WatchMode::ActiveRepo),
        ];
        let target = select_watch_target(WatchMode::ActiveRepo, Some("b"), &repos).unwrap();
        assert_eq!(target.repo_id, "b");
        assert_eq!(target.path, "D:/b");
    }

    #[test]
    fn repo_manual_watch_overrides_global_active_mode() {
        let repos = vec![repo("a", WatchMode::Manual)];
        assert!(select_watch_target(WatchMode::ActiveRepo, Some("a"), &repos).is_none());
    }

    #[test]
    fn debouncer_coalesces_event_storm() {
        let mut debouncer = Debouncer::new(500);
        debouncer.push_event(1_000);
        debouncer.push_event(1_100);
        assert_eq!(debouncer.poll(1_550), DebounceDecision::Waiting);
        assert_eq!(
            debouncer.poll(1_601),
            DebounceDecision::Ready { event_count: 2 }
        );
        assert_eq!(debouncer.poll(2_000), DebounceDecision::Waiting);
    }

    #[test]
    fn watcher_filters_heavy_directory_events() {
        assert!(is_ignored_watch_path("target/debug/app.exe"));
        assert!(is_ignored_watch_path("src\\node_modules\\pkg\\index.js"));
        assert!(is_ignored_watch_path(".next/cache/file"));
        assert!(!is_ignored_watch_path("src/targeted.rs"));
        assert!(!is_ignored_watch_path(".git/HEAD"));
    }

    #[test]
    fn watcher_batch_is_relevant_when_any_path_is_relevant() {
        assert!(!has_relevant_watch_path(&[
            "target/debug/app.exe".to_string(),
            ".next/cache/file".to_string()
        ]));
        assert!(has_relevant_watch_path(&[
            "target/debug/app.exe".to_string(),
            "src/main.rs".to_string()
        ]));
        assert!(has_relevant_watch_path(&[]));
    }
}
