//! A received message as readable text: context (forward, quoted reply),
//! text, downloaded attachments and transcribed voice notes.

use super::api::{ApiError, Params};
use super::bot::{say, Bot, API_TIMEOUT};
use super::store::{Fields, Object};
use serde_json::Value;
use std::path::{Path, PathBuf};

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

/// 16 kHz mono samples of any audio file Telegram sends (Ogg Opus voice
/// notes, MP3, video notes), decoded and resampled by Core Audio.
fn decode(file: &Path) -> Result<Vec<f32>, String> {
    use core_foundation::base::TCFType;
    use core_foundation::url::CFURL;
    use std::ffi::c_void;

    #[repr(C)]
    struct StreamFormat {
        sample_rate: f64,
        format_id: u32,
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        bytes_per_frame: u32,
        channels_per_frame: u32,
        bits_per_channel: u32,
        reserved: u32,
    }
    #[repr(C)]
    struct Buffer {
        channels: u32,
        byte_size: u32,
        data: *mut c_void,
    }
    #[repr(C)]
    struct BufferList {
        count: u32,
        buffers: [Buffer; 1],
    }
    #[link(name = "AudioToolbox", kind = "framework")]
    extern "C" {
        fn ExtAudioFileOpenURL(url: *const c_void, file: *mut *mut c_void) -> i32;
        fn ExtAudioFileSetProperty(file: *mut c_void, id: u32, size: u32, data: *const c_void) -> i32;
        fn ExtAudioFileRead(file: *mut c_void, frames: *mut u32, data: *mut BufferList) -> i32;
        fn ExtAudioFileDispose(file: *mut c_void) -> i32;
    }
    const CLIENT_DATA_FORMAT: u32 = u32::from_be_bytes(*b"cfmt");
    const LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
    const FLOAT_PACKED: u32 = 1 | 8; // kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked

    let url = CFURL::from_path(file, false).ok_or_else(|| format!("{}: bad path", file.display()))?;
    let mut handle: *mut c_void = std::ptr::null_mut();
    let status = unsafe { ExtAudioFileOpenURL(url.as_concrete_TypeRef() as *const c_void, &mut handle) };
    if status != 0 || handle.is_null() {
        return Err(format!("Core Audio can't open the file ({status})"));
    }
    let wanted = StreamFormat {
        sample_rate: 16_000.0,
        format_id: LINEAR_PCM,
        format_flags: FLOAT_PACKED,
        bytes_per_packet: 4,
        frames_per_packet: 1,
        bytes_per_frame: 4,
        channels_per_frame: 1,
        bits_per_channel: 32,
        reserved: 0,
    };
    let mut samples = Vec::new();
    let result = (|| {
        let status = unsafe {
            ExtAudioFileSetProperty(
                handle,
                CLIENT_DATA_FORMAT,
                std::mem::size_of::<StreamFormat>() as u32,
                &wanted as *const StreamFormat as *const c_void,
            )
        };
        if status != 0 {
            return Err(format!("Core Audio can't convert the file ({status})"));
        }
        let mut chunk = vec![0f32; 16_000];
        loop {
            let mut frames = chunk.len() as u32;
            let mut list = BufferList {
                count: 1,
                buffers: [Buffer {
                    channels: 1,
                    byte_size: (chunk.len() * 4) as u32,
                    data: chunk.as_mut_ptr() as *mut c_void,
                }],
            };
            let status = unsafe { ExtAudioFileRead(handle, &mut frames, &mut list) };
            if status != 0 {
                return Err(format!("Core Audio read failed ({status})"));
            }
            if frames == 0 {
                return Ok(());
            }
            samples.extend_from_slice(&chunk[..frames as usize]);
        }
    })();
    unsafe { ExtAudioFileDispose(handle) };
    result.map(|()| samples)
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn decodes_a_wav_to_16_khz_mono() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tone.wav");
        let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut writer = hound::WavWriter::create(&file, spec).unwrap();
        for i in 0..48_000 {
            let value = ((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 16_000.0) as i16;
            writer.write_sample(value).unwrap();
            writer.write_sample(value).unwrap();
        }
        writer.finalize().unwrap();
        let samples = decode(&file).unwrap();
        assert!((15_900..=16_100).contains(&samples.len()), "{} samples", samples.len());
        let peak = samples.iter().fold(0f32, |p, s| p.max(s.abs()));
        assert!(peak > 0.4 && peak < 0.6, "peak {peak}");
    }

    /// `ASP_VOICE_NOTE=note.ogg cargo test --release real_voice_note -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_voice_note() {
        let file = std::path::PathBuf::from(std::env::var("ASP_VOICE_NOTE").unwrap());
        let started = std::time::Instant::now();
        let samples = decode(&file).unwrap();
        let model = crate::dictation::parakeet_model_dir().expect("Parakeet model");
        let text = crate::dictation::transcribe_recording(&model, &samples).unwrap();
        println!("{:.1} s of audio in {:.1} s: {text}", samples.len() as f32 / 16_000.0, started.elapsed().as_secs_f32());
        assert!(!text.is_empty());
    }
}
