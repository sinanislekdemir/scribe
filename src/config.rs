use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const APP_NAME: &str = "scribe";
const LEGACY_APP_NAME: &str = "mydocs-tui";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub server: String,
    pub login_name: String,
    pub app_password: String,
    #[serde(default = "default_root")]
    pub remote_root: String,
    #[serde(default = "default_root")]
    pub last_folder: String,
    #[serde(default)]
    pub last_file: Option<String>,
}

fn default_root() -> String {
    "/".to_string()
}

impl Config {
    pub fn new(server: String, login_name: String, app_password: String) -> Self {
        Self {
            server: normalize_server(&server),
            login_name,
            app_password,
            remote_root: "/".to_string(),
            last_folder: "/".to_string(),
            last_file: None,
        }
    }
}

pub fn normalize_server(server: &str) -> String {
    let mut s = server.trim().to_string();
    if s.is_empty() {
        return s;
    }
    if !s.starts_with("http://") && !s.starts_with("https://") {
        s = format!("https://{s}");
    }
    while s.ends_with('/') {
        s.pop();
    }
    s
}

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("no config directory")?;
    Ok(base.join(APP_NAME))
}

pub fn cache_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir().context("no cache directory")?;
    Ok(base.join(APP_NAME))
}

fn legacy_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|base| base.join(LEGACY_APP_NAME))
}

fn legacy_cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|base| base.join(LEGACY_APP_NAME))
}

pub fn credentials_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("credentials.toml"))
}

pub fn load() -> Result<Option<Config>> {
    let path = credentials_path()?;
    if !path.exists() {
        migrate_legacy_credentials(&path);
    }
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).context("parse credentials.toml")?;
    Ok(Some(cfg))
}

/// Copy credentials from the old `mydocs-tui` config directory on first run.
fn migrate_legacy_credentials(new_path: &Path) {
    let Some(old_dir) = legacy_config_dir() else {
        return;
    };
    let old_path = old_dir.join("credentials.toml");
    if !old_path.exists() || new_path.exists() {
        return;
    }
    if let Some(parent) = new_path.parent() {
        let _ = fs::create_dir_all(parent);
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    if fs::copy(&old_path, new_path).is_ok() {
        let _ = fs::set_permissions(new_path, fs::Permissions::from_mode(0o600));
    }
}

/// Move the old cache (including backups) to the new app name if possible.
fn migrate_legacy_cache() {
    let (Ok(new_cache), Some(old_cache)) = (cache_dir(), legacy_cache_dir()) else {
        return;
    };
    if new_cache.exists() || !old_cache.exists() {
        return;
    }
    let _ = fs::rename(old_cache, new_cache);
}

pub fn save(cfg: &Config) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir)?;
    let path = credentials_path()?;
    let text = toml::to_string_pretty(cfg).context("serialize credentials")?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {} for writing", path.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn remove() -> Result<()> {
    let path = credentials_path()?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn ensure_dirs() -> Result<()> {
    migrate_legacy_cache();
    let cfg = config_dir()?;
    let cache = cache_dir()?;
    fs::create_dir_all(&cfg)?;
    fs::create_dir_all(&cache)?;
    let _ = fs::set_permissions(&cfg, fs::Permissions::from_mode(0o700));
    let _ = fs::set_permissions(&cache, fs::Permissions::from_mode(0o700));
    Ok(())
}
