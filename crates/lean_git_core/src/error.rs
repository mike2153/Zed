use serde::{Deserialize, Serialize};
use std::fmt;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppError {
    pub code: String,
    pub message: String,
}

impl AppError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn io(context: &str, err: std::io::Error) -> Self {
        Self::new("io", format!("{context}: {err}"))
    }

    pub fn git(context: &str, stderr: &str) -> Self {
        let redacted = redact_sensitive_git_output(stderr);
        let message = if redacted.trim().is_empty() {
            context.to_string()
        } else {
            format!("{context}: {}", redacted.trim())
        };
        Self::new("git", message)
    }
}

pub fn redact_sensitive_git_output(input: &str) -> String {
    redact_sensitive_key_values(&redact_url_userinfo(input))
}

fn redact_url_userinfo(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(offset) = input[cursor..].find("://") {
        let scheme_start = cursor + offset;
        let auth_start = scheme_start + 3;
        output.push_str(&input[cursor..auth_start]);

        let segment_end = input[auth_start..]
            .find(|ch: char| {
                ch == '/' || ch == '?' || ch == '#' || ch == '\'' || ch == '"' || ch.is_whitespace()
            })
            .map_or(input.len(), |end| auth_start + end);
        if let Some(at_offset) = input[auth_start..segment_end].rfind('@') {
            output.push_str("[redacted]@");
            cursor = auth_start + at_offset + 1;
        } else {
            cursor = auth_start;
        }
    }
    output.push_str(&input[cursor..]);
    output
}

fn redact_sensitive_key_values(input: &str) -> String {
    const KEYS: [&str; 5] = ["access_token", "oauth_token", "password", "passwd", "token"];

    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while cursor < input.len() {
        let remaining = &input[cursor..];
        let lower = remaining.to_ascii_lowercase();
        let mut match_at = None;
        let mut match_key = "";
        for key in KEYS {
            if let Some(pos) = lower.find(key) {
                if match match_at {
                    Some(current) => pos < current,
                    None => true,
                } {
                    match_at = Some(pos);
                    match_key = key;
                }
            }
        }

        let Some(pos) = match_at else {
            output.push_str(remaining);
            break;
        };
        let absolute = cursor + pos;
        let key_end = absolute + match_key.len();
        if !is_sensitive_key_boundary(input, absolute, key_end)
            || input[key_end..].chars().next() != Some('=')
        {
            let next = input[absolute..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(1);
            output.push_str(&input[cursor..absolute + next]);
            cursor = absolute + next;
            continue;
        }

        output.push_str(&input[cursor..key_end + 1]);
        output.push_str("[redacted]");
        cursor = key_end + 1;
        while cursor < input.len() {
            let Some(ch) = input[cursor..].chars().next() else {
                break;
            };
            if ch == '&' || ch == '\'' || ch == '"' || ch.is_whitespace() {
                break;
            }
            cursor += ch.len_utf8();
        }
    }
    output
}

fn is_sensitive_key_boundary(input: &str, start: usize, end: usize) -> bool {
    let before = input[..start].chars().next_back();
    let after = input[end..].chars().next();
    let before_ok = match before {
        Some(ch) => !is_key_char(ch),
        None => true,
    };
    let after_ok = match after {
        Some(ch) => ch == '=' || !is_key_char(ch),
        None => true,
    };
    before_ok && after_ok
}

fn is_key_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        AppError::io("filesystem operation failed", value)
    }
}

impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        AppError::new("json", value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_error_redacts_url_userinfo() {
        let err = AppError::git(
            "fetch failed",
            "fatal: Authentication failed for 'https://user:secret@example.invalid/repo.git/'",
        );
        assert!(err.message.contains("https://[redacted]@example.invalid"));
        assert!(!err.message.contains("secret"));
    }

    #[test]
    fn git_error_redacts_sensitive_query_values() {
        let err = AppError::git(
            "fetch failed",
            "remote: denied https://example.invalid/repo?token=abc123&password=sekret",
        );
        assert!(err.message.contains("token=[redacted]"));
        assert!(err.message.contains("password=[redacted]"));
        assert!(!err.message.contains("abc123"));
        assert!(!err.message.contains("sekret"));
    }
}
