use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::theme::ThemeName;

/// UI preferences that are not secrets. Stored in `~/.config/scribe/settings.toml`.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub theme: ThemeName,
}

pub fn path() -> Result<PathBuf> {
    Ok(config::config_dir()?.join("settings.toml"))
}

pub fn load() -> Result<Settings> {
    let path = path()?;
    if !path.exists() {
        return Ok(Settings::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let settings: Settings = toml::from_str(&text).context("parse settings.toml")?;
    Ok(settings)
}

pub fn save(settings: &Settings) -> Result<()> {
    let dir = config::config_dir()?;
    fs::create_dir_all(&dir)?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    let path = path()?;
    let text = toml::to_string_pretty(settings).context("serialize settings")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_keeps_theme() {
        let settings = Settings {
            theme: ThemeName::Paper,
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert!(text.contains("theme = \"paper\""));
        let parsed: Settings = toml::from_str(&text).unwrap();
        assert_eq!(parsed.theme, ThemeName::Paper);
    }

    #[test]
    fn missing_theme_defaults_to_monokai() {
        let parsed: Settings = toml::from_str("").unwrap();
        assert_eq!(parsed.theme, ThemeName::Monokai);
    }
}
