use crate::error::{AppError, AppResult};
use crate::limits::MAX_DIRECTORY_ENTRIES;
use crate::models::{DirectoryListing, RepoId, TreeEntry};

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

pub fn parse_ls_tree_z(
    repo_id: RepoId,
    base_path: &str,
    bytes: &[u8],
    cursor: usize,
    limit: usize,
) -> AppResult<DirectoryListing> {
    let limit = limit.min(MAX_DIRECTORY_ENTRIES).max(1);
    let mut entries = Vec::new();
    for record in bytes.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        entries.push(parse_record(base_path, record)?);
    }

    let total = entries.len();
    let paged: Vec<TreeEntry> = entries.into_iter().skip(cursor).take(limit).collect();
    let next = cursor + paged.len();
    Ok(DirectoryListing {
        repo_id,
        path: base_path.to_string(),
        entries: paged,
        next_cursor: (next < total).then_some(next),
        truncated: next < total,
    })
}

fn parse_record(base_path: &str, record: &[u8]) -> AppResult<TreeEntry> {
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| AppError::new("tree_parse", "ls-tree record is missing a path"))?;
    let meta = String::from_utf8_lossy(&record[..tab]);
    let mut parts = meta.split_whitespace();
    let _mode = parts
        .next()
        .ok_or_else(|| AppError::new("tree_parse", "ls-tree record is missing mode"))?;
    let kind = parts
        .next()
        .ok_or_else(|| AppError::new("tree_parse", "ls-tree record is missing type"))?;
    let _object = parts
        .next()
        .ok_or_else(|| AppError::new("tree_parse", "ls-tree record is missing object id"))?;
    let size = parts.next().and_then(|value| value.parse::<u64>().ok());
    let name = String::from_utf8_lossy(&record[tab + 1..]).to_string();
    let path = join_tree_path(base_path, &name);
    let is_dir = kind == "tree";

    Ok(TreeEntry {
        name: name.clone(),
        path,
        is_dir,
        size: (!is_dir).then_some(size).flatten(),
        modified_epoch_ms: None,
        heavy: is_dir && HEAVY_DIRS.contains(&name.as_str()),
    })
}

fn join_tree_path(base_path: &str, name: &str) -> String {
    if base_path.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", base_path.trim_matches('/'), name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blob_and_tree_records() {
        let bytes = b"040000 tree abcdef -\tsrc\0100644 blob 123456 12\tREADME.md\0";
        let listing = parse_ls_tree_z("repo".to_string(), "", bytes, 0, 10).unwrap();

        assert_eq!(listing.entries.len(), 2);
        assert_eq!(listing.entries[0].name, "src");
        assert!(listing.entries[0].is_dir);
        assert_eq!(listing.entries[1].size, Some(12));
    }

    #[test]
    fn prefixes_nested_paths_and_paginates() {
        let bytes = b"100644 blob 1 1\ta.txt\0100644 blob 2 1\tb.txt\0";
        let listing = parse_ls_tree_z("repo".to_string(), "src", bytes, 1, 1).unwrap();

        assert_eq!(listing.entries[0].path, "src/b.txt");
        assert_eq!(listing.next_cursor, None);
    }
}
