use crate::error::{AppError, AppResult};
use crate::limits::{MAX_DIFF_LINES, MAX_FILE_PREVIEW_BYTES};
use crate::models::{DiffFile, DiffHunk, DiffLine, DiffLineKind, DiffResult, DiffTarget};

pub fn parse_unified_diff(
    target: DiffTarget,
    input: &[u8],
    truncated: bool,
) -> AppResult<DiffResult> {
    let raw_bytes = input.len();
    let text = String::from_utf8_lossy(input);
    let mut files: Vec<DiffFile> = Vec::new();
    let mut current_file: Option<DiffFile> = None;
    let mut current_hunk: Option<DiffHunk> = None;
    let mut old_lineno = 0_u32;
    let mut new_lineno = 0_u32;
    let mut line_count = 0_usize;
    let mut hit_line_cap = false;

    for line in text.lines() {
        if line_count >= MAX_DIFF_LINES {
            hit_line_cap = true;
            break;
        }
        line_count += 1;

        if line.starts_with("diff --git ") {
            finish_hunk(&mut current_file, &mut current_hunk);
            if let Some(file) = current_file.take() {
                files.push(file);
            }
            current_file = Some(DiffFile {
                old_path: None,
                new_path: None,
                binary: false,
                hunks: Vec::new(),
            });
            continue;
        }

        if current_file.is_none() {
            current_file = Some(DiffFile {
                old_path: None,
                new_path: None,
                binary: false,
                hunks: Vec::new(),
            });
        }

        if let Some(path) = line.strip_prefix("--- ") {
            if let Some(file) = current_file.as_mut() {
                file.old_path = normalize_diff_path(path);
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("+++ ") {
            if let Some(file) = current_file.as_mut() {
                file.new_path = normalize_diff_path(path);
            }
            continue;
        }

        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            if let Some(file) = current_file.as_mut() {
                file.binary = true;
            }
            continue;
        }

        if line.starts_with("@@ ") {
            finish_hunk(&mut current_file, &mut current_hunk);
            let (old_start, new_start) = parse_hunk_header(line)?;
            old_lineno = old_start;
            new_lineno = new_start;
            current_hunk = Some(DiffHunk {
                old_start,
                new_start,
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }

        if let Some(hunk) = current_hunk.as_mut() {
            let (kind, content) = match line.as_bytes().first().copied() {
                Some(b'+') => {
                    let line = DiffLine {
                        kind: DiffLineKind::Added,
                        old_lineno: None,
                        new_lineno: Some(new_lineno),
                        content: line[1..].to_string(),
                    };
                    new_lineno += 1;
                    hunk.lines.push(line);
                    continue;
                }
                Some(b'-') => {
                    let line = DiffLine {
                        kind: DiffLineKind::Removed,
                        old_lineno: Some(old_lineno),
                        new_lineno: None,
                        content: line[1..].to_string(),
                    };
                    old_lineno += 1;
                    hunk.lines.push(line);
                    continue;
                }
                Some(b' ') => (DiffLineKind::Context, &line[1..]),
                _ => (DiffLineKind::Meta, line),
            };

            let diff_line = DiffLine {
                kind,
                old_lineno: if kind == DiffLineKind::Context {
                    Some(old_lineno)
                } else {
                    None
                },
                new_lineno: if kind == DiffLineKind::Context {
                    Some(new_lineno)
                } else {
                    None
                },
                content: content.to_string(),
            };
            if kind == DiffLineKind::Context {
                old_lineno += 1;
                new_lineno += 1;
            }
            hunk.lines.push(diff_line);
        }
    }

    finish_hunk(&mut current_file, &mut current_hunk);
    if let Some(file) = current_file.take() {
        files.push(file);
    }

    Ok(DiffResult {
        target,
        files,
        truncated: truncated || hit_line_cap,
        raw_bytes,
    })
}

fn finish_hunk(current_file: &mut Option<DiffFile>, current_hunk: &mut Option<DiffHunk>) {
    if let (Some(file), Some(hunk)) = (current_file.as_mut(), current_hunk.take()) {
        file.hunks.push(hunk);
    }
}

fn normalize_diff_path(path: &str) -> Option<String> {
    if path == "/dev/null" {
        None
    } else {
        Some(
            path.strip_prefix("a/")
                .or_else(|| path.strip_prefix("b/"))
                .unwrap_or(path)
                .to_string(),
        )
    }
}

fn parse_hunk_header(line: &str) -> AppResult<(u32, u32)> {
    let mut parts = line.split(' ');
    let _marker = parts.next();
    let old = parts
        .next()
        .ok_or_else(|| AppError::new("parse_diff", "missing old hunk range"))?;
    let new = parts
        .next()
        .ok_or_else(|| AppError::new("parse_diff", "missing new hunk range"))?;
    Ok((parse_range_start(old, '-')?, parse_range_start(new, '+')?))
}

fn parse_range_start(range: &str, prefix: char) -> AppResult<u32> {
    let raw = range
        .strip_prefix(prefix)
        .ok_or_else(|| AppError::new("parse_diff", "invalid hunk range prefix"))?;
    let start = raw.split(',').next().unwrap_or(raw);
    start
        .parse()
        .map_err(|_| AppError::new("parse_diff", "invalid hunk line number"))
}

pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .take(MAX_FILE_PREVIEW_BYTES.min(bytes.len()))
        .any(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> DiffTarget {
        DiffTarget {
            repo_id: "repo".to_string(),
            path: "file.txt".to_string(),
            staged: false,
            commit: None,
        }
    }

    #[test]
    fn parses_unified_diff_hunk() {
        let diff = b"diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n old\n-removed\n+added\n";
        let parsed = parse_unified_diff(target(), diff, false).unwrap();
        let hunk = &parsed.files[0].hunks[0];
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.new_start, 1);
        assert_eq!(hunk.lines[1].kind, DiffLineKind::Removed);
        assert_eq!(hunk.lines[2].new_lineno, Some(2));
    }

    #[test]
    fn detects_binary_diff() {
        let diff = b"diff --git a/a.bin b/a.bin\nBinary files a/a.bin and b/a.bin differ\n";
        let parsed = parse_unified_diff(target(), diff, false).unwrap();
        assert!(parsed.files[0].binary);
    }

    #[test]
    fn binary_sample_detection_uses_nul() {
        assert!(looks_binary(b"abc\0def"));
        assert!(!looks_binary(b"plain text"));
    }
}
