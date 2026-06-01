//! Runnable end-to-end demonstration of the GitView engine.
//!
//! Spins up a real bare remote + working clone in a temp directory, then drives
//! the *actual* `AppService` API that the GitView UI calls — add repo, trust,
//! stage, commit, push, then fetch + fast-forward pull — printing what happens
//! at each step. The only raw `git` calls are for the throwaway remote/clone
//! infrastructure; every stage/commit/push/pull goes through the engine.
//!
//! Run with:
//!     cargo run -p lean_git_core --example gitview_demo

use lean_git_core::git::exec::GitExec;
use lean_git_core::models::{AppConfig, RepoStatus};
use lean_git_core::service::AppService;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

fn main() -> io::Result<()> {
    let root = scratch_dir();
    println!("workspace: {}", root.display());

    let remote = root.join("remote.git");
    let work = root.join("work");
    let verify = root.join("verify");

    // --- Throwaway remote + working clone (infrastructure, raw git) ----------
    git_bare_init(&remote);
    git_clone(&remote, &work);
    git(&work, &["config", "user.email", "demo@example.invalid"]);
    git(&work, &["config", "user.name", "GitView Demo"]);

    // Seed an initial doc commit so the branch (and its upstream) exists.
    fs::write(work.join("README.md"), "# Demo\n\nLine one.\n")?;
    git(&work, &["add", "README.md"]);
    git(&work, &["commit", "-m", "docs: initial readme"]);

    // --- Everything below is the engine the GitView UI uses ------------------
    let service = AppService::new(
        root.join("engine-config"),
        GitExec::default(),
        AppConfig::default(),
        None,
    );

    let summary = unwrap("add_repo", service.add_repo(&work));
    let id = summary.id.clone();
    println!("\n[1] registered repo  id={}  label={}", id, summary.label);

    service.set_active_repo(&id).expect("set active");
    let trusted = unwrap("set_repo_trusted", service.set_repo_trusted(&id, true));
    println!("[2] trusted repo     trusted={}", trusted.trusted);

    // Publish the initial commit to the remote and remember the upstream.
    let pushed = unwrap(
        "push (initial)",
        service.push_repo(id.clone(), Some(remote.to_string_lossy().to_string())),
    );
    println!("[3] pushed initial   {}", branch_line(&pushed));

    // --- Make a change to the doc, then stage + commit + push via engine -----
    fs::write(work.join("README.md"), "# Demo\n\nLine one.\nLine two.\n")?;
    println!("\n[4] edited README.md (added 'Line two.')");

    let staged = unwrap(
        "stage_paths",
        service.stage_paths(id.clone(), vec!["README.md".to_string()]),
    );
    println!(
        "[5] staged           staged={:?}",
        staged.staged.iter().map(|f| &f.path).collect::<Vec<_>>()
    );

    let committed = unwrap(
        "commit",
        service.commit(id.clone(), "docs: add line two".to_string()),
    );
    println!(
        "[6] committed        clean_tree={}  {}",
        committed.raw_entry_count == 0,
        branch_line(&committed)
    );

    let after_push = unwrap("push", service.push_repo(id.clone(), None));
    println!("[7] pushed change    {}", branch_line(&after_push));

    // --- Prove the remote actually received it ------------------------------
    git_clone(&remote, &verify);
    let remote_doc = fs::read_to_string(verify.join("README.md"))?.replace("\r\n", "\n");
    let remote_subject = git_stdout(&verify, &["log", "-1", "--format=%s"]);
    println!(
        "\n[8] verified on remote subject={:?}  doc_has_line_two={}",
        remote_subject,
        remote_doc.contains("Line two.")
    );

    // --- Fetch + fast-forward pull a change made elsewhere ------------------
    git(&verify, &["config", "user.email", "demo@example.invalid"]);
    git(&verify, &["config", "user.name", "GitView Demo"]);
    fs::write(verify.join("README.md"), format!("{remote_doc}Line three.\n"))?;
    git(&verify, &["commit", "-am", "docs: add line three"]);
    git(&verify, &["push"]);

    unwrap("fetch_repo", service.fetch_repo(id.clone()));
    let pulled = unwrap("pull_ff_only", service.pull_ff_only(id.clone()));
    let local_doc = fs::read_to_string(work.join("README.md"))?.replace("\r\n", "\n");
    println!(
        "[9] fetched + pulled  behind={}  local_has_line_three={}",
        pulled.behind,
        local_doc.contains("Line three.")
    );

    // Operation log recorded by the engine.
    println!("\nengine operation log:");
    for entry in unwrap("operation_log", service.operation_log()).iter().take(12) {
        println!(
            "  {:<6} ok={:<5} {}ms  {}",
            entry.action,
            entry.ok,
            entry.duration_ms,
            entry.message.lines().next().unwrap_or("")
        );
    }

    let ok = committed.raw_entry_count == 0
        && remote_doc.contains("Line two.")
        && remote_subject == "docs: add line two"
        && local_doc.contains("Line three.")
        && pulled.behind == 0;
    println!("\nRESULT: {}", if ok { "PASS" } else { "FAIL" });

    let _ = fs::remove_dir_all(&root);
    if ok {
        Ok(())
    } else {
        Err(io::Error::other("demo assertions failed"))
    }
}

fn branch_line(status: &RepoStatus) -> String {
    format!(
        "branch={} ahead={} behind={}",
        status.branch.as_deref().unwrap_or("(detached)"),
        status.ahead,
        status.behind
    )
}

fn unwrap<T>(step: &str, result: lean_git_core::AppResult<T>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => panic!("{step} failed: [{}] {}", err.code, err.message),
    }
}

fn scratch_dir() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gitview_demo_{stamp}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn git_bare_init(path: &Path) {
    run(Command::new("git").args(["init", "--bare", "--initial-branch=main", &path.to_string_lossy()]));
}

fn git_clone(remote: &Path, into: &Path) {
    run(Command::new("git").args([
        "clone",
        &remote.to_string_lossy(),
        &into.to_string_lossy(),
    ]));
}

fn git(repo: &Path, args: &[&str]) {
    run(Command::new("git").arg("-C").arg(repo).args(args));
}

fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
