//! A received message as readable text: context (forward, quoted reply),
//! text, downloaded attachments and transcribed voice notes.

use super::api::{ApiError, Params};
use super::bot::{say, Bot, API_TIMEOUT};
use super::store::{Fields, Object};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Below this peak the note is silent (microphone off is about -91 dB):
/// on silence Parakeet invents words ("Thank you."), so it isn't asked.
const SILENT_DB: f32 = -60.0;

enum Unheard {
    Silent(f32),
    Failed(String),
}

impl Bot {
    pub(crate) fn describe(&self, message: &Object, update_id: i64, text: &str, speaker: &str) -> String {
        let mut lines = self.context(message, speaker);
        if !text.is_empty() {
            lines.push(text.to_string());
        }
        if let Err(missing) = self.attachments(message, update_id, &mut lines) {
            lines.push(missing);
        }
        if lines.is_empty() {
            say!(self, "(message sans texte)", "(message without text)")
        } else {
            lines.join("\n")
        }
    }

    /// Stops at the first attachment that can't be downloaded, as a line.
    fn attachments(&self, message: &Object, update_id: i64, lines: &mut Vec<String>) -> Result<(), String> {
        let voice = message
            .object("voice")
            .or_else(|| message.object("audio"))
            .or_else(|| message.object("video_note"));
        if let Some(voice) = voice {
            let file = self.fetch(voice, &say!(self, "vocal", "voice note"), &format!("{update_id}-vocal.ogg"))?;
            let duration = voice.int("duration").map_or("?".to_string(), |d| d.to_string());
            let said = self.transcribe(&file);
            lines.push(say!(self, "[vocal {duration} s, transcrit] {said}", "[voice {duration} s, transcribed] {said}"));
        }
        if let Some(photo) = message.list("photo").last().and_then(Value::as_object) {
            let file = self.fetch(photo, "photo", &format!("{update_id}-photo.jpg"))?;
            lines.push(format!("[photo] {}", file.display()));
        }
        for kind in ["document", "video"] {
            if let Some(item) = message.object(kind) {
                let name = item
                    .text("file_name")
                    .and_then(|name| Path::new(name).file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| kind.to_string());
                let file = self.fetch(item, kind, &format!("{update_id}-{name}"))?;
                lines.push(format!("[{kind}] {}", file.display()));
            }
        }
        Ok(())
    }

    fn fetch(&self, item: &Object, kind: &str, name: &str) -> Result<PathBuf, String> {
        let download = || -> Result<PathBuf, ApiError> {
            let file_id = item.text("file_id").unwrap_or("");
            let info = self
                .telegram()
                .call("getFile", &Params::new().set("file_id", file_id), API_TIMEOUT, None)?;
            let remote = info
                .get("file_path")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::Network("getFile: no file_path".to_string()))?;
            let target = self.store.path(&["attachments", name]);
            self.telegram().download(remote, &target)?;
            Ok(target)
        };
        download().map_err(|error| {
            let heavy = item
                .int("file_size")
                .filter(|size| *size > 0)
                .map(|size| {
                    let megabytes = size as f64 / 1e6;
                    say!(self, ", {megabytes:.0} Mo", ", {megabytes:.0} MB")
                })
                .unwrap_or_default();
            say!(self, "[{kind} non téléchargé{heavy} : {error}]", "[{kind} not downloaded{heavy}: {error}]")
        })
    }

    fn transcribe(&self, file: &Path) -> String {
        match self.hear(file) {
            Ok(text) => text,
            Err(Unheard::Silent(peak)) => say!(
                self,
                "(vocal muet : aucun son enregistré, crête {peak:.0} dB)",
                "(silent voice note: nothing recorded, peak {peak:.0} dB)"
            ),
            Err(Unheard::Failed(reason)) => {
                say!(self, "(vocal non transcrit : {reason})", "(voice note not transcribed: {reason})")
            }
        }
    }

    fn hear(&self, file: &Path) -> Result<String, Unheard> {
        let samples = decode(file).map_err(Unheard::Failed)?;
        let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        let decibels = if peak > 0.0 { 20.0 * peak.log10() } else { -120.0 };
        if decibels < SILENT_DB {
            return Err(Unheard::Silent(decibels));
        }
        let model = self
            .config()
            .ok()
            .and_then(|settings| settings.text("parakeet_model").map(PathBuf::from))
            .filter(|dir| dir.is_dir())
            .or_else(crate::dictation::parakeet_model_dir)
            .ok_or_else(|| {
                Unheard::Failed(say!(
                    self,
                    "modèle Parakeet absent : choisis-le dans les réglages de dictée d’ASP",
                    "Parakeet model missing: pick it in ASP's dictation settings"
                ))
            })?;
        crate::dictation::transcribe_recording(&model, &samples).map_err(Unheard::Failed)
    }
}

/// 16 kHz mono samples of any audio file Telegram sends (OGG/Opus voice
/// notes, MP3, video notes), decoded by ffmpeg.
fn decode(file: &Path) -> Result<Vec<f32>, String> {
    let ffmpeg = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let output = Command::new(&ffmpeg)
        .args(["-nostdin", "-loglevel", "error", "-i"])
        .arg(file)
        .args(["-ac", "1", "-ar", "16000", "-f", "s16le", "-"])
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg: {}", reason.trim()));
    }
    Ok(output
        .stdout
        .chunks_exact(2)
        .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32768.0)
        .collect())
}
