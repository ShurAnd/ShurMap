//! Настройки программы: последняя открытая карта и добавленные слои.
//! Хранятся в %APPDATA%\ShurMap\settings.json

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::APP_NAME;

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LayerEntry {
    pub path: String,
    #[serde(default = "yes")]
    pub visible: bool,
    #[serde(default)]
    pub color: usize,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub map: Option<String>,
    #[serde(default)]
    pub layers: Vec<LayerEntry>,
}

/// Папка настроек, например C:\Users\you\AppData\Roaming\ShurMap
pub fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join(APP_NAME))
}

pub fn load() -> Settings {
    let Some(dir) = config_dir() else {
        return Settings::default();
    };

    if let Ok(text) = std::fs::read_to_string(dir.join("settings.json")) {
        if let Ok(settings) = serde_json::from_str::<Settings>(&text) {
            return settings;
        }
    }

    // Старые версии хранили только путь к карте в last_file.txt
    if let Ok(old) = std::fs::read_to_string(dir.join("last_file.txt")) {
        let path = old.trim();
        if !path.is_empty() {
            return Settings {
                map: Some(path.to_string()),
                layers: Vec::new(),
            };
        }
    }
    Settings::default()
}

pub fn save(settings: &Settings) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(dir.join("settings.json"), text);
    }
}
