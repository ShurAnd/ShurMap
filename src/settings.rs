//! Настройки программы: последняя открытая карта, список слоёв и свёрнутые разделы панели.
//! Слои запоминаются, но при запуске все выключены; фильтр по странам не запоминается.
//! Хранятся в %APPDATA%\ShurMap\settings.json

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::APP_NAME;

/// Слой в списке. Включён ли он, не хранится: после запуска все слои выключены.
#[derive(Serialize, Deserialize, Clone)]
pub struct LayerEntry {
    pub path: String,
    #[serde(default)]
    pub color: usize,
    /// Значок точек (ключ из icons.rs): запоминается, чтобы слой сразу рисовался правильно;
    /// при загрузке слоя подбирается заново по его содержимому
    #[serde(default)]
    pub icon: Option<String>,
    /// Раздел панели слоёв (ключ из categories.rs); если не задан, подбирается по содержимому
    #[serde(default)]
    pub category: Option<String>,
    /// Раздел выбран пользователем (иначе он подбирается по содержимому слоя при загрузке)
    #[serde(default)]
    pub category_manual: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub map: Option<String>,
    #[serde(default)]
    pub layers: Vec<LayerEntry>,
    /// Свёрнутые разделы панели слоёв (ключи из categories.rs)
    #[serde(default)]
    pub closed_categories: Vec<String>,
}

/// Папка настроек, например C:\Users\you\AppData\Roaming\ShurMap
pub fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join(APP_NAME))
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            map: None,
            layers: Vec::new(),
            closed_categories: Vec::new(),
        }
    }
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
                closed_categories: Vec::new(),
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
