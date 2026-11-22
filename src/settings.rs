use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::theme::ThemeName;

pub const DEFAULT_MAX_WIDTH: usize = 120;
pub const MIN_MAX_WIDTH: usize = 40;
pub const MAX_MAX_WIDTH: usize = 240;
pub const DEFAULT_LANGUAGE: &str = "tr_TR";

fn default_max_width() -> usize {
    DEFAULT_MAX_WIDTH
}

fn default_language() -> String {
    DEFAULT_LANGUAGE.to_string()
}

/// UI preferences that are not secrets. Stored in `~/.config/scribe/settings.toml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub theme: ThemeName,
    /// Maximum text column count. The terminal width can still make it narrower.
    #[serde(default = "default_max_width")]
    pub max_width: usize,
    /// Hunspell dictionary stem for `F5` (`tr_TR`, `en_US`, ...), or a path.
    #[serde(default = "default_language")]
    pub language: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeName::default(),
            max_width: DEFAULT_MAX_WIDTH,
            language: default_language(),
        }
    }
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
    let mut settings: Settings = toml::from_str(&text).context("parse settings.toml")?;
    settings.max_width = settings.max_width.clamp(MIN_MAX_WIDTH, MAX_MAX_WIDTH);
    settings.language = settings.language.trim().to_string();
    if settings.language.is_empty() {
        settings.language = default_language();
    }
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
    fn settings_round_trip_keeps_theme_width_and_language() {
        let settings = Settings {
            theme: ThemeName::Paper,
            max_width: 88,
            language: "en_US".to_string(),
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert!(text.contains("theme = \"paper\""));
        assert!(text.contains("max_width = 88"));
        assert!(text.contains("language = \"en_US\""));
        let parsed: Settings = toml::from_str(&text).unwrap();
        assert_eq!(parsed.theme, ThemeName::Paper);
        assert_eq!(parsed.max_width, 88);
        assert_eq!(parsed.language, "en_US");
    }

    #[test]
    fn missing_settings_use_defaults() {
        let parsed: Settings = toml::from_str("").unwrap();
        assert_eq!(parsed.theme, ThemeName::Monokai);
        assert_eq!(parsed.max_width, DEFAULT_MAX_WIDTH);
        assert_eq!(parsed.language, DEFAULT_LANGUAGE);
    }

    #[test]
    fn default_width_matches_the_editor_default() {
        assert_eq!(DEFAULT_MAX_WIDTH, crate::editor::MAX_TEXT_WIDTH);
    }
}
