use crate::error::{AppError, AppResult};
use crate::limits::{CONFIG_VERSION, DEFAULT_MAX_GIT_WORKERS, MAX_GIT_WORKERS, MIN_GIT_WORKERS};
use crate::models::AppConfig;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_CONFIG_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn default_config_dir(app_name: &str) -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        PathBuf::from(appdata).join(app_name)
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".config")
            .join(app_name)
    }
}

pub fn config_path(config_dir: &Path) -> PathBuf {
    config_dir.join("config.json")
}

fn backup_config_path(config_dir: &Path) -> PathBuf {
    config_dir.join("config.json.bak")
}

pub fn load_config(config_dir: &Path) -> AppResult<AppConfig> {
    let mut path = config_path(config_dir);
    let backup = backup_config_path(config_dir);
    if !path.exists() {
        if backup.exists() {
            path = backup.clone();
        } else {
            return Ok(AppConfig::default());
        }
    }

    let mut config = match load_config_file(&path) {
        Ok(config) => config,
        Err(primary_err) if path != backup && backup.exists() => {
            load_config_file(&backup).map_err(|_| primary_err)?
        }
        Err(err) => return Err(err),
    };
    migrate_config(&mut config);
    Ok(config)
}

fn load_config_file(path: &Path) -> AppResult<AppConfig> {
    let bytes = fs::read(path).map_err(|err| AppError::io("read config", err))?;
    serde_json::from_slice(&bytes).map_err(Into::into)
}

pub fn save_config_atomic(config_dir: &Path, config: &AppConfig) -> AppResult<()> {
    fs::create_dir_all(config_dir).map_err(|err| AppError::io("create config directory", err))?;
    let target = config_path(config_dir);
    let backup = backup_config_path(config_dir);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let unique = TEMP_CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = config_dir.join(format!(
        "config.{}.{}.{}.tmp",
        std::process::id(),
        stamp,
        unique
    ));
    let data = serde_json::to_vec_pretty(config)?;

    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|err| AppError::io("write temp config", err))?;
        file.write_all(&data)
            .map_err(|err| AppError::io("write temp config", err))?;
        file.sync_all()
            .map_err(|err| AppError::io("sync temp config", err))?;
    }
    if target.exists() {
        fs::copy(&target, &backup).map_err(|err| AppError::io("backup old config", err))?;
    }
    if let Err(err) = replace_file(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return Err(AppError::io("commit config", err));
    }
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    let source = wide(source);
    let target = wide(target);
    let ok = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

pub fn migrate_config(config: &mut AppConfig) {
    if config.version == 0 {
        config.version = CONFIG_VERSION;
    }
    config.settings.max_git_workers = clamp_git_workers(config.settings.max_git_workers);
}

pub fn clamp_git_workers(value: usize) -> usize {
    if value == 0 {
        DEFAULT_MAX_GIT_WORKERS
    } else {
        value.clamp(MIN_GIT_WORKERS, MAX_GIT_WORKERS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{RepoConfig, WatchMode};

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!("lean_git_core_{name}_{stamp}"));
        dir
    }

    #[test]
    fn missing_config_loads_default() {
        let dir = temp_dir("missing");
        let config = load_config(&dir).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert!(config.repos.is_empty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = temp_dir("round_trip");
        let mut config = AppConfig::default();
        config.repos.push(RepoConfig {
            id: "repo".to_string(),
            path: "D:/repo".to_string(),
            label: "repo".to_string(),
            favorite: true,
            trusted: true,
            watch: WatchMode::Manual,
        });

        save_config_atomic(&dir, &config).unwrap();
        let loaded = load_config(&dir).unwrap();
        assert_eq!(loaded, config);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_retains_backup_after_replacing_config() {
        let dir = temp_dir("backup_retained");
        let mut first = AppConfig::default();
        first.repos.push(RepoConfig {
            id: "old".to_string(),
            path: "D:/old".to_string(),
            label: "old".to_string(),
            favorite: false,
            trusted: false,
            watch: WatchMode::Manual,
        });
        let mut second = AppConfig::default();
        second.repos.push(RepoConfig {
            id: "new".to_string(),
            path: "D:/new".to_string(),
            label: "new".to_string(),
            favorite: false,
            trusted: false,
            watch: WatchMode::Manual,
        });

        save_config_atomic(&dir, &first).unwrap();
        save_config_atomic(&dir, &second).unwrap();

        let loaded = load_config(&dir).unwrap();
        let backup = load_config_file(&backup_config_path(&dir)).unwrap();
        assert_eq!(loaded.repos[0].id, "new");
        assert_eq!(backup.repos[0].id, "old");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_config_returns_error() {
        let dir = temp_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(config_path(&dir), b"{not json").unwrap();

        let error = load_config(&dir).unwrap_err();
        assert_eq!(error.code, "json");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_primary_uses_backup_when_available() {
        let dir = temp_dir("corrupt_with_backup");
        fs::create_dir_all(&dir).unwrap();
        fs::write(config_path(&dir), b"{not json").unwrap();
        let mut config = AppConfig::default();
        config.settings.max_git_workers = 1;
        let bytes = serde_json::to_vec_pretty(&config).unwrap();
        fs::write(backup_config_path(&dir), bytes).unwrap();

        let loaded = load_config(&dir).unwrap();
        assert_eq!(loaded.settings.max_git_workers, 1);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn migration_clamps_worker_count() {
        let mut config = AppConfig::default();
        config.settings.max_git_workers = 100;
        migrate_config(&mut config);
        assert_eq!(config.settings.max_git_workers, MAX_GIT_WORKERS);
    }

    #[test]
    fn old_repo_config_defaults_to_untrusted() {
        let json = r#"{
          "version": 1,
          "repos": [{
            "id": "repo",
            "path": "D:/repo",
            "label": "repo",
            "favorite": false,
            "watch": "manual"
          }],
          "settings": {
            "max_git_workers": 2,
            "watch_mode": "manual",
            "low_memory_mode": true
          },
          "last_active_repo": null
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert!(!config.repos[0].trusted);
    }

    #[test]
    fn load_uses_backup_when_primary_is_missing() {
        let dir = temp_dir("backup");
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.settings.max_git_workers = 1;
        let bytes = serde_json::to_vec_pretty(&config).unwrap();
        fs::write(backup_config_path(&dir), bytes).unwrap();

        let loaded = load_config(&dir).unwrap();
        assert_eq!(loaded, config);

        let _ = fs::remove_dir_all(dir);
    }
}
