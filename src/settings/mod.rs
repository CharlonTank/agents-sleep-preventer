//! Application settings with JSON persistence

pub mod window;

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Sleep prevention settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SleepPreventionSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for SleepPreventionSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Speech-to-text settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechToTextSettings {
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub vocabulary_words: Vec<String>,
}

impl Default for SpeechToTextSettings {
    fn default() -> Self {
        Self {
            language: default_language(),
            model: default_model(),
            vocabulary_words: Vec::new(),
        }
    }
}

/// A downloadable Whisper model the user can choose from.
#[derive(Debug, Clone, Copy)]
pub struct WhisperModelChoice {
    /// Stable identifier persisted in settings.
    pub id: &'static str,
    /// Human-readable label shown in the picker.
    pub label: &'static str,
    /// GGML file name as published on Hugging Face and stored locally.
    pub filename: &'static str,
}

impl WhisperModelChoice {
    /// Download URL for this model on the whisper.cpp Hugging Face repo.
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            self.filename
        )
    }
}

/// Application settings
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppSettings {
    #[serde(default)]
    pub sleep_prevention: SleepPreventionSettings,
    #[serde(default)]
    pub speech_to_text: SpeechToTextSettings,
}

fn default_true() -> bool {
    true
}

fn default_language() -> String {
    "en".to_string()
}

fn default_model() -> String {
    "large-v3-turbo-q5_0".to_string()
}

impl AppSettings {
    /// Get the settings file path
    pub fn settings_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("AgentsSleepPreventer")
            .join("settings.json")
    }

    fn legacy_settings_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("ClaudeSleepPreventer")
            .join("settings.json")
    }

    /// Load settings from disk, returning defaults if file doesn't exist or is invalid
    pub fn load() -> Self {
        let primary_path = Self::settings_path();
        let legacy_path = Self::legacy_settings_path();
        let path = if primary_path.exists() {
            primary_path
        } else if legacy_path.exists() {
            legacy_path
        } else {
            return Self::default();
        };

        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save settings to disk
    pub fn save(&self) -> Result<(), String> {
        let path = Self::settings_path();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create settings directory: {}", e))?;
        }

        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize settings: {}", e))?;

        fs::write(&path, content).map_err(|e| format!("Failed to write settings: {}", e))
    }

    /// Get the list of supported languages for speech-to-text
    pub fn supported_languages() -> Vec<(&'static str, &'static str)> {
        vec![
            ("en", "English"),
            ("auto", "Auto-detect"),
            ("fr", "French"),
            ("de", "German"),
            ("es", "Spanish"),
            ("it", "Italian"),
            ("pt", "Portuguese"),
            ("zh", "Chinese"),
            ("ja", "Japanese"),
            ("ko", "Korean"),
        ]
    }

    /// The Whisper models the user can choose to download, ordered from
    /// lightest/fastest to highest quality. The first entry is the default.
    pub fn supported_models() -> Vec<WhisperModelChoice> {
        vec![
            WhisperModelChoice {
                id: "large-v3-turbo-q5_0",
                label: "Turbo — Faster (574 MB)",
                filename: "ggml-large-v3-turbo-q5_0.bin",
            },
            WhisperModelChoice {
                id: "large-v3-turbo",
                label: "Turbo — Higher quality (1.6 GB)",
                filename: "ggml-large-v3-turbo.bin",
            },
        ]
    }

    /// The currently selected model, falling back to the default (first entry)
    /// when the stored id is unknown.
    pub fn selected_model(&self) -> WhisperModelChoice {
        let id = self.speech_to_text.model.as_str();
        let models = Self::supported_models();
        models
            .iter()
            .find(|m| m.id == id)
            .copied()
            .unwrap_or(models[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_settings() {
        let settings = AppSettings::default();
        assert!(settings.sleep_prevention.enabled);
        assert_eq!(settings.speech_to_text.language, "en");
        assert!(settings.speech_to_text.vocabulary_words.is_empty());
    }

    #[test]
    fn test_deserialize_partial() {
        let json = r#"{"sleep_prevention": {"enabled": false}}"#;
        let settings: AppSettings = serde_json::from_str(json).unwrap();
        assert!(!settings.sleep_prevention.enabled);
        // speech_to_text should have defaults
        assert_eq!(settings.speech_to_text.language, "en");
    }
}
