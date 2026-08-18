//! Phase 5: конфигурация и хранение состояния.
//!
//! Файл: `%APPDATA%\KeySoundTrigger\config.json` (по спецификации).
//! При первом запуске папка и дефолтный конфиг создаются автоматически.
//! Запись атомарная: tmp-файл + rename.

use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const APP_DIR_NAME: &str = "KeySoundTrigger";
pub const CONFIG_FILE_NAME: &str = "config.json";

// ---------------------------------------------------------------------------
// Модель конфигурации (зеркалит config.json из спецификации)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    /// Общая громкость приложения 0.0..=1.0.
    pub master_volume: f32,
    /// true — звуки накладываются; false — новый звук прерывает текущие.
    pub allow_overlap: bool,
    /// Автозагрузка вместе с Windows.
    pub auto_start: bool,
    /// Имя устройства ВЫВОДА для mic-микшера (Soundpad-режим): обычно
    /// «CABLE Input» виртуального кабеля VB-Audio — сюда рендерится
    /// смешанный поток «микрофон + звуки триггеров». Пользователь выбирает
    /// парное «CABLE Output» как микрофон в Discord/игре.
    /// Настраивается автоматически (см. main.js `ensureMicRoutingReady`) —
    /// в UI нет ручного выбора устройства. `None` — функция выключена.
    /// Микрофон для захвата не хранится: всегда берётся текущий системный
    /// микрофон по умолчанию (см. `audio_engine::open_default_mic_capture`).
    #[serde(default)]
    pub mic_output_device: Option<String>,
    #[serde(default)]
    pub triggers: Vec<TriggerRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TriggerRule {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub words: Vec<String>,
    #[serde(default)]
    pub sounds: Vec<SoundOption>,
    /// Дополнительно микшировать звук в живой mic-поток (слышат люди в звонке).
    #[serde(default)]
    pub play_to_mic: bool,
    /// Проигрывать звук локально (слышит сам пользователь). По умолчанию
    /// включено; выключив — звук уходит только в mic-поток (play_to_mic),
    /// то есть собеседники слышат, а сам пользователь — нет.
    #[serde(default = "default_true")]
    pub play_for_self: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SoundOption {
    pub path: String,
    /// Громкость конкретного звука 0.0..=1.0 (умножается на master_volume).
    pub volume: f32,
    /// Вес для взвешенного случайного выбора (P = weight / Σweights).
    pub weight: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            master_volume: 0.8,
            allow_overlap: true,
            auto_start: false,
            mic_output_device: None,
            triggers: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Ошибки
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("ошибка ввода-вывода: {0}")]
    Io(#[from] std::io::Error),
    #[error("ошибка разбора JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ошибка валидации конфигурации: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Пути
// ---------------------------------------------------------------------------

/// Папка конфигурации: `%APPDATA%\KeySoundTrigger` (fallback — рядом с exe).
pub fn config_dir() -> PathBuf {
    if let Some(appdata) = env::var_os("APPDATA") {
        PathBuf::from(appdata).join(APP_DIR_NAME)
    } else {
        env::current_exe()
            .map(|exe| exe.with_file_name(APP_DIR_NAME))
            .unwrap_or_else(|_| PathBuf::from(APP_DIR_NAME))
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE_NAME)
}

// ---------------------------------------------------------------------------
// Загрузка / сохранение
// ---------------------------------------------------------------------------

/// Загрузить конфиг из конкретного файла (без создания чего-либо).
pub fn load_from(path: &Path) -> Result<AppConfig, ConfigError> {
    let raw = fs::read_to_string(path)?;
    let cfg: AppConfig = serde_json::from_str(&raw)?;
    Ok(cfg)
}

/// Сохранить конфиг в конкретный файл атомарно (tmp + замена).
pub fn save_to(config: &AppConfig, path: &Path) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    // std::fs::rename на Windows не перезаписывает существующий файл.
    let _ = fs::remove_file(path);
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Продакшн-загрузка: читает `%APPDATA%\...\config.json`;
/// при отсутствии — создаёт дефолтный и возвращает его.
/// Возвращает (config, warnings, path).
pub fn load_or_create() -> (AppConfig, Vec<String>, PathBuf) {
    let path = config_path();
    match load_from(&path) {
        Ok(cfg) => {
            let warnings = validate(&cfg).unwrap_or_else(|e| vec![e.to_string()]);
            (cfg, warnings, path)
        }
        Err(ConfigError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = AppConfig::default();
            let mut warnings = Vec::new();
            if let Err(e) = save_to(&cfg, &path) {
                warnings.push(format!("не удалось создать конфиг по умолчанию: {e}"));
            }
            (cfg, warnings, path)
        }
        Err(e) => {
            // Битый конфиг не затираем — работаем на дефолте в памяти.
            (
                AppConfig::default(),
                vec![format!(
                    "конфиг повреждён ({e}); используются настройки по умолчанию"
                )],
                path,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Валидация
// ---------------------------------------------------------------------------

/// Проверяет конфиг. Жёсткие нарушения → `Err(Validation)`,
/// мягкие проблемы (пропавшие файлы, пустые списки) → warnings в `Ok`.
pub fn validate(config: &AppConfig) -> Result<Vec<String>, ConfigError> {
    let mut warnings = Vec::new();

    if !(0.0..=1.0).contains(&config.master_volume) {
        return Err(ConfigError::Validation(format!(
            "master_volume должен быть в диапазоне 0.0..=1.0, получено {}",
            config.master_volume
        )));
    }

    for trg in &config.triggers {
        if trg.id.trim().is_empty() {
            return Err(ConfigError::Validation("у триггера пустой id".to_string()));
        }
        if trg.words.is_empty() {
            warnings.push(format!("триггер '{}' не имеет слов — не сработает", trg.name));
        }
        if trg.sounds.is_empty() {
            warnings.push(format!("триггер '{}' не имеет звуков", trg.name));
        } else if trg.sounds.iter().all(|s| s.weight == 0) {
            warnings.push(format!(
                "у триггера '{}' все веса равны 0 — будет выбираться первый звук",
                trg.name
            ));
        }
        for s in &trg.sounds {
            if !(0.0..=1.0).contains(&s.volume) {
                return Err(ConfigError::Validation(format!(
                    "громкость звука '{}' вне диапазона 0.0..=1.0: {}",
                    s.path, s.volume
                )));
            }
            if s.path.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "пустой путь звука в триггере '{}'",
                    trg.name
                )));
            }
            if !Path::new(&s.path).exists() {
                warnings.push(format!("файл не найден: {}", s.path));
            }
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "renarrator_test_{}_{}",
            std::process::id(),
            tag
        ));
        dir.join("config.json")
    }

    fn sample() -> AppConfig {
        AppConfig {
            master_volume: 0.7,
            allow_overlap: false,
            auto_start: true,
            mic_output_device: None,
            triggers: vec![TriggerRule {
                id: "t1".into(),
                name: "Банан".into(),
                words: vec!["банан".into(), "banan".into()],
                sounds: vec![SoundOption {
                    path: "C:\\sounds\\boom.mp3".into(),
                    volume: 0.9,
                    weight: 60,
                }],
                play_to_mic: false,
                play_for_self: true,
            }],
        }
    }

    #[test]
    fn save_load_roundtrip() {
        let path = temp_path("roundtrip");
        let cfg = sample();
        save_to(&cfg, &path).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(cfg, loaded);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_overwrites_existing() {
        let path = temp_path("overwrite");
        save_to(&sample(), &path).unwrap();
        let mut cfg2 = sample();
        cfg2.master_volume = 0.3;
        save_to(&cfg2, &path).unwrap();
        assert_eq!(load_from(&path).unwrap().master_volume, 0.3);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn validation_rejects_bad_master_volume() {
        let mut cfg = sample();
        cfg.master_volume = 1.5;
        assert!(matches!(
            validate(&cfg),
            Err(ConfigError::Validation(_))
        ));
    }

    #[test]
    fn validation_rejects_bad_sound_volume() {
        let mut cfg = sample();
        cfg.triggers[0].sounds[0].volume = -0.2;
        assert!(matches!(
            validate(&cfg),
            Err(ConfigError::Validation(_))
        ));
    }

    #[test]
    fn validation_warns_about_missing_file() {
        let cfg = sample(); // C:\sounds\boom.mp3 почти наверняка не существует
        let warnings = validate(&cfg).unwrap();
        assert!(warnings.iter().any(|w| w.contains("файл не найден")));
    }

    #[test]
    fn validation_rejects_empty_trigger_id() {
        let mut cfg = sample();
        cfg.triggers[0].id = "  ".into();
        assert!(matches!(
            validate(&cfg),
            Err(ConfigError::Validation(_))
        ));
    }

    #[test]
    fn default_config_is_valid() {
        let warnings = validate(&AppConfig::default()).unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn missing_fields_deserialize_with_defaults() {
        // config.json без полей triggers/words/sounds → дефолты, не падение.
        let cfg: AppConfig =
            serde_json::from_str(r#"{"master_volume":0.5,"allow_overlap":true,"auto_start":false}"#)
                .unwrap();
        assert!(cfg.triggers.is_empty());
        assert_eq!(cfg.master_volume, 0.5);
    }
}
