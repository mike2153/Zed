use crate::error::{AppError, AppResult};
use crate::fs::tree::resolve_repo_relative;
use crate::models::{FileSearchResult, FileSearchResults, RepoId};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_SCAN_ENTRIES: usize = 25_000;
const DEFAULT_RESULT_LIMIT: usize = 60;
const MAX_RESULT_LIMIT: usize = 120;
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

pub fn search_worktree_files(
    repo_id: RepoId,
    repo_path: &Path,
    query: &str,
    limit: Option<usize>,
) -> AppResult<FileSearchResults> {
    let query = query.trim();
    let limit = limit
        .unwrap_or(DEFAULT_RESULT_LIMIT)
        .clamp(1, MAX_RESULT_LIMIT);
    if query.is_empty() {
        return Ok(FileSearchResults {
            repo_id,
            query: String::new(),
            results: Vec::new(),
            scanned: 0,
            truncated: false,
        });
    }

    let root = resolve_repo_relative(repo_path, "")?;
    let mut stack = vec![root.clone()];
    let mut results = Vec::new();
    let mut scanned = 0_usize;
    let mut truncated = false;

    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if dir == root => return Err(AppError::io("read repository root", err)),
            Err(_) => continue,
        };

        for entry in entries {
            if scanned >= MAX_SCAN_ENTRIES {
                truncated = true;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if file_type.is_dir() {
                if !is_heavy_dir(&name) {
                    stack.push(entry.path());
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            scanned += 1;
            let path = normalize_relative_path(&root, entry.path())?;
            if let Some((score, match_positions)) = fuzzy_score(&path, query) {
                results.push(FileSearchResult {
                    path,
                    score,
                    match_positions,
                });
            }
        }

        if truncated {
            break;
        }
    }

    results.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.len().cmp(&b.path.len()))
            .then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase()))
    });
    results.truncate(limit);

    Ok(FileSearchResults {
        repo_id,
        query: query.to_string(),
        results,
        scanned,
        truncated,
    })
}

fn normalize_relative_path(root: &Path, path: PathBuf) -> AppResult<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| AppError::new("invalid_path", "search result escaped repository root"))?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn is_heavy_dir(name: &str) -> bool {
    HEAVY_DIRS.contains(&name)
}

fn fuzzy_score(path: &str, query: &str) -> Option<(i64, Vec<usize>)> {
    let path_lower = path.to_lowercase();
    let query_lower = query.to_lowercase();
    let query_chars: Vec<char> = query_lower.chars().filter(|c| !c.is_whitespace()).collect();
    if query_chars.is_empty() {
        return None;
    }

    if let Some(byte_index) = path_lower.find(&query_lower) {
        let char_index = path_lower[..byte_index].chars().count();
        let len = query_lower.chars().count();
        let mut score = 6_000_i64 - char_index as i64 - path_lower.len() as i64;
        if char_index == 0 || is_boundary(path_lower.chars().nth(char_index.saturating_sub(1))) {
            score += 600;
        }
        return Some((score, (char_index..char_index + len).collect()));
    }

    let mut positions = Vec::with_capacity(query_chars.len());
    let mut score = 1_500_i64 - path_lower.len() as i64;
    let mut query_index = 0_usize;
    let mut last_match: Option<usize> = None;

    for (path_index, ch) in path_lower.chars().enumerate() {
        if query_index >= query_chars.len() {
            break;
        }
        if ch != query_chars[query_index] {
            continue;
        }
        positions.push(path_index);
        score += 95;
        if last_match
            .map(|last| last + 1 == path_index)
            .unwrap_or(false)
        {
            score += 85;
        }
        if path_index == 0 || is_boundary(path_lower.chars().nth(path_index.saturating_sub(1))) {
            score += 55;
        }
        if let Some(last) = last_match {
            score -= path_index.saturating_sub(last + 1) as i64 * 3;
        } else {
            score -= path_index as i64 * 2;
        }
        last_match = Some(path_index);
        query_index += 1;
    }

    (query_index == query_chars.len()).then_some((score, positions))
}

fn is_boundary(ch: Option<char>) -> bool {
    matches!(
        ch,
        None | Some('/') | Some('\\') | Some('-') | Some('_') | Some('.') | Some(' ')
    )
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
        dir.push(format!("lean_git_search_{name}_{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn searches_files_in_repo_scope() {
        let dir = temp_dir("basic");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src").join("main.rs"), "fn main() {}").unwrap();
        fs::write(dir.join("README.md"), "# repo").unwrap();

        let result = search_worktree_files("repo".to_string(), &dir, "main", Some(10)).unwrap();
        assert_eq!(result.results[0].path, "src/main.rs");
        assert!(!result.results[0].match_positions.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn skips_heavy_directories() {
        let dir = temp_dir("heavy");
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::write(dir.join("target").join("match.rs"), "skip").unwrap();
        fs::write(dir.join("match.rs"), "keep").unwrap();

        let result = search_worktree_files("repo".to_string(), &dir, "match", Some(10)).unwrap();
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].path, "match.rs");
        let _ = fs::remove_dir_all(dir);
    }
}
