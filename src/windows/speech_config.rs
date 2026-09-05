use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy)]
pub struct Model {
    pub id: &'static str,
    pub label: &'static str,
    pub filename: &'static str,
    pub repository: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    pub parakeet: bool,
}

pub const MODELS: [Model; 3] = [
    Model {
        id: "large-v3-turbo-q5_0",
        label: "Whisper Turbo · 574 MB",
        filename: "ggml-large-v3-turbo-q5_0.bin",
        repository: "ggerganov/whisper.cpp",
        bytes: 574041195,
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        parakeet: false,
    },
    Model {
        id: "parakeet-v3",
        label: "Parakeet v3 · 669 MB",
        filename: "ggml-parakeet-tdt-0.6b-v3-q8_0.bin",
        repository: "ggml-org/parakeet-GGUF",
        bytes: 668757119,
        sha256: "4d64e9e96c2792186d072fde0034df0ad670cf680a2f53069052ead827fd600e",
        parakeet: true,
    },
    Model {
        id: "tiny",
        label: "Whisper Tiny · 78 MB",
        filename: "ggml-tiny.bin",
        repository: "ggerganov/whisper.cpp",
        bytes: 77691713,
        sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
        parakeet: false,
    },
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub model: String,
    pub language: String,
    pub hotkey: String,
    pub vocabulary: String,
    pub sounds: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "parakeet-v3".into(),
            language: "en".into(),
            hotkey: "Ctrl+Alt+Space".into(),
            vocabulary: String::new(),
            sounds: true,
        }
    }
}

impl Settings {
    pub fn load(dir: &Path) -> Result<Self> {
        match fs::read(dir.join("dictation.json")) {
            Ok(bytes) => {
                let settings: Self =
                    serde_json::from_slice(&bytes).context("Invalid dictation.json")?;
                settings.validate()?;
                Ok(settings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        self.validate()?;
        fs::create_dir_all(dir)?;
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer_pretty(&mut temp, self)?;
        temp.as_file().sync_all()?;
        temp.persist(dir.join("dictation.json"))?;
        Ok(())
    }
    pub fn validate(&self) -> Result<()> {
        self.selected_model()?;
        Hotkey::parse(&self.hotkey)?;
        ensure!(
            self.language == "auto"
                || ((2..=3).contains(&self.language.len())
                    && self.language.bytes().all(|c| c.is_ascii_lowercase())),
            "Use a language code such as en, fr, de, es, or auto"
        );
        ensure!(
            self.vocabulary.len() <= 4000,
            "Vocabulary must be at most 4000 bytes"
        );
        Ok(())
    }
    pub fn selected_model(&self) -> Result<Model> {
        MODELS
            .iter()
            .find(|m| m.id == self.model)
            .copied()
            .context("Unknown dictation model")
    }
    pub fn model_path(&self, dir: &Path) -> Result<PathBuf> {
        Ok(dir.join("models").join(self.selected_model()?.filename))
    }
    pub fn ready(&self, dir: &Path) -> bool {
        self.selected_model().is_ok_and(|m| {
            fs::metadata(dir.join("models").join(m.filename)).is_ok_and(|s| s.len() == m.bytes)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub key: u32,
}

impl Hotkey {
    pub fn parse(text: &str) -> Result<Self> {
        let mut modifiers = 0;
        let mut key = None;
        for token in text.split('+').map(str::trim) {
            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => modifiers |= 2,
                "alt" => modifiers |= 1,
                "shift" => modifiers |= 4,
                "win" | "super" => modifiers |= 8,
                value => {
                    ensure!(key.is_none(), "Choose exactly one key in the shortcut");
                    key = Some(match value {
                        "space" => 0x20,
                        s if s.len() == 1 && s.as_bytes()[0].is_ascii_alphanumeric() => {
                            s.as_bytes()[0].to_ascii_uppercase() as u32
                        }
                        s if s.starts_with('f') => {
                            let n: u32 = s[1..].parse().context("Invalid function key")?;
                            ensure!((1..=24).contains(&n), "Use F1 through F24");
                            0x70 + n - 1
                        }
                        _ => bail!("Use a letter, number, Space, or F1-F24 in the shortcut"),
                    });
                }
            }
        }
        ensure!(
            modifiers != 0,
            "The shortcut needs Ctrl, Alt, Shift, or Win"
        );
        Ok(Self {
            modifiers,
            key: key.context("Missing shortcut key")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortcuts_and_settings_validate_without_losing_preferences() {
        assert_eq!(
            Hotkey::parse("Ctrl+Alt+Space").unwrap(),
            Hotkey {
                modifiers: 3,
                key: 32
            }
        );
        for invalid in ["Space", "Ctrl", "Ctrl+A+B", "Ctrl+F25"] {
            assert!(Hotkey::parse(invalid).is_err());
        }
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            language: "fr".into(),
            vocabulary: "Marius, Cleemo".into(),
            ..Settings::default()
        };
        settings.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), settings);
        fs::write(dir.path().join("dictation.json"), "bad json").unwrap();
        assert!(Settings::load(dir.path()).is_err());
    }
}
