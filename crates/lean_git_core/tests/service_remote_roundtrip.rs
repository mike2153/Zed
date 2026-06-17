//! End-to-end coverage of the high-level `AppService` git workflow against a
//! local bare remote: stage -> commit -> push, then fetch -> fast-forward pull.
//!
//! The existing `local_bare_remote.rs` test drives the low-level `GitExec`
//! directly; this test drives the same operations through the public
//! `AppService` API that the GitView UI actually calls (add_repo,
//! set_repo_trusted, stage_paths, commit, push_repo, fetch_repo, pull_ff_only),
//! so a regression in the service layer (trust gating, refresh-after-mutation,
//! upstream configuration) is caught here.

use lean_git_core::git::exec::GitExec;
use lean_git_core::models::AppConfig;
use lean_git_core::service::AppService;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("lean_git_service_remote_{name}_{stamp}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_git_no_repo(args: &[&str]) {
    let output = Command::new("git").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn read_doc(path: &Path) -> String {
    fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

fn test_service(name: &str) -> AppService {
    AppService::new(
        temp_dir(&format!("{name}_config")),
        GitExec::default(),
        AppConfig::default(),
        None,
    )
}

/// Drives the full GitView remote workflow through `AppService`:
///   1. register + trust a cloned working repo,
///   2. edit a doc, stage it, commit it, and push it to the bare remote,
///   3. confirm the remote actually received the commit (via a fresh clone),
///   4. let another clone push a change and pull it back fast-forward only.
#[test]
fn app_service_stage_commit_push_then_pull_against_local_bare_remote() {
    let root = temp_dir("workflow");
    let remote = root.join("remote.git");
    let work = root.join("work");
    let other = root.join("other");

    // A bare remote plus a working clone that already has one commit, so the
    // branch (and its eventual upstream) exists.
    run_git_no_repo(&[
        "init",
        "--bare",
        "--initial-branch=main",
        remote.to_str().unwrap(),
    ]);
    run_git_no_repo(&["clone", remote.to_str().unwrap(), work.to_str().unwrap()]);
    run_git(&work, &["config", "user.email", "test@example.invalid"]);
    run_git(&work, &["config", "user.name", "Lean Git Test"]);
    fs::write(work.join("README.md"), "line one\n").unwrap();
    run_git(&work, &["add", "README.md"]);
    run_git(&work, &["commit", "-m", "docs: initial readme"]);

    let service = test_service("workflow");
    let summary = service.add_repo(&work).unwrap();
    let id = summary.id.clone();
    service.set_active_repo(&id).unwrap();
    service.set_repo_trusted(&id, true).unwrap();

    // Push the initial commit to the bare remote and record the upstream so the
    // later `push_repo(None)` and `pull_ff_only` have an upstream to use.
    let pushed = service
        .push_repo(id.clone(), Some(remote.to_string_lossy().to_string()))
        .unwrap();
    assert_eq!(pushed.raw_entry_count, 0, "tree should be clean after push");

    // Edit a doc, then stage + commit + push it entirely through the service.
    fs::write(work.join("README.md"), "line one\nline two\n").unwrap();
    let staged = service
        .stage_paths(id.clone(), vec!["README.md".to_string()])
        .unwrap();
    assert_eq!(staged.staged[0].path, "README.md");

    let committed = service
        .commit(id.clone(), "docs: add line two".to_string())
        .unwrap();
    assert_eq!(
        committed.raw_entry_count, 0,
        "tree should be clean after commit"
    );

    let after_push = service.push_repo(id.clone(), None).unwrap();
    assert_eq!(after_push.behind, 0, "should not be behind after pushing");

    // The remote must now actually contain the committed doc change.
    run_git_no_repo(&["clone", remote.to_str().unwrap(), other.to_str().unwrap()]);
    assert_eq!(read_doc(&other.join("README.md")), "line one\nline two\n");
    assert_eq!(
        git_stdout(&other, &["log", "-1", "--format=%s"]),
        "docs: add line two"
    );

    // Another clone pushes a change; the service fetches and fast-forwards it in.
    run_git(&other, &["config", "user.email", "test@example.invalid"]);
    run_git(&other, &["config", "user.name", "Lean Git Test"]);
    fs::write(other.join("README.md"), "line one\nline two\nline three\n").unwrap();
    run_git(&other, &["commit", "-am", "docs: add line three"]);
    run_git(&other, &["push"]);

    service.fetch_repo(id.clone()).unwrap();
    let pulled = service.pull_ff_only(id.clone()).unwrap();
    assert_eq!(pulled.behind, 0, "should be up to date after pull");
    assert_eq!(
        read_doc(&work.join("README.md")),
        "line one\nline two\nline three\n"
    );

    let _ = fs::remove_dir_all(root);
}

/// Untrusted repos must not be able to push, even with a valid remote URL: the
/// trust gate is the security boundary the UI relies on.
#[test]
fn app_service_push_blocked_when_repo_is_untrusted() {
    let root = temp_dir("untrusted_push");
    let remote = root.join("remote.git");
    let work = root.join("work");

    run_git_no_repo(&[
        "init",
        "--bare",
        "--initial-branch=main",
        remote.to_str().unwrap(),
    ]);
    run_git_no_repo(&["clone", remote.to_str().unwrap(), work.to_str().unwrap()]);
    run_git(&work, &["config", "user.email", "test@example.invalid"]);
    run_git(&work, &["config", "user.name", "Lean Git Test"]);
    fs::write(work.join("README.md"), "line one\n").unwrap();
    run_git(&work, &["add", "README.md"]);
    run_git(&work, &["commit", "-m", "docs: initial readme"]);

    let service = test_service("untrusted_push");
    let summary = service.add_repo(&work).unwrap();

    // Never trusted -> push must fail closed before contacting the remote.
    let err = service
        .push_repo(summary.id, Some(remote.to_string_lossy().to_string()))
        .unwrap_err();
    assert_eq!(err.code, "repo_untrusted");

    let _ = fs::remove_dir_all(root);
}
