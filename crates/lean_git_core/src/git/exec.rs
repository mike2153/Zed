use crate::error::{AppError, AppResult};
use crate::limits::{CommandLimits, MAX_STDERR_BYTES, MAX_STDOUT_BYTES};
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const NONINTERACTIVE_GIT_ENV: [(&str, &str); 2] =
    [("GIT_TERMINAL_PROMPT", "0"), ("GCM_INTERACTIVE", "Never")];
const REMOVED_PROMPT_ENV: [&str; 2] = ["GIT_ASKPASS", "SSH_ASKPASS"];
const REMOVED_GIT_OVERRIDE_ENV: [&str; 20] = [
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
    "GIT_DIFF_OPTS",
    "GIT_DIR",
    "GIT_EDITOR",
    "GIT_EXEC_PATH",
    "GIT_EXTERNAL_DIFF",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PAGER",
    "GIT_PROXY_COMMAND",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_SSL_NO_VERIFY",
    "GIT_TEMPLATE_DIR",
];
const REMOVED_GIT_OVERRIDE_ENV_PREFIXES: [&str; 2] = ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

#[derive(Debug, Clone)]
pub struct GitExec {
    executable: PathBuf,
}

impl Default for GitExec {
    fn default() -> Self {
        Self {
            executable: resolve_system_git_executable().unwrap_or_else(|| PathBuf::from("git")),
        }
    }
}

impl GitExec {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn run(&self, options: GitRunOptions) -> AppResult<GitOutput> {
        let start = Instant::now();
        let mut command = Command::new(&self.executable);
        if let Some(repo) = &options.repo {
            command.arg("-C").arg(repo);
        }
        command
            .args(&options.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_noninteractive_env(&mut command);
        suppress_console_window(&mut command);

        let mut child = command
            .spawn()
            .map_err(|err| AppError::io("spawn git", err))?;

        let mut stdout = child.stdout.take().expect("stdout pipe exists");
        let mut stderr = child.stderr.take().expect("stderr pipe exists");
        let stdout_limit = options.limits.max_stdout_bytes;
        let stderr_limit = options.limits.max_stderr_bytes;

        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        let stdout_overflowed = Arc::new(AtomicBool::new(false));
        let stdout_overflow_signal = Arc::clone(&stdout_overflowed);

        std::thread::spawn(move || {
            let _ = out_tx.send(read_capped(
                &mut stdout,
                stdout_limit,
                Some(stdout_overflow_signal),
            ));
        });
        std::thread::spawn(move || {
            let _ = err_tx.send(read_capped(&mut stderr, stderr_limit, None));
        });

        let status = loop {
            if stdout_overflowed.load(Ordering::Relaxed) {
                terminate_child_process_tree(&mut child);
                let stdout = out_rx
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap_or_else(|_| CappedBytes::empty());
                let stderr = err_rx
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap_or_else(|_| CappedBytes::empty());
                return Ok(GitOutput {
                    status: GitExitStatus::OutputLimitExceeded,
                    stdout: stdout.bytes,
                    stderr: stderr.bytes,
                    duration: start.elapsed(),
                    truncated_stdout: true,
                    truncated_stderr: stderr.truncated,
                });
            }
            match child
                .try_wait()
                .map_err(|err| AppError::io("wait git", err))?
            {
                Some(status) => break status,
                None if start.elapsed() >= options.limits.timeout => {
                    terminate_child_process_tree(&mut child);
                    let stdout = out_rx
                        .recv_timeout(Duration::from_secs(1))
                        .unwrap_or_else(|_| CappedBytes::empty());
                    let stderr = err_rx
                        .recv_timeout(Duration::from_secs(1))
                        .unwrap_or_else(|_| CappedBytes::empty());
                    return Ok(GitOutput {
                        status: GitExitStatus::TimedOut,
                        stdout: stdout.bytes,
                        stderr: stderr.bytes,
                        duration: start.elapsed(),
                        truncated_stdout: stdout.truncated,
                        truncated_stderr: stderr.truncated,
                    });
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        };

        let stdout = out_rx.recv().unwrap_or_else(|_| CappedBytes::empty());
        let stderr = err_rx.recv().unwrap_or_else(|_| CappedBytes::empty());
        let exit_status = if stdout.truncated {
            GitExitStatus::OutputLimitExceeded
        } else {
            GitExitStatus::from_status(status)
        };

        Ok(GitOutput {
            status: exit_status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            duration: start.elapsed(),
            truncated_stdout: stdout.truncated,
            truncated_stderr: stderr.truncated,
        })
    }

    pub fn git_version(&self) -> AppResult<String> {
        let output = self.run(GitRunOptions {
            repo: None,
            args: vec!["--version".to_string()],
            limits: CommandLimits {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 16 * 1024,
                max_stderr_bytes: 16 * 1024,
            },
        })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(AppError::git(
                "git --version failed",
                &String::from_utf8_lossy(&output.stderr),
            ))
        }
    }

    pub fn status_porcelain_v2(&self, repo: &Path) -> AppResult<GitOutput> {
        self.run(GitRunOptions {
            repo: Some(repo.to_path_buf()),
            args: safe_read_only_args(vec![
                "status".to_string(),
                "--porcelain=v2".to_string(),
                "--branch".to_string(),
                "-z".to_string(),
            ]),
            limits: CommandLimits::status(),
        })
    }
}

fn apply_noninteractive_env(command: &mut Command) {
    remove_git_override_env(command);
    for (key, value) in NONINTERACTIVE_GIT_ENV {
        command.env(key, value);
    }
}

fn remove_git_override_env(command: &mut Command) {
    let mut keys = Vec::new();
    keys.extend(env::vars_os().map(|(key, _)| key));
    keys.extend(command.get_envs().map(|(key, _)| key.to_os_string()));

    for key in keys {
        if should_remove_git_env_key(&key) {
            command.env_remove(key);
        }
    }

    for key in REMOVED_PROMPT_ENV {
        command.env_remove(key);
    }
    for key in REMOVED_GIT_OVERRIDE_ENV {
        command.env_remove(key);
    }
}

fn should_remove_git_env_key(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    REMOVED_PROMPT_ENV
        .iter()
        .chain(REMOVED_GIT_OVERRIDE_ENV.iter())
        .any(|candidate| key.eq_ignore_ascii_case(candidate))
        || REMOVED_GIT_OVERRIDE_ENV_PREFIXES
            .iter()
            .any(|prefix| starts_with_ignore_ascii_case(&key, prefix))
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

fn terminate_child_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        let mut kill = Command::new("taskkill.exe");
        kill.args(["/PID", pid.as_str(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        suppress_console_window(&mut kill);
        let _ = kill.status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// On Windows, prevents a console window from briefly flashing each time a
/// child process is spawned from the (console-less) GUI. No-op elsewhere.
#[cfg(windows)]
fn suppress_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: run the child without allocating a console window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn suppress_console_window(_command: &mut Command) {}

pub fn safe_read_only_args(args: Vec<String>) -> Vec<String> {
    let mut safe_args = vec![
        "--literal-pathspecs".to_string(),
        "--no-optional-locks".to_string(),
        "-c".to_string(),
        "core.fsmonitor=false".to_string(),
        "-c".to_string(),
        "core.hooksPath=".to_string(),
        "-c".to_string(),
        "protocol.ext.allow=never".to_string(),
        "-c".to_string(),
        "diff.external=".to_string(),
    ];
    safe_args.extend(args);
    safe_args
}

pub fn safe_mutation_args(args: Vec<String>) -> Vec<String> {
    let mut safe_args = vec![
        "--literal-pathspecs".to_string(),
        "-c".to_string(),
        "core.fsmonitor=false".to_string(),
        "-c".to_string(),
        "core.hooksPath=".to_string(),
        "-c".to_string(),
        "protocol.ext.allow=never".to_string(),
        "-c".to_string(),
        "diff.external=".to_string(),
    ];
    safe_args.extend(args);
    safe_args
}

#[derive(Debug, Clone)]
pub struct GitRunOptions {
    pub repo: Option<PathBuf>,
    pub args: Vec<String>,
    pub limits: CommandLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitOutput {
    pub status: GitExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    #[serde(skip)]
    pub duration: Duration,
    pub truncated_stdout: bool,
    pub truncated_stderr: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitExitStatus {
    Success,
    Failed(Option<i32>),
    TimedOut,
    OutputLimitExceeded,
}

impl GitExitStatus {
    pub fn success(&self) -> bool {
        matches!(self, Self::Success)
    }

    fn from_status(status: ExitStatus) -> Self {
        if status.success() {
            Self::Success
        } else {
            Self::Failed(status.code())
        }
    }
}

struct CappedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CappedBytes {
    fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }
}

fn read_capped(
    reader: &mut impl Read,
    limit: usize,
    overflow_signal: Option<Arc<AtomicBool>>,
) -> CappedBytes {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                let remaining = limit.saturating_sub(bytes.len());
                if remaining == 0 {
                    truncated = true;
                    if let Some(signal) = &overflow_signal {
                        signal.store(true, Ordering::Relaxed);
                    }
                    continue;
                }
                let copy = read.min(remaining);
                bytes.extend_from_slice(&chunk[..copy]);
                if copy < read {
                    truncated = true;
                    if let Some(signal) = &overflow_signal {
                        signal.store(true, Ordering::Relaxed);
                    }
                }
            }
            Err(_) => break,
        }
    }

    CappedBytes { bytes, truncated }
}

pub fn default_stdout_limit() -> usize {
    MAX_STDOUT_BYTES
}

pub fn default_stderr_limit() -> usize {
    MAX_STDERR_BYTES
}

fn resolve_system_git_executable() -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("LEAN_GIT_EXPLORER_GIT") {
        let candidate = PathBuf::from(explicit);
        if candidate.is_file() {
            return Some(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    for candidate in known_git_locations() {
        if candidate.is_file() {
            return Some(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    let path = env::var_os("PATH")?;
    let names = git_candidate_names();
    for dir in env::split_paths(&path) {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate.canonicalize().unwrap_or(candidate));
            }
        }
    }
    None
}

#[cfg(windows)]
fn known_git_locations() -> Vec<PathBuf> {
    let mut locations = Vec::new();
    for root in [
        env::var_os("ProgramFiles"),
        env::var_os("ProgramFiles(x86)"),
    ]
    .into_iter()
    .flatten()
    {
        let root = PathBuf::from(root);
        locations.push(root.join("Git").join("cmd").join("git.exe"));
        locations.push(root.join("Git").join("bin").join("git.exe"));
    }
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        locations.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Git")
                .join("cmd")
                .join("git.exe"),
        );
    }
    locations
}

#[cfg(not(windows))]
fn known_git_locations() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/bin/git"),
        PathBuf::from("/usr/local/bin/git"),
    ]
}

#[cfg(windows)]
fn git_candidate_names() -> [&'static str; 2] {
    ["git.exe", "git"]
}

#[cfg(not(windows))]
fn git_candidate_names() -> [&'static str; 1] {
    ["git"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_version_runs() {
        let version = GitExec::default().git_version().unwrap();
        assert!(version.starts_with("git version"));
    }

    #[test]
    fn missing_git_is_reported() {
        let git = GitExec::new("definitely-not-a-git-binary-for-lean-git-explorer");
        let err = git.git_version().unwrap_err();
        assert_eq!(err.code, "io");
    }

    #[test]
    fn safe_read_only_args_disable_external_git_hooks_and_protocols() {
        let args = safe_read_only_args(vec!["status".to_string()]);
        let has_pair = |flag: &str, value: &str| {
            args.windows(2)
                .any(|pair| pair[0] == flag && pair[1] == value)
        };
        assert!(has_pair("-c", "core.fsmonitor=false"));
        assert!(has_pair("-c", "core.hooksPath="));
        assert!(has_pair("-c", "protocol.ext.allow=never"));
        assert!(has_pair("-c", "diff.external="));
        assert!(args.contains(&"--no-optional-locks".to_string()));
        assert!(args.contains(&"--literal-pathspecs".to_string()));
        assert_eq!(args.last(), Some(&"status".to_string()));
    }

    #[test]
    fn safe_mutation_args_disable_hooks_and_use_literal_pathspecs() {
        let args = safe_mutation_args(vec![
            "add".to_string(),
            "--".to_string(),
            "file[0].txt".to_string(),
        ]);
        let has_pair = |flag: &str, value: &str| {
            args.windows(2)
                .any(|pair| pair[0] == flag && pair[1] == value)
        };
        assert!(args.contains(&"--literal-pathspecs".to_string()));
        assert!(has_pair("-c", "core.fsmonitor=false"));
        assert!(has_pair("-c", "core.hooksPath="));
        assert!(has_pair("-c", "protocol.ext.allow=never"));
        assert!(has_pair("-c", "diff.external="));
        assert_eq!(args.last(), Some(&"file[0].txt".to_string()));
    }

    #[test]
    fn git_prompt_environment_is_noninteractive() {
        assert!(NONINTERACTIVE_GIT_ENV.contains(&("GIT_TERMINAL_PROMPT", "0")));
        assert!(NONINTERACTIVE_GIT_ENV.contains(&("GCM_INTERACTIVE", "Never")));
        assert!(REMOVED_PROMPT_ENV.contains(&"GIT_ASKPASS"));
        assert!(REMOVED_PROMPT_ENV.contains(&"SSH_ASKPASS"));
    }

    #[test]
    fn git_override_environment_is_removed_from_command() {
        let mut command = Command::new("git");
        for key in REMOVED_PROMPT_ENV
            .iter()
            .chain(REMOVED_GIT_OVERRIDE_ENV.iter())
        {
            command.env(key, "injected");
        }
        command.env("GIT_CONFIG_KEY_0", "core.sshCommand");
        command.env("GIT_CONFIG_VALUE_0", "marker-helper");
        command.env("git_config_key_1", "credential.helper");
        command.env("git_config_value_1", "marker-helper");

        apply_noninteractive_env(&mut command);

        for key in REMOVED_PROMPT_ENV
            .iter()
            .chain(REMOVED_GIT_OVERRIDE_ENV.iter())
        {
            assert_env_removed(&command, key);
        }
        assert_env_removed(&command, "GIT_CONFIG_KEY_0");
        assert_env_removed(&command, "GIT_CONFIG_VALUE_0");
        assert_env_removed(&command, "git_config_key_1");
        assert_env_removed(&command, "git_config_value_1");
        assert_eq!(
            explicit_env_value(&command, "GIT_TERMINAL_PROMPT"),
            Some(Some("0".to_string()))
        );
        assert_eq!(
            explicit_env_value(&command, "GCM_INTERACTIVE"),
            Some(Some("Never".to_string()))
        );
    }

    #[test]
    fn git_override_environment_matching_is_case_insensitive() {
        assert!(should_remove_git_env_key(OsStr::new("git_ssh_command")));
        assert!(should_remove_git_env_key(OsStr::new("GIT_CONFIG_KEY_0")));
        assert!(should_remove_git_env_key(OsStr::new("git_config_value_7")));
        assert!(!should_remove_git_env_key(OsStr::new("GIT_TRACE")));
        assert!(!should_remove_git_env_key(OsStr::new("SSH_AUTH_SOCK")));
    }

    #[test]
    fn read_capped_sets_overflow_signal() {
        let signal = Arc::new(AtomicBool::new(false));
        let mut input = std::io::Cursor::new(vec![1_u8; 10]);
        let output = read_capped(&mut input, 4, Some(Arc::clone(&signal)));
        assert_eq!(output.bytes.len(), 4);
        assert!(output.truncated);
        assert!(signal.load(Ordering::Relaxed));
    }

    #[test]
    fn stdout_overflow_is_non_success_even_if_git_exits_quickly() {
        let output = GitExec::default()
            .run(GitRunOptions {
                repo: None,
                args: vec!["--version".to_string()],
                limits: CommandLimits {
                    timeout: Duration::from_secs(10),
                    max_stdout_bytes: 4,
                    max_stderr_bytes: 16 * 1024,
                },
            })
            .unwrap();
        assert_eq!(output.status, GitExitStatus::OutputLimitExceeded);
        assert!(output.truncated_stdout);
        assert!(!output.status.success());
    }

    fn assert_env_removed(command: &Command, key: &str) {
        assert_eq!(explicit_env_value(command, key), Some(None), "{key}");
    }

    fn explicit_env_value(command: &Command, key: &str) -> Option<Option<String>> {
        command
            .get_envs()
            .find(|(candidate, _)| candidate.to_string_lossy().eq_ignore_ascii_case(key))
            .map(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
    }
}
