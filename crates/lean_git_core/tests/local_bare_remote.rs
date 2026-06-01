use lean_git_core::git::exec::{GitExec, GitExitStatus, GitRunOptions};
use lean_git_core::limits::CommandLimits;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("lean_git_remote_{name}_{stamp}"));
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

fn spawn_unauthorized_http_remote(listener: TcpListener) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let start = Instant::now();
        let mut first_request_tx = Some(tx);
        while start.elapsed() < Duration::from_secs(10) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0_u8; 2048];
                    let bytes = stream.read(&mut request).unwrap_or(0);
                    if let Some(tx) = first_request_tx.take() {
                        let text = String::from_utf8_lossy(&request[..bytes]).to_string();
                        let _ = tx.send(text);
                    }
                    let response = concat!(
                        "HTTP/1.1 401 Unauthorized\r\n",
                        "WWW-Authenticate: Basic realm=\"LeanGit\"\r\n",
                        "Content-Length: 0\r\n",
                        "Connection: close\r\n",
                        "\r\n"
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return,
            }
        }
    });
    rx
}

#[test]
fn fetch_pull_ff_only_and_push_against_local_bare_remote() {
    let root = temp_dir("roundtrip");
    let remote = root.join("remote.git");
    let work = root.join("work");
    let other = root.join("other");

    run_git_no_repo(&["init", "--bare", remote.to_str().unwrap()]);
    run_git_no_repo(&["clone", remote.to_str().unwrap(), work.to_str().unwrap()]);
    run_git(&work, &["config", "user.email", "test@example.invalid"]);
    run_git(&work, &["config", "user.name", "Lean Git Test"]);
    fs::write(work.join("file.txt"), "one\n").unwrap();
    run_git(&work, &["add", "file.txt"]);
    run_git(&work, &["commit", "-m", "initial"]);

    let git = GitExec::default();
    let push = git
        .run(GitRunOptions {
            repo: Some(work.clone()),
            args: vec![
                "push".to_string(),
                "-u".to_string(),
                "origin".to_string(),
                "HEAD".to_string(),
            ],
            limits: CommandLimits::network(),
        })
        .unwrap();
    assert!(
        push.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    run_git_no_repo(&["clone", remote.to_str().unwrap(), other.to_str().unwrap()]);
    run_git(&other, &["config", "user.email", "test@example.invalid"]);
    run_git(&other, &["config", "user.name", "Lean Git Test"]);
    fs::write(other.join("file.txt"), "two\n").unwrap();
    run_git(&other, &["commit", "-am", "remote change"]);
    run_git(&other, &["push"]);

    let fetch = git
        .run(GitRunOptions {
            repo: Some(work.clone()),
            args: vec!["fetch".to_string(), "--prune".to_string()],
            limits: CommandLimits::network(),
        })
        .unwrap();
    assert!(
        fetch.status.success(),
        "fetch failed: {}",
        String::from_utf8_lossy(&fetch.stderr)
    );

    let pull = git
        .run(GitRunOptions {
            repo: Some(work.clone()),
            args: vec!["pull".to_string(), "--ff-only".to_string()],
            limits: CommandLimits::network(),
        })
        .unwrap();
    assert!(
        pull.status.success(),
        "pull failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );
    assert_eq!(
        fs::read_to_string(work.join("file.txt"))
            .unwrap()
            .replace("\r\n", "\n"),
        "two\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn http_auth_failure_returns_without_prompting() {
    let root = temp_dir("http_auth");
    run_git_no_repo(&["init", root.to_str().unwrap()]);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/repo.git",
        listener.local_addr().unwrap().port()
    );
    let request_rx = spawn_unauthorized_http_remote(listener);
    run_git(&root, &["remote", "add", "origin", &url]);

    let output = GitExec::default()
        .run(GitRunOptions {
            repo: Some(root.clone()),
            args: vec!["fetch".to_string(), "origin".to_string()],
            limits: CommandLimits {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 16 * 1024,
                max_stderr_bytes: 16 * 1024,
            },
        })
        .unwrap();

    assert!(!output.status.success());
    assert_ne!(output.status, GitExitStatus::TimedOut);
    let request = request_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("git did not contact the local HTTP remote");
    assert!(request.contains("GET /repo.git/info/refs?service=git-upload-pack"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not read Username")
            || stderr.contains("Authentication failed")
            || stderr.contains("terminal prompts disabled")
            || stderr.contains("Authentication is required")
            || stderr.contains("unable to get password from user"),
        "unexpected auth stderr: {stderr}"
    );

    let _ = fs::remove_dir_all(root);
}
