//! Application settings with JSON persistence

pub mod window;

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Manual override of the sleep behavior (popover tri-state control).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SleepOverride {
    /// Prevent sleep while agents are working (normal behavior).
    #[default]
    Auto,
    /// Keep the Mac awake even when no agent is working.
    ForceAwake,
    /// Never prevent sleep, even while agents are working.
    ForceSleep,
}

impl SleepOverride {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ForceAwake => "awake",
            Self::ForceSleep => "sleep",
        }
    }
}

/// Sleep prevention settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SleepPreventionSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Manual override: force-awake / force-sleep / auto.
    #[serde(default)]
    pub force: SleepOverride,
}

impl Default for SleepPreventionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            force: SleepOverride::Auto,
        }
    }
}

/// Agent notification settings (task finished, agent needs attention)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for NotificationSettings {
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
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    /// Volume of the start/stop dictation cues, 0.0 to 1.0.
    #[serde(default = "default_sound_volume")]
    pub sound_volume: f32,
    /// Mute the start/stop dictation cues without losing the volume setting.
    #[serde(default)]
    pub sound_muted: bool,
}

impl Default for SpeechToTextSettings {
    fn default() -> Self {
        Self {
            language: default_language(),
            model: default_model(),
            vocabulary_words: Vec::new(),
            hotkey: default_hotkey(),
            sound_volume: default_sound_volume(),
            sound_muted: false,
        }
    }
}

/// A configurable modifier-key combo held down to start/stop dictation.
#[derive(Debug, Clone, Copy)]
pub struct HotkeyChoice {
    /// Stable identifier persisted in settings.
    pub id: &'static str,
    /// Human-readable label shown in the picker.
    pub label: &'static str,
    /// Bitmask of CGEventFlags modifier bits that must all be held together.
    pub mask: u64,
}

/// The dictation shortcut actually in effect: either a preset modifier combo
/// or a user-recorded one (`custom:<mask-hex>:<keycode|none>:<label>`),
/// which may include a regular key on top of the modifiers.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedHotkey {
    /// CGEventFlags modifier bits that must all be held.
    pub mask: u64,
    /// Regular key (CGKeyCode) that must be held too, if any.
    pub keycode: Option<u16>,
    /// Human-readable label.
    pub label: String,
}

impl ResolvedHotkey {
    /// Persisted id for a user-recorded shortcut.
    pub fn custom_id(&self) -> String {
        format!(
            "custom:{:x}:{}:{}",
            self.mask,
            self.keycode
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.label
        )
    }

    /// Parse a `custom:` id; None when it is not one or is malformed.
    pub fn parse_custom(id: &str) -> Option<Self> {
        let rest = id.strip_prefix("custom:")?;
        let mut parts = rest.splitn(3, ':');
        let mask = u64::from_str_radix(parts.next()?, 16).ok()?;
        let key_part = parts.next()?;
        let keycode = if key_part == "none" {
            None
        } else {
            Some(key_part.parse().ok()?)
        };
        let label = parts.next().unwrap_or("Custom").to_string();
        if mask == 0 && keycode.is_none() {
            return None;
        }
        Some(Self {
            mask,
            keycode,
            label,
        })
    }
}

// CGEventFlags modifier bits (Core Graphics / Quartz Event Services).
// Same values as the NSEvent device-independent modifier flags.
pub const CG_FLAG_SHIFT: u64 = 0x00020000;
pub const CG_FLAG_CONTROL: u64 = 0x00040000;
pub const CG_FLAG_OPTION: u64 = 0x00080000;
pub const CG_FLAG_COMMAND: u64 = 0x00100000;
pub const CG_FLAG_FN: u64 = 0x00800000;

/// Transcription engine backing a model choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelEngine {
    /// whisper.cpp via the bundled whisper-cli binary.
    Whisper,
    /// Parakeet TDT via in-process ONNX Runtime (transcribe-rs).
    Parakeet,
}

/// Files making up the Parakeet ONNX model directory.
pub const PARAKEET_FILES: [&str; 4] = [
    "encoder-model.int8.onnx",
    "decoder_joint-model.int8.onnx",
    "nemo128.onnx",
    "vocab.txt",
];

/// A downloadable transcription model the user can choose from.
#[derive(Debug, Clone, Copy)]
pub struct ModelChoice {
    /// Stable identifier persisted in settings.
    pub id: &'static str,
    /// Human-readable label shown in the picker.
    pub label: &'static str,
    /// On-disk name under the models dir: a GGML file for Whisper,
    /// a directory for Parakeet.
    pub filename: &'static str,
    pub engine: ModelEngine,
}

impl ModelChoice {
    /// Files to download: (url, path relative to the models dir).
    pub fn download_files(&self) -> Vec<(String, PathBuf)> {
        match self.engine {
            ModelEngine::Whisper => vec![(
                format!(
                    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
                    self.filename
                ),
                PathBuf::from(self.filename),
            )],
            ModelEngine::Parakeet => PARAKEET_FILES
                .iter()
                .map(|f| {
                    (
                        format!(
                            "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/{}",
                            f
                        ),
                        PathBuf::from(self.filename).join(f),
                    )
                })
                .collect(),
        }
    }

    /// Whether this model is fully present under the given models dir.
    pub fn is_downloaded_in(&self, models_dir: &std::path::Path) -> bool {
        match self.engine {
            ModelEngine::Whisper => models_dir.join(self.filename).exists(),
            ModelEngine::Parakeet => {
                let dir = models_dir.join(self.filename);
                PARAKEET_FILES.iter().all(|f| dir.join(f).exists())
            }
        }
    }
}

/// Application settings
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppSettings {
    #[serde(default)]
    pub sleep_prevention: SleepPreventionSettings,
    #[serde(default)]
    pub speech_to_text: SpeechToTextSettings,
    #[serde(default)]
    pub notifications: NotificationSettings,
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

fn default_hotkey() -> String {
    "fn_shift".to_string()
}

fn default_sound_volume() -> f32 {
    0.7
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

        // Atomic tmp+rename: other asp processes read this file every sync,
        // and a raced truncate-then-write would deserialize as defaults. The
        // pid suffix keeps two concurrent short-lived writers apart.
        let tmp = path.with_file_name(format!("settings.json.{}.tmp", std::process::id()));
        fs::write(&tmp, content).map_err(|e| format!("Failed to write settings: {}", e))?;
        fs::rename(&tmp, &path).map_err(|e| format!("Failed to persist settings: {}", e))
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

    /// The transcription models the user can choose to download.
    /// The first entry is the default.
    pub fn supported_models() -> Vec<ModelChoice> {
        vec![
            ModelChoice {
                id: "large-v3-turbo-q5_0",
                label: "Whisper Turbo — Fast (574 MB)",
                filename: "ggml-large-v3-turbo-q5_0.bin",
                engine: ModelEngine::Whisper,
            },
            ModelChoice {
                id: "large-v3-turbo",
                label: "Whisper Turbo — Best accuracy (1.6 GB)",
                filename: "ggml-large-v3-turbo.bin",
                engine: ModelEngine::Whisper,
            },
            ModelChoice {
                id: "parakeet-tdt-0.6b-v3-int8",
                label: "Parakeet v3 — Fastest (670 MB, EU languages)",
                filename: "parakeet-tdt-0.6b-v3-int8",
                engine: ModelEngine::Parakeet,
            },
        ]
    }

    /// The currently selected model, falling back to the default (first entry)
    /// when the stored id is unknown.
    pub fn selected_model(&self) -> ModelChoice {
        let id = self.speech_to_text.model.as_str();
        let models = Self::supported_models();
        models
            .iter()
            .find(|m| m.id == id)
            .copied()
            .unwrap_or(models[0])
    }

    /// The modifier-key combos the user can choose to trigger dictation.
    /// The first entry is the default.
    pub fn supported_hotkeys() -> Vec<HotkeyChoice> {
        vec![
            HotkeyChoice {
                id: "fn_shift",
                label: "Fn + Shift",
                mask: CG_FLAG_FN | CG_FLAG_SHIFT,
            },
            HotkeyChoice {
                id: "fn_control",
                label: "Fn + Control",
                mask: CG_FLAG_FN | CG_FLAG_CONTROL,
            },
            HotkeyChoice {
                id: "fn_option",
                label: "Fn + Option",
                mask: CG_FLAG_FN | CG_FLAG_OPTION,
            },
            HotkeyChoice {
                id: "control_option",
                label: "Control + Option",
                mask: CG_FLAG_CONTROL | CG_FLAG_OPTION,
            },
            HotkeyChoice {
                id: "command_shift",
                label: "Command + Shift",
                mask: CG_FLAG_COMMAND | CG_FLAG_SHIFT,
            },
        ]
    }

    /// The currently selected hotkey, falling back to the default (first entry)
    /// when the stored id is unknown.
    pub fn selected_hotkey(&self) -> HotkeyChoice {
        let id = self.speech_to_text.hotkey.as_str();
        let hotkeys = Self::supported_hotkeys();
        hotkeys
            .iter()
            .find(|h| h.id == id)
            .copied()
            .unwrap_or(hotkeys[0])
    }

    /// The dictation shortcut in effect: a user-recorded `custom:` combo
    /// wins, otherwise the selected preset (with its usual fallback).
    pub fn resolved_hotkey(&self) -> ResolvedHotkey {
        if let Some(custom) = ResolvedHotkey::parse_custom(&self.speech_to_text.hotkey) {
            return custom;
        }
        let preset = self.selected_hotkey();
        ResolvedHotkey {
            mask: preset.mask,
            keycode: None,
            label: preset.label.to_string(),
        }
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
        // Fields absent from older settings files get their defaults
        assert_eq!(settings.sleep_prevention.force, SleepOverride::Auto);
        // speech_to_text should have defaults
        assert_eq!(settings.speech_to_text.language, "en");
    }

    #[test]
    fn test_model_download_files() {
        let models = AppSettings::supported_models();
        let whisper = models.iter().find(|m| m.engine == ModelEngine::Whisper).unwrap();
        assert_eq!(whisper.download_files().len(), 1);

        let parakeet = models.iter().find(|m| m.engine == ModelEngine::Parakeet).unwrap();
        let files = parakeet.download_files();
        assert_eq!(files.len(), 4);
        for (url, rel) in &files {
            assert!(url.starts_with("https://huggingface.co/istupakov/"));
            assert!(rel.starts_with("parakeet-tdt-0.6b-v3-int8"));
        }
    }

    #[test]
    fn test_selected_hotkey_falls_back_to_default_when_unknown() {
        let mut settings = AppSettings::default();
        assert_eq!(settings.selected_hotkey().id, "fn_shift");

        settings.speech_to_text.hotkey = "fn_option".to_string();
        assert_eq!(settings.selected_hotkey().id, "fn_option");

        settings.speech_to_text.hotkey = "bogus".to_string();
        assert_eq!(settings.selected_hotkey().id, "fn_shift");
    }

    #[test]
    fn test_custom_hotkey_roundtrip_and_resolution() {
        let custom = ResolvedHotkey {
            mask: CG_FLAG_COMMAND | CG_FLAG_OPTION,
            keycode: Some(49),
            label: "⌥⌘Space".to_string(),
        };
        let parsed = ResolvedHotkey::parse_custom(&custom.custom_id()).unwrap();
        assert_eq!(parsed, custom);

        let mut settings = AppSettings::default();
        settings.speech_to_text.hotkey = custom.custom_id();
        assert_eq!(settings.resolved_hotkey(), custom);

        // Preset ids resolve through the preset table
        settings.speech_to_text.hotkey = "fn_shift".to_string();
        let resolved = settings.resolved_hotkey();
        assert_eq!(resolved.mask, CG_FLAG_FN | CG_FLAG_SHIFT);
        assert_eq!(resolved.keycode, None);

        // Malformed / degenerate customs fall back to the preset default
        assert!(ResolvedHotkey::parse_custom("custom:0:none:Nothing").is_none());
        assert!(ResolvedHotkey::parse_custom("custom:zz:12:Bad").is_none());

        // Labels containing ':' survive the roundtrip
        let weird = ResolvedHotkey {
            mask: CG_FLAG_CONTROL,
            keycode: Some(41),
            label: "⌃:".to_string(),
        };
        assert_eq!(ResolvedHotkey::parse_custom(&weird.custom_id()).unwrap(), weird);
    }
}
