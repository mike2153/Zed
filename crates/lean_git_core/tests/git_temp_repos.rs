use lean_git_core::git::exec::GitExec;
use lean_git_core::git::exec::GitRunOptions;
use lean_git_core::git::exec::{safe_mutation_args, safe_read_only_args};
use lean_git_core::git::history::parse_history_page;
use lean_git_core::git::tree::parse_ls_tree_z;
use lean_git_core::limits::CommandLimits;
use lean_git_core::models::AppConfig;
use lean_git_core::repo::{refresh_repo_status, validate_repo_root};
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
    dir.push(format!("lean_git_integration_{name}_{stamp}"));
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

fn init_repo(name: &str) -> PathBuf {
    let repo = temp_dir(name);
    run_git(&repo, &["init"]);
    run_git(&repo, &["config", "user.email", "test@example.invalid"]);
    run_git(&repo, &["config", "user.name", "Lean Git Test"]);
    fs::write(repo.join("tracked.txt"), "one\n").unwrap();
    run_git(&repo, &["add", "tracked.txt"]);
    run_git(&repo, &["commit", "-m", "initial"]);
    repo
}

fn test_service(name: &str) -> AppService {
    AppService::new(
        temp_dir(&format!("{name}_config")),
        GitExec::default(),
        AppConfig::default(),
        None,
    )
}

fn repo_config(repo: &Path) -> lean_git_core::models::RepoConfig {
    lean_git_core::models::RepoConfig {
        id: "repo".to_string(),
        path: repo.to_string_lossy().to_string(),
        label: "repo".to_string(),
        favorite: false,
        trusted: true,
        watch: lean_git_core::models::WatchMode::Manual,
    }
}

#[test]
fn validates_repo_root_with_spaces() {
    let repo = init_repo("repo with spaces");
    let nested = repo.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let root = validate_repo_root(&GitExec::default(), &nested).unwrap();
    assert_eq!(root.canonicalize().unwrap(), repo.canonicalize().unwrap());
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn status_clean_repo() {
    let repo = init_repo("clean");
    let status = refresh_repo_status(&GitExec::default(), &repo_config(&repo)).unwrap();
    assert_eq!(status.raw_entry_count, 0);
    assert!(matches!(
        status.branch.as_deref(),
        Some("master") | Some("main")
    ));
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn status_modified_and_untracked_files() {
    let repo = init_repo("modified");
    fs::write(repo.join("tracked.txt"), "two\n").unwrap();
    fs::write(repo.join("--new file.txt"), "new\n").unwrap();
    let status = refresh_repo_status(&GitExec::default(), &repo_config(&repo)).unwrap();
    assert_eq!(status.unstaged[0].path, "tracked.txt");
    assert_eq!(status.untracked[0].path, "--new file.txt");
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn status_staged_file() {
    let repo = init_repo("staged");
    fs::write(repo.join("staged.txt"), "staged\n").unwrap();
    run_git(&repo, &["add", "staged.txt"]);
    let status = refresh_repo_status(&GitExec::default(), &repo_config(&repo)).unwrap();
    assert_eq!(status.staged[0].path, "staged.txt");
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn status_rename_file() {
    let repo = init_repo("rename");
    run_git(&repo, &["mv", "tracked.txt", "renamed file.txt"]);
    let status = refresh_repo_status(&GitExec::default(), &repo_config(&repo)).unwrap();
    assert_eq!(status.staged[0].path, "renamed file.txt");
    assert_eq!(status.staged[0].old_path.as_deref(), Some("tracked.txt"));
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn lists_committed_tree_with_git_ls_tree() {
    let repo = init_repo("tree");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    run_git(&repo, &["add", "src/main.rs"]);
    run_git(&repo, &["commit", "-m", "add src"]);
    let git = GitExec::default();
    let output = git
        .run(GitRunOptions {
            repo: Some(repo.clone()),
            args: vec![
                "ls-tree".to_string(),
                "-z".to_string(),
                "-l".to_string(),
                "HEAD:src".to_string(),
            ],
            limits: CommandLimits::tree(),
        })
        .unwrap();

    assert!(output.status.success());
    let listing = parse_ls_tree_z("repo".to_string(), "src", &output.stdout, 0, 10).unwrap();
    assert_eq!(listing.entries[0].path, "src/main.rs");
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn history_graph_handles_real_branch_merge() {
    let repo = init_repo("history_merge");
    let default_branch = git_stdout(&repo, &["branch", "--show-current"]);
    fs::write(repo.join("main.txt"), "main\n").unwrap();
    run_git(&repo, &["add", "main.txt"]);
    run_git(&repo, &["commit", "-m", "main"]);
    run_git(&repo, &["branch", "feature"]);
    run_git(&repo, &["checkout", "feature"]);
    fs::write(repo.join("feature.txt"), "feature\n").unwrap();
    run_git(&repo, &["add", "feature.txt"]);
    run_git(&repo, &["commit", "-m", "feature"]);
    run_git(&repo, &["checkout", &default_branch]);
    fs::write(repo.join("after-feature.txt"), "main after branch\n").unwrap();
    run_git(&repo, &["add", "after-feature.txt"]);
    run_git(&repo, &["commit", "-m", "main after branch"]);
    run_git(
        &repo,
        &["merge", "--no-ff", "feature", "-m", "merge feature"],
    );

    let output = GitExec::default()
        .run(GitRunOptions {
            repo: Some(repo.clone()),
            args: safe_read_only_args(vec![
                "log".to_string(),
                "--topo-order".to_string(),
                "--decorate=short".to_string(),
                "--parents".to_string(),
                "--max-count=20".to_string(),
                "--format=%H%x1f%P%x1f%an%x1f%at%x1f%D%x1f%s".to_string(),
            ]),
            limits: CommandLimits::standard(),
        })
        .unwrap();

    assert!(output.status.success());
    let page = parse_history_page(&"repo".to_string(), &output.stdout, 20).unwrap();
    let merge = page
        .commits
        .iter()
        .find(|commit| commit.subject == "merge feature")
        .unwrap();
    assert_eq!(merge.parents.len(), 2);
    assert!(
        merge
            .graph_edges
            .iter()
            .any(|edge| { edge.parent_index == Some(1) && edge.from_lane != edge.to_lane })
    );
    let feature = page
        .commits
        .iter()
        .find(|commit| commit.subject == "feature")
        .unwrap();
    assert!(
        feature.graph_edges.iter().any(|edge| {
            edge.parent_index == Some(0)
                && edge.from_lane > edge.to_lane
                && edge.color_index as usize == feature.lane
        }),
        "feature branch should converge into its start commit on the lower main lane: {feature:?}"
    );
    assert!(page.lane_count >= 2);
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn literal_pathspec_prevents_bracket_expansion_when_staging() {
    let repo = init_repo("literal_pathspec");
    fs::write(repo.join("file[0].txt"), "literal\n").unwrap();
    fs::write(repo.join("file0.txt"), "wildcard target\n").unwrap();

    let output = GitExec::default()
        .run(GitRunOptions {
            repo: Some(repo.clone()),
            args: safe_mutation_args(vec![
                "add".to_string(),
                "--".to_string(),
                "file[0].txt".to_string(),
            ]),
            limits: CommandLimits::standard(),
        })
        .unwrap();
    assert!(
        output.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let staged = git_stdout(&repo, &["diff", "--cached", "--name-only"]);
    assert_eq!(staged, "file[0].txt");
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn safe_mutation_commit_profile_disables_prepare_commit_msg_hook() {
    let repo = init_repo("commit_hook");
    fs::create_dir_all(repo.join(".git").join("hooks")).unwrap();
    fs::write(
        repo.join(".git").join("hooks").join("prepare-commit-msg"),
        "#!/bin/sh\necho hook-ran > hook-ran.txt\n",
    )
    .unwrap();
    run_git(&repo, &["config", "core.hooksPath", ".git/hooks"]);
    fs::write(repo.join("next.txt"), "next\n").unwrap();
    run_git(&repo, &["add", "next.txt"]);

    let output = GitExec::default()
        .run(GitRunOptions {
            repo: Some(repo.clone()),
            args: safe_mutation_args(vec![
                "commit".to_string(),
                "--no-verify".to_string(),
                "--no-gpg-sign".to_string(),
                "-m".to_string(),
                "safe commit".to_string(),
            ]),
            limits: CommandLimits::standard(),
        })
        .unwrap();
    assert!(
        output.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!repo.join("hook-ran.txt").exists());
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn app_service_trust_gate_stage_and_commit_flow() {
    let repo = init_repo("service_commit");
    fs::write(repo.join("service.txt"), "service\n").unwrap();
    let service = test_service("service_commit");
    let summary = service.add_repo(&repo).unwrap();

    let blocked = service
        .stage_paths(summary.id.clone(), vec!["service.txt".to_string()])
        .unwrap_err();
    assert_eq!(blocked.code, "repo_untrusted");

    service.set_repo_trusted(&summary.id, true).unwrap();
    let staged = service
        .stage_paths(summary.id.clone(), vec!["service.txt".to_string()])
        .unwrap();
    assert_eq!(staged.staged[0].path, "service.txt");

    let clean = service
        .commit(summary.id.clone(), "service commit".to_string())
        .unwrap();
    assert_eq!(clean.raw_entry_count, 0);
    let log = git_stdout(&repo, &["log", "-1", "--format=%s"]);
    assert_eq!(log, "service commit");
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn app_service_host_trust_uses_external_trust_decision_for_mutations() {
    let repo = init_repo("service_host_trust");
    run_git(&repo, &["config", "core.fsmonitor", "marker-helper"]);
    fs::write(repo.join("host-trusted.txt"), "host trusted\n").unwrap();
    let service = test_service("service_host_trust");
    let summary = service.add_repo(&repo).unwrap();

    let standalone_trust = service.set_repo_trusted(&summary.id, true).unwrap_err();
    assert_eq!(standalone_trust.code, "unsafe_repo_config");

    service.set_repo_trusted_by_host(&summary.id, true).unwrap();
    let staged = service
        .stage_paths(summary.id.clone(), vec!["host-trusted.txt".to_string()])
        .unwrap();
    assert_eq!(staged.staged[0].path, "host-trusted.txt");

    service
        .set_repo_trusted_by_host(&summary.id, false)
        .unwrap();
    let blocked = service
        .unstage_paths(summary.id.clone(), vec!["host-trusted.txt".to_string()])
        .unwrap_err();
    assert_eq!(blocked.code, "repo_untrusted");

    let _ = fs::remove_dir_all(repo);
}

#[test]
fn app_service_blocks_trust_when_repo_local_credential_helper_is_configured() {
    let repo = init_repo("unsafe_local_credential");
    run_git(&repo, &["config", "credential.helper", "!echo helper-ran"]);
    let service = test_service("unsafe_local_credential");
    let summary = service.add_repo(&repo).unwrap();

    let err = service.set_repo_trusted(&summary.id, true).unwrap_err();
    assert_eq!(err.code, "unsafe_repo_config");
    assert!(err.message.contains("credential.helper"));

    let _ = fs::remove_dir_all(repo);
}

#[test]
fn app_service_blocks_trust_when_repo_local_fsmonitor_is_configured() {
    let repo = init_repo("unsafe_local_fsmonitor");
    run_git(&repo, &["config", "core.fsmonitor", "marker-helper"]);
    let service = test_service("unsafe_local_fsmonitor");
    let summary = service.add_repo(&repo).unwrap();

    let err = service.set_repo_trusted(&summary.id, true).unwrap_err();
    assert_eq!(err.code, "unsafe_repo_config");
    assert!(err.message.contains("core.fsmonitor"));

    let _ = fs::remove_dir_all(repo);
}

#[test]
fn app_service_blocks_fetch_when_repo_local_ssh_command_is_configured() {
    let repo = init_repo("unsafe_local_ssh");
    let service = test_service("unsafe_local_ssh");
    let summary = service.add_repo(&repo).unwrap();
    service.set_repo_trusted(&summary.id, true).unwrap();
    run_git(&repo, &["config", "core.sshCommand", "marker-helper"]);

    let err = service.fetch_repo(summary.id).unwrap_err();
    assert_eq!(err.code, "unsafe_repo_config");
    assert!(err.message.contains("core.sshCommand"));

    let _ = fs::remove_dir_all(repo);
}

#[test]
fn app_service_blocks_stage_when_repo_local_clean_filter_is_configured() {
    let repo = init_repo("unsafe_local_filter");
    let service = test_service("unsafe_local_filter");
    let summary = service.add_repo(&repo).unwrap();
    service.set_repo_trusted(&summary.id, true).unwrap();
    run_git(&repo, &["config", "filter.proof.clean", "marker-helper"]);
    fs::write(repo.join("filter.txt"), "filter\n").unwrap();

    let err = service
        .stage_paths(summary.id.clone(), vec!["filter.txt".to_string()])
        .unwrap_err();
    assert_eq!(err.code, "unsafe_repo_config");
    assert!(err.message.contains("filter.*.clean"));
    let status = service.refresh_repo(summary.id).unwrap();
    assert!(
        status
            .untracked
            .iter()
            .any(|entry| entry.path == "filter.txt")
    );

    let _ = fs::remove_dir_all(repo);
}

#[test]
fn app_service_blocks_commit_when_repo_local_gpg_program_is_configured() {
    let repo = init_repo("unsafe_local_gpg");
    let service = test_service("unsafe_local_gpg");
    let summary = service.add_repo(&repo).unwrap();
    service.set_repo_trusted(&summary.id, true).unwrap();
    fs::write(repo.join("signed.txt"), "signed\n").unwrap();
    service
        .stage_paths(summary.id.clone(), vec!["signed.txt".to_string()])
        .unwrap();
    run_git(&repo, &["config", "gpg.program", "marker-helper"]);

    let err = service
        .commit(summary.id.clone(), "blocked commit".to_string())
        .unwrap_err();
    assert_eq!(err.code, "unsafe_repo_config");
    assert!(err.message.contains("gpg.program"));

    let _ = fs::remove_dir_all(repo);
}

#[test]
fn app_service_blocks_fetch_when_repo_local_git_proxy_is_configured() {
    let repo = init_repo("unsafe_local_git_proxy");
    let service = test_service("unsafe_local_git_proxy");
    let summary = service.add_repo(&repo).unwrap();
    service.set_repo_trusted(&summary.id, true).unwrap();
    run_git(&repo, &["config", "core.gitProxy", "marker-helper"]);

    let err = service.fetch_repo(summary.id).unwrap_err();
    assert_eq!(err.code, "unsafe_repo_config");
    assert!(err.message.contains("core.gitProxy"));

    let _ = fs::remove_dir_all(repo);
}

#[test]
fn app_service_read_views_share_core_limits_and_parsers() {
    let repo = init_repo("service_views");
    fs::write(repo.join("tracked.txt"), "two\n").unwrap();
    let service = test_service("service_views");
    let summary = service.add_repo(&repo).unwrap();

    let status = service.refresh_repo(summary.id.clone()).unwrap();
    assert_eq!(status.unstaged[0].path, "tracked.txt");

    let diff = service
        .get_diff(summary.id.clone(), "tracked.txt".to_string(), false)
        .unwrap();
    assert!(!diff.files.is_empty());

    let preview = service
        .read_file_preview(summary.id.clone(), "tracked.txt".to_string())
        .unwrap();
    assert_eq!(preview.text.as_deref(), Some("two\n"));

    let history = service
        .get_history(summary.id.clone(), None, Some(10))
        .unwrap();
    assert_eq!(history.commits[0].subject, "initial");

    let commit_diff = service
        .get_commit_diff(summary.id.clone(), history.commits[0].id.clone(), None)
        .unwrap();
    assert_eq!(
        commit_diff.target.commit.as_deref(),
        Some(history.commits[0].id.as_str())
    );
    assert!(
        commit_diff
            .files
            .iter()
            .any(|file| file.new_path.as_deref() == Some("tracked.txt"))
    );

    let _ = fs::remove_dir_all(repo);
}

#[test]
fn app_service_refresh_non_active_repo_does_not_change_selection() {
    let repo_a = init_repo("service_active_a");
    let repo_b = init_repo("service_active_b");
    fs::write(repo_b.join("tracked.txt"), "changed in b\n").unwrap();
    let service = test_service("service_active");
    let summary_a = service.add_repo(&repo_a).unwrap();
    let summary_b = service.add_repo(&repo_b).unwrap();
    service.set_active_repo(&summary_a.id).unwrap();

    let status_b = service.refresh_repo(summary_b.id.clone()).unwrap();
    assert_eq!(status_b.unstaged[0].path, "tracked.txt");
    let active = service.active_repo_config().unwrap().unwrap();
    assert_eq!(active.id, summary_a.id);
    assert!(service.active_status().unwrap().is_none());

    let _ = fs::remove_dir_all(repo_a);
    let _ = fs::remove_dir_all(repo_b);
}
