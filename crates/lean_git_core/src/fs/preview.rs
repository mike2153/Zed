use crate::error::{AppError, AppResult};
use crate::fs::tree::resolve_repo_relative;
use crate::git::diff::looks_binary;
use crate::limits::MAX_FILE_PREVIEW_BYTES;
use crate::models::{FilePreview, RepoId};
use std::fs;
use std::io::Read;
use std::path::Path;

pub fn read_worktree_file(
    repo_id: RepoId,
    repo_path: &Path,
    relative_path: &str,
) -> AppResult<FilePreview> {
    let path = resolve_repo_relative(repo_path, relative_path)?;
    let mut file = fs::File::open(&path).map_err(|err| AppError::io("open file preview", err))?;
    let mut buffer = Vec::with_capacity(MAX_FILE_PREVIEW_BYTES);
    let bytes_read = file
        .by_ref()
        .take(MAX_FILE_PREVIEW_BYTES as u64 + 1)
        .read_to_end(&mut buffer)
        .map_err(|err| AppError::io("read file preview", err))?;
    let truncated = bytes_read > MAX_FILE_PREVIEW_BYTES;
    if truncated {
        buffer.truncate(MAX_FILE_PREVIEW_BYTES);
    }
    let is_binary = looks_binary(&buffer);
    let text = if is_binary {
        None
    } else {
        Some(String::from_utf8_lossy(&buffer).to_string())
    };
    Ok(FilePreview {
        repo_id,
        path: relative_path.to_string(),
        is_binary,
        truncated,
        bytes_read: buffer.len(),
        text,
    })
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
        dir.push(format!("lean_git_preview_{name}_{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_text_preview() {
        let dir = temp_dir("text");
        fs::write(dir.join("file.txt"), "hello").unwrap();
        let preview = read_worktree_file("repo".to_string(), &dir, "file.txt").unwrap();
        assert!(!preview.is_binary);
        assert_eq!(preview.text.as_deref(), Some("hello"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_binary_preview() {
        let dir = temp_dir("binary");
        fs::write(dir.join("file.bin"), b"abc\0def").unwrap();
        let preview = read_worktree_file("repo".to_string(), &dir, "file.bin").unwrap();
        assert!(preview.is_binary);
        assert!(preview.text.is_none());
        let _ = fs::remove_dir_all(dir);
    }
}
