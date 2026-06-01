use std::time::Duration;

pub const CONFIG_VERSION: u32 = 1;
pub const DEFAULT_MAX_GIT_WORKERS: usize = 2;
pub const MIN_GIT_WORKERS: usize = 1;
pub const MAX_GIT_WORKERS: usize = 8;
pub const DEFAULT_HISTORY_LIMIT: usize = 200;
pub const MAX_HISTORY_LIMIT: usize = 1_000;
pub const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STDERR_BYTES: usize = 512 * 1024;
pub const MAX_DIFF_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_DIFF_LINES: usize = 20_000;
pub const MAX_FILE_PREVIEW_BYTES: usize = 1024 * 1024;
pub const MAX_TREE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_DIRECTORY_ENTRIES: usize = 2_000;
pub const MAX_DIRECTORY_CACHE: usize = 32;
pub const MAX_OPERATION_LOG: usize = 200;
pub const MAX_OPERATION_LOG_MESSAGE_BYTES: usize = 4096;
pub const MAX_IPC_PATHS: usize = 256;
pub const MAX_IPC_PATH_LEN: usize = 4096;
pub const MAX_COMMIT_MESSAGE_LEN: usize = 8192;
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(20);
pub const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(60);
pub const NETWORK_GIT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug)]
pub struct CommandLimits {
    pub timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl CommandLimits {
    pub fn standard() -> Self {
        Self {
            timeout: DEFAULT_GIT_TIMEOUT,
            max_stdout_bytes: MAX_STDOUT_BYTES,
            max_stderr_bytes: MAX_STDERR_BYTES,
        }
    }

    pub fn status() -> Self {
        Self {
            timeout: STATUS_TIMEOUT,
            max_stdout_bytes: MAX_STDOUT_BYTES,
            max_stderr_bytes: MAX_STDERR_BYTES,
        }
    }

    pub fn diff() -> Self {
        Self {
            timeout: DEFAULT_GIT_TIMEOUT,
            max_stdout_bytes: MAX_DIFF_BYTES,
            max_stderr_bytes: MAX_STDERR_BYTES,
        }
    }

    pub fn tree() -> Self {
        Self {
            timeout: DEFAULT_GIT_TIMEOUT,
            max_stdout_bytes: MAX_TREE_BYTES,
            max_stderr_bytes: MAX_STDERR_BYTES,
        }
    }

    pub fn file_preview() -> Self {
        Self {
            timeout: DEFAULT_GIT_TIMEOUT,
            max_stdout_bytes: MAX_FILE_PREVIEW_BYTES + 1,
            max_stderr_bytes: MAX_STDERR_BYTES,
        }
    }

    pub fn network() -> Self {
        Self {
            timeout: NETWORK_GIT_TIMEOUT,
            max_stdout_bytes: MAX_STDOUT_BYTES,
            max_stderr_bytes: MAX_STDERR_BYTES,
        }
    }
}
