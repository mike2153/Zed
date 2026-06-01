use crate::error::{AppError, AppResult};
use crate::models::{FileStatus, RepoId, RepoStatus, StatusBucket, StatusKind};

pub fn parse_status_z(repo_id: &RepoId, input: &[u8]) -> AppResult<RepoStatus> {
    let mut status = RepoStatus::empty(repo_id.clone());
    let records = input
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    let mut pending_rename: Option<(Vec<String>, StatusKind)> = None;

    for record in records {
        if let Some((fields, kind)) = pending_rename.take() {
            let old_path = bytes_to_string(record)?;
            push_entries(
                &mut status,
                files_from_fields(&fields, kind, Some(old_path))?,
            );
            continue;
        }

        if record.starts_with(b"# ") {
            parse_branch_header(record, &mut status)?;
            continue;
        }

        let text = bytes_to_string(record)?;
        let kind_marker = text.split(' ').next().unwrap_or("");
        match kind_marker {
            "1" => {
                let fields: Vec<String> = text.splitn(9, ' ').map(ToString::to_string).collect();
                push_entries(
                    &mut status,
                    files_from_fields(&fields, StatusKind::Ordinary, None)?,
                );
            }
            "2" => {
                let fields: Vec<String> = text.splitn(10, ' ').map(ToString::to_string).collect();
                let kind = match fields.get(8).and_then(|score| score.chars().next()) {
                    Some('R') => StatusKind::Renamed,
                    Some('C') => StatusKind::Copied,
                    _ => StatusKind::Renamed,
                };
                pending_rename = Some((fields, kind));
            }
            "u" => {
                let fields: Vec<String> = text.splitn(11, ' ').map(ToString::to_string).collect();
                push_entries(
                    &mut status,
                    files_from_fields(&fields, StatusKind::Unmerged, None)?,
                );
            }
            "?" => {
                let path = text.strip_prefix("? ").unwrap_or("").to_string();
                push_entry(
                    &mut status,
                    FileStatus {
                        path,
                        old_path: None,
                        index_status: "?".to_string(),
                        worktree_status: "?".to_string(),
                        kind: StatusKind::Untracked,
                        bucket: StatusBucket::Untracked,
                    },
                );
            }
            "!" => {}
            other => {
                return Err(AppError::new(
                    "parse_status",
                    format!("unknown porcelain v2 record kind '{other}'"),
                ));
            }
        }
    }

    if pending_rename.is_some() {
        return Err(AppError::new(
            "parse_status",
            "rename/copy record missing old path",
        ));
    }

    Ok(status)
}

fn parse_branch_header(record: &[u8], status: &mut RepoStatus) -> AppResult<()> {
    let text = bytes_to_string(record)?;
    if let Some(value) = text.strip_prefix("# branch.head ") {
        status.branch = if value == "(detached)" {
            None
        } else {
            Some(value.to_string())
        };
    } else if let Some(value) = text.strip_prefix("# branch.upstream ") {
        status.upstream = Some(value.to_string());
    } else if let Some(value) = text.strip_prefix("# branch.ab ") {
        for part in value.split(' ') {
            if let Some(ahead) = part.strip_prefix('+') {
                status.ahead = ahead.parse().unwrap_or(0);
            } else if let Some(behind) = part.strip_prefix('-') {
                status.behind = behind.parse().unwrap_or(0);
            }
        }
    }
    Ok(())
}

fn files_from_fields(
    fields: &[String],
    kind: StatusKind,
    old_path: Option<String>,
) -> AppResult<Vec<FileStatus>> {
    let xy = fields
        .get(1)
        .ok_or_else(|| AppError::new("parse_status", "missing XY field"))?;
    let mut chars = xy.chars();
    let index = chars.next().unwrap_or('.');
    let worktree = chars.next().unwrap_or('.');
    let path_index = match kind {
        StatusKind::Unmerged => 10,
        StatusKind::Renamed | StatusKind::Copied => 9,
        _ => 8,
    };
    let path = fields
        .get(path_index)
        .ok_or_else(|| AppError::new("parse_status", "missing path field"))?
        .clone();
    if matches!(kind, StatusKind::Unmerged) || index == 'U' || worktree == 'U' {
        return Ok(vec![FileStatus {
            path,
            old_path,
            index_status: index.to_string(),
            worktree_status: worktree.to_string(),
            kind,
            bucket: StatusBucket::Conflicted,
        }]);
    }

    let mut files = Vec::new();
    if index != '.' {
        files.push(FileStatus {
            path: path.clone(),
            old_path: old_path.clone(),
            index_status: index.to_string(),
            worktree_status: ".".to_string(),
            kind,
            bucket: StatusBucket::Staged,
        });
    }
    if worktree != '.' {
        files.push(FileStatus {
            path,
            old_path: None,
            index_status: ".".to_string(),
            worktree_status: worktree.to_string(),
            kind: StatusKind::Ordinary,
            bucket: StatusBucket::Unstaged,
        });
    }
    Ok(files)
}

fn push_entries(status: &mut RepoStatus, files: Vec<FileStatus>) {
    for file in files {
        push_entry(status, file);
    }
}

fn push_entry(status: &mut RepoStatus, file: FileStatus) {
    status.raw_entry_count += 1;
    match file.bucket {
        StatusBucket::Staged => status.staged.push(file),
        StatusBucket::Unstaged => status.unstaged.push(file),
        StatusBucket::Untracked => status.untracked.push(file),
        StatusBucket::Conflicted => status.conflicted.push(file),
    }
}

fn bytes_to_string(bytes: &[u8]) -> AppResult<String> {
    std::str::from_utf8(bytes)
        .map(ToString::to_string)
        .map_err(|err| AppError::new("parse_status", format!("status output is not utf-8: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &[u8]) -> RepoStatus {
        parse_status_z(&"repo".to_string(), input).unwrap()
    }

    #[test]
    fn parse_empty_clean_status() {
        let status = parse(b"# branch.oid abc\0# branch.head main\0");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.raw_entry_count, 0);
    }

    #[test]
    fn parse_branch_ahead_behind() {
        let status =
            parse(b"# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -3\0");
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 3);
    }

    #[test]
    fn parse_detached_head() {
        let status = parse(b"# branch.head (detached)\0");
        assert_eq!(status.branch, None);
    }

    #[test]
    fn parse_modified_unstaged_file() {
        let status = parse(b"1 .M N... 100644 100644 100644 abc abc file.txt\0");
        assert_eq!(status.unstaged[0].path, "file.txt");
        assert_eq!(status.unstaged[0].worktree_status, "M");
    }

    #[test]
    fn parse_added_staged_file() {
        let status = parse(b"1 A. N... 000000 100644 100644 abc abc new file.txt\0");
        assert_eq!(status.staged[0].path, "new file.txt");
        assert_eq!(status.staged[0].index_status, "A");
    }

    #[test]
    fn parse_untracked_file() {
        let status = parse(b"? --leading-dash.txt\0");
        assert_eq!(status.untracked[0].path, "--leading-dash.txt");
    }

    #[test]
    fn parse_renamed_file_z_record() {
        let status =
            parse(b"2 R. N... 100644 100644 100644 abc def R100 new name.txt\0old name.txt\0");
        assert_eq!(status.staged[0].path, "new name.txt");
        assert_eq!(status.staged[0].old_path.as_deref(), Some("old name.txt"));
        assert_eq!(status.staged[0].kind, StatusKind::Renamed);
    }

    #[test]
    fn parse_copied_file_z_record() {
        let status = parse(b"2 A. N... 100644 100644 100644 abc def C100 copy.txt\0source.txt\0");
        assert_eq!(status.staged[0].path, "copy.txt");
        assert_eq!(status.staged[0].old_path.as_deref(), Some("source.txt"));
        assert_eq!(status.staged[0].kind, StatusKind::Copied);
    }

    #[test]
    fn parse_dual_staged_and_unstaged_file() {
        let status = parse(b"1 MM N... 100644 100644 100644 abc def file.txt\0");
        assert_eq!(status.staged[0].path, "file.txt");
        assert_eq!(status.staged[0].index_status, "M");
        assert_eq!(status.staged[0].worktree_status, ".");
        assert_eq!(status.unstaged[0].path, "file.txt");
        assert_eq!(status.unstaged[0].index_status, ".");
        assert_eq!(status.unstaged[0].worktree_status, "M");
    }

    #[test]
    fn parse_conflicted_unmerged_file() {
        let status = parse(b"u UU N... 100644 100644 100644 100644 a b c conflict.txt\0");
        assert_eq!(status.conflicted[0].path, "conflict.txt");
        assert_eq!(status.conflicted[0].kind, StatusKind::Unmerged);
    }

    #[test]
    fn reject_malformed_record_without_panic() {
        let err = parse_status_z(&"repo".to_string(), b"1 M\0").unwrap_err();
        assert_eq!(err.code, "parse_status");
    }
}
