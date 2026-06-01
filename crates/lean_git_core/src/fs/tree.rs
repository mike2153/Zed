use crate::error::{AppError, AppResult};
use crate::limits::MAX_DIRECTORY_ENTRIES;
use crate::models::{DirectoryListing, RepoId, TreeEntry};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const HEAVY_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".venv",
];

pub fn list_worktree_dir(
    repo_id: RepoId,
    repo_path: &Path,
    relative_path: &str,
    cursor: usize,
    limit: usize,
) -> AppResult<DirectoryListing> {
    let limit = limit.min(MAX_DIRECTORY_ENTRIES).max(1);
    let end = cursor.saturating_add(limit);
    let dir = resolve_repo_relative(repo_path, relative_path)?;
    let mut best = BinaryHeap::new();
    let mut total = 0_usize;

    for entry in fs::read_dir(&dir).map_err(|err| AppError::io("read directory", err))? {
        let entry = entry.map_err(|err| AppError::io("read directory entry", err))?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name == ".git" {
            continue;
        }
        total += 1;
        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().map(|meta| meta.is_dir()).unwrap_or(false);
        let path = if relative_path.is_empty() {
            file_name.clone()
        } else {
            format!("{}/{}", relative_path.replace('\\', "/"), file_name)
        };
        let tree_entry = TreeEntry {
            name: file_name.clone(),
            path,
            is_dir,
            size: metadata
                .as_ref()
                .filter(|meta| meta.is_file())
                .map(|meta| meta.len()),
            modified_epoch_ms: metadata
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64),
            heavy: is_dir && HEAVY_DIRS.contains(&file_name.as_str()),
        };
        let ranked = RankedEntry::new(total, tree_entry);
        if best.len() < end {
            best.push(ranked);
        } else if best.peek().map(|worst| ranked < *worst).unwrap_or(true) {
            let _ = best.pop();
            best.push(ranked);
        }
    }

    let mut entries: Vec<TreeEntry> = best.into_iter().map(|ranked| ranked.entry).collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let paged: Vec<TreeEntry> = entries.into_iter().skip(cursor).take(limit).collect();
    let next = cursor + paged.len();
    Ok(DirectoryListing {
        repo_id,
        path: relative_path.to_string(),
        entries: paged,
        next_cursor: (next < total).then_some(next),
        truncated: next < total,
    })
}

#[derive(Debug, Eq, PartialEq)]
struct RankedEntry {
    is_file: bool,
    lower_name: String,
    tie: usize,
    entry: TreeEntry,
}

impl RankedEntry {
    fn new(tie: usize, entry: TreeEntry) -> Self {
        Self {
            is_file: !entry.is_dir,
            lower_name: entry.name.to_lowercase(),
            tie,
            entry,
        }
    }
}

impl Ord for RankedEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.is_file
            .cmp(&other.is_file)
            .then_with(|| self.lower_name.cmp(&other.lower_name))
            .then_with(|| self.tie.cmp(&other.tie))
    }
}

impl PartialOrd for RankedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub fn resolve_repo_relative(repo_path: &Path, relative_path: &str) -> AppResult<PathBuf> {
    let relative = Path::new(relative_path);
    if relative.is_absolute() {
        return Err(AppError::new(
            "invalid_path",
            "absolute paths are not allowed inside repository operations",
        ));
    }

    let root = repo_path
        .canonicalize()
        .map_err(|err| AppError::io("canonicalize repo path", err))?;
    let joined = root.join(relative);
    let resolved = joined
        .canonicalize()
        .map_err(|err| AppError::io("canonicalize repository child path", err))?;
    if !resolved.starts_with(&root) {
        return Err(AppError::new(
            "invalid_path",
            "path escapes the repository root",
        ));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!("lean_git_tree_{name}_{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_directories_first_and_marks_heavy() {
        let dir = temp_dir("order");
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::write(dir.join("a.txt"), "a").unwrap();
        let listing = list_worktree_dir("repo".to_string(), &dir, "", 0, 10).unwrap();
        assert_eq!(listing.entries[0].name, "node_modules");
        assert!(listing.entries[0].heavy);
        assert_eq!(listing.entries[1].name, "a.txt");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn paginates_large_directories() {
        let dir = temp_dir("page");
        fs::write(dir.join("a.txt"), "a").unwrap();
        fs::write(dir.join("b.txt"), "b").unwrap();
        let listing = list_worktree_dir("repo".to_string(), &dir, "", 0, 1).unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.next_cursor, Some(1));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_paths_that_escape_repo_root() {
        let dir = temp_dir("escape");
        let err = resolve_repo_relative(&dir, "..").unwrap_err();
        assert_eq!(err.code, "invalid_path");
        let _ = fs::remove_dir_all(dir);
    }
}
