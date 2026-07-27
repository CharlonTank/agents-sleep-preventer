use std::env;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;

use objc::{class, msg_send, sel, sel_impl};
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;

use crate::logging;
use crate::native_dialogs;
use crate::settings::{AppSettings, ModelChoice, ModelEngine};

/// Model files we know how to use when locating an already-downloaded model.
/// Includes the legacy default (`ggml-medium.bin`) so existing installs keep
/// working after the default model changed to large-v3-turbo.
const LEGACY_MODEL_FILENAMES: &[&str] = &["ggml-medium.bin", "ggml-base.bin"];

#[derive(Debug, Clone, PartialEq)]
pub enum DictationSetupStatus {
    Ready,
    MissingModel,
}

pub struct WhisperTranscriber {
    /// Model file (Whisper) or model directory (Parakeet), with its engine.
    model: Option<(ModelEngine, PathBuf)>,
    whisper_path: PathBuf,
}

impl WhisperTranscriber {
    pub fn new() -> Self {
        let whisper_path = Self::find_whisper_cli();
        let model = Self::find_model();

        Self {
            model,
            whisper_path,
        }
    }

    /// Find whisper-cli: bundled first, then homebrew, then system PATH
    fn find_whisper_cli() -> PathBuf {
        // Try bundled version first (in app's Resources folder)
        if let Some(exe_path) = env::current_exe().ok() {
            let resources = exe_path
                .parent() // MacOS
                .and_then(|p| p.parent()) // Contents
                .map(|p| p.join("Resources").join("whisper-cli"));

            if let Some(bundled) = resources {
                if bundled.exists() {
                    return bundled;
                }
            }
        }

        // Try common homebrew locations (not in PATH when launched from /Applications)
        let homebrew_paths = [
            "/opt/homebrew/bin/whisper-cli", // Apple Silicon
            "/usr/local/bin/whisper-cli",    // Intel Mac
        ];

        for path in homebrew_paths {
            let p = PathBuf::from(path);
            if p.exists() {
                return p;
            }
        }

        // Fall back to system whisper-cli (relies on PATH)
        PathBuf::from("whisper-cli")
    }

    pub fn setup_status(&self) -> DictationSetupStatus {
        if self.model.is_some() {
            DictationSetupStatus::Ready
        } else {
            DictationSetupStatus::MissingModel
        }
    }

    /// Get the app support directory for storing models
    fn app_support_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("AgentsSleepPreventer")
    }

    fn legacy_app_support_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("ClaudeSleepPreventer")
    }

    /// Directories where we store downloaded models, in priority order.
    fn model_dirs() -> [PathBuf; 2] {
        [
            Self::app_support_dir().join("models"),
            Self::legacy_app_support_dir().join("models"),
        ]
    }

    /// The model the user has selected in settings.
    fn selected_model_choice() -> ModelChoice {
        // Dev override: WHISPER_MODEL=large-v3-turbo forces that Whisper model.
        if let Ok(name) = env::var("WHISPER_MODEL") {
            let id = name.trim();
            if let Some(m) = AppSettings::supported_models()
                .into_iter()
                .find(|m| m.id == id)
            {
                return m;
            }
        }
        AppSettings::load().selected_model()
    }

    /// Whether the model currently selected in settings is already downloaded.
    pub fn selected_model_downloaded() -> bool {
        let model = Self::selected_model_choice();
        Self::model_dirs()
            .iter()
            .any(|dir| model.is_downloaded_in(dir))
    }

    fn find_model() -> Option<(ModelEngine, PathBuf)> {
        // 1. The selected model, if downloaded.
        let selected = Self::selected_model_choice();
        for dir in Self::model_dirs() {
            if selected.is_downloaded_in(&dir) {
                return Some((selected.engine, dir.join(selected.filename)));
            }
        }

        // 2. Any other known model we downloaded previously, so dictation keeps
        //    working for existing installs that have a different model on disk.
        for dir in Self::model_dirs() {
            for model in AppSettings::supported_models() {
                if model.is_downloaded_in(&dir) {
                    return Some((model.engine, dir.join(model.filename)));
                }
            }
            for filename in LEGACY_MODEL_FILENAMES {
                let path = dir.join(filename);
                if path.exists() {
                    return Some((ModelEngine::Whisper, path));
                }
            }
        }

        // 3. Homebrew location (if the user installed whisper-cpp before).
        let homebrew_dir = PathBuf::from("/opt/homebrew/share/whisper-cpp/models");
        let known: Vec<&str> = AppSettings::supported_models()
            .iter()
            .filter(|m| m.engine == ModelEngine::Whisper)
            .map(|m| m.filename)
            .chain(LEGACY_MODEL_FILENAMES.iter().copied())
            .collect();
        for filename in known {
            let path = homebrew_dir.join(filename);
            if path.exists() {
                return Some((ModelEngine::Whisper, path));
            }
        }
        for stem in ["ggml-large-v3-turbo", "ggml-medium", "ggml-base"] {
            let quantized = homebrew_dir.join(format!("{}-q5_0.bin", stem));
            if quantized.exists() {
                return Some((ModelEngine::Whisper, quantized));
            }
        }

        None
    }

    pub fn is_available(&self) -> bool {
        self.model.is_some()
    }

    /// Engine and on-disk location of the resolved model, if any.
    pub fn model_info(&self) -> Option<(ModelEngine, PathBuf)> {
        self.model.clone()
    }

    pub fn transcribe(&self, audio_path: &PathBuf) -> Result<String, String> {
        let (engine, model_path) = self
            .model
            .as_ref()
            .ok_or("No transcription model found. Use Setup Dictation to download.")?;

        match engine {
            ModelEngine::Whisper => self.transcribe_whisper(model_path, audio_path),
            ModelEngine::Parakeet => transcribe_parakeet(model_path, audio_path),
        }
    }

    fn transcribe_whisper(
        &self,
        model_path: &PathBuf,
        audio_path: &PathBuf,
    ) -> Result<String, String> {
        let language = preferred_language().unwrap_or_else(|| "auto".to_string());
        let vocabulary = get_vocabulary_prompt();

        // Audio is already 16kHz mono WAV from AudioRecorder
        let mut cmd = Command::new(&self.whisper_path);
        cmd.args([
            "-m",
            model_path.to_str().unwrap(),
            "-f",
            audio_path.to_str().unwrap(),
            "-t",
            "8", // 8 threads for Apple Silicon
            "--no-timestamps",
        ])
        .args(["--suppress-nst"])
        .args(["-l", &language]);

        // Add vocabulary as initial prompt if available
        if !vocabulary.is_empty() {
            cmd.args(["--prompt", &vocabulary]);
        }

        let output = cmd
            .output()
            .map_err(|e| format!("whisper-cli failed: {}", e))?;

        if output.status.success() {
            let transcription = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if transcription.is_empty() {
                Err("No speech detected".to_string())
            } else {
                Ok(transcription)
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("Transcription failed: {}", stderr))
        }
    }
}

/// Commit the transcript in chunks while recording so the hotkey release
/// only has to transcribe the last few seconds, never the whole recording.
const COMMIT_TARGET_SECS: usize = 15;
/// Search for the quietest cut point this far back from the commit target,
/// so chunks split on a natural pause instead of mid-word.
const COMMIT_SEARCH_SECS: usize = 5;
/// Silence padding around each transcribed snippet to soften edge effects.
const SNIPPET_PAD_SECS: f32 = 0.2;

/// Quietest 30ms-frame boundary in `raw[from..to]` (channel-aligned).
fn quietest_cut(raw: &[f32], channels: u16, sample_rate: u32, from: usize, to: usize) -> usize {
    let channels = channels.max(1) as usize;
    let frame = ((sample_rate as usize * 30 / 1000) * channels).max(channels);
    let from = from - (from % channels);
    let to = to.min(raw.len());

    let mut best = to - (to % channels);
    let mut best_rms = f32::MAX;
    let mut offset = from;
    while offset + frame <= to {
        let rms = raw[offset..offset + frame]
            .iter()
            .map(|s| s * s)
            .sum::<f32>()
            / frame as f32;
        if rms < best_rms {
            best_rms = rms;
            best = offset + frame;
        }
        offset += frame;
    }
    best
}

/// Transcribe a 16k-mono snippet padded with a little silence; None when
/// empty or failed.
fn transcribe_snippet(model: &mut ParakeetModel, samples: &[f32]) -> Option<String> {
    if samples.is_empty() {
        return None;
    }
    let pad = (SNIPPET_PAD_SECS * 16_000.0) as usize;
    let mut padded = vec![0.0f32; pad];
    padded.extend_from_slice(samples);
    padded.resize(padded.len() + pad, 0.0);
    model
        .transcribe_with(&padded, &ParakeetParams::default())
        .ok()
        .map(|result| result.text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Spawn the Parakeet streaming thread: loads the model once, then keeps a
/// running transcript while recording — audio older than ~15s is committed
/// in chunks cut at natural pauses, and only the uncommitted tail is
/// re-transcribed for the live preview. On `stop` the final text is the
/// committed chunks plus one last pass over the tail, so the release is
/// near-instant regardless of dictation length.
pub(crate) fn spawn_parakeet_stream(
    model_dir: PathBuf,
    buffer: std::sync::Arc<std::sync::Mutex<Vec<f32>>>,
    channels: u16,
    sample_rate: u32,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    partial_tx: mpsc::Sender<String>,
    result_tx: mpsc::Sender<super::DictationResult>,
) {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let mut model = match ParakeetModel::load(&model_dir, &Quantization::Int8) {
            Ok(m) => m,
            Err(e) => {
                // Still honor the hotkey release: report the error once.
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = result_tx.send(super::DictationResult::Error(format!(
                    "Failed to load Parakeet model: {}",
                    e
                )));
                return;
            }
        };
        logging::log(&format!(
            "[transcription] Parakeet stream: model loaded in {:.1}s",
            start.elapsed().as_secs_f32()
        ));

        let snapshot_16k = |raw: &[f32]| super::audio::AudioRecorder::to_16k_mono(raw, channels, sample_rate);

        let commit_target = COMMIT_TARGET_SECS * sample_rate as usize * channels.max(1) as usize;
        let search_span = COMMIT_SEARCH_SECS * sample_rate as usize * channels.max(1) as usize;
        let mut committed: Vec<String> = Vec::new();
        let mut committed_len = 0usize; // raw-domain index of committed audio

        let mut last_len = 0usize;
        'stream: while !stop.load(Ordering::Relaxed) {
            // Sleep in short slices so a hotkey release moves on to the
            // final pass without waiting out the full interval.
            for _ in 0..6 {
                std::thread::sleep(Duration::from_millis(50));
                if stop.load(Ordering::Relaxed) {
                    break 'stream;
                }
            }
            let raw = buffer.lock().unwrap().clone();
            // Skip when no new audio or less than ~0.5s captured.
            if raw.len() == last_len || raw.len() < (sample_rate as usize / 2) {
                continue;
            }
            last_len = raw.len();

            // Commit a chunk once the uncommitted tail is long enough, cut
            // at the quietest frame so words stay whole. One bounded pass
            // every ~15s of speech; the release never re-pays for it.
            if raw.len() - committed_len > commit_target {
                let target = committed_len + commit_target;
                let cut = quietest_cut(&raw, channels, sample_rate, target - search_span, target);
                if cut > committed_len {
                    let chunk = snapshot_16k(&raw[committed_len..cut]);
                    if let Some(text) = transcribe_snippet(&mut model, &chunk) {
                        committed.push(text);
                    }
                    committed_len = cut;
                }
            }

            // Preview = committed transcript + a fresh pass on the tail.
            let tail = snapshot_16k(&raw[committed_len..]);
            if let Some(text) = transcribe_snippet(&mut model, &tail) {
                let preview = committed
                    .iter()
                    .map(String::as_str)
                    .chain(std::iter::once(text.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ");
                let _ = partial_tx.send(preview);
            }
        }

        // Final: committed chunks + one last pass over the short tail only.
        let raw = buffer.lock().unwrap().clone();
        let tail = snapshot_16k(&raw[committed_len.min(raw.len())..]);
        let outcome = if committed.is_empty() && tail.len() < 1600 {
            super::DictationResult::Error("No audio recorded".to_string())
        } else {
            let mut parts = committed;
            if let Some(text) = transcribe_snippet(&mut model, &tail) {
                parts.push(text);
            }
            let text = parts.join(" ").trim().to_string();
            if text.is_empty() {
                super::DictationResult::Error("No speech detected".to_string())
            } else {
                super::DictationResult::Transcribed(text)
            }
        };
        let _ = result_tx.send(outcome);
    });
}

/// Transcribe in-process with Parakeet via ONNX Runtime (non-streaming path,
/// used when transcribing a saved WAV, e.g. fallback flows).
fn transcribe_parakeet(model_dir: &PathBuf, audio_path: &PathBuf) -> Result<String, String> {
    let start = std::time::Instant::now();
    let mut model = ParakeetModel::load(model_dir, &Quantization::Int8)
        .map_err(|e| format!("Failed to load Parakeet model: {}", e))?;
    let loaded = start.elapsed();

    let samples = transcribe_rs::audio::read_wav_samples(audio_path)
        .map_err(|e| format!("Failed to read audio: {}", e))?;

    let result = model
        .transcribe_with(&samples, &ParakeetParams::default())
        .map_err(|e| format!("Parakeet transcription failed: {}", e))?;

    logging::log(&format!(
        "[transcription] Parakeet: load {:.1}s, total {:.1}s",
        loaded.as_secs_f32(),
        start.elapsed().as_secs_f32()
    ));

    let text = result.text.trim().to_string();
    if text.is_empty() {
        Err("No speech detected".to_string())
    } else {
        Ok(text)
    }
}

pub(crate) fn download_model_with_window(
    window: &native_dialogs::SetupWindow,
) -> Result<(), String> {
    window.set_title("Downloading Model");
    window.set_message("Downloading model... 0%");
    window.set_primary_enabled(false);
    window.set_secondary_visible(false);
    window.show_progress(true);
    window.set_progress(0.0);

    let models_dir = WhisperTranscriber::app_support_dir().join("models");
    if let Err(e) = fs::create_dir_all(&models_dir) {
        window.show_progress(false);
        window.set_primary_enabled(true);
        return Err(format!("Failed to create models directory: {}", e));
    }

    let model = AppSettings::load().selected_model();
    let files: Vec<(String, PathBuf)> = model
        .download_files()
        .into_iter()
        .map(|(url, rel)| (url, models_dir.join(rel)))
        .collect();
    let files_for_thread = files.clone();
    let handle = window.handle();
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let total = files_for_thread.len();
        let mut result = Ok(());
        for (i, (url, path)) in files_for_thread.iter().enumerate() {
            if let Some(parent) = path.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    result = Err(format!("Failed to create model directory: {}", e));
                    break;
                }
            }
            // Scale this file's 0-100% into its slice of the overall progress.
            let base = (i as f64 / total as f64) * 100.0;
            let scale = 1.0 / total as f64;
            result = download_model_with_progress(path, url, &handle, base, scale);
            if result.is_err() {
                let _ = fs::remove_file(path);
                break;
            }
        }
        let _ = tx.send(result);
        handle.stop_modal();
    });

    window.run_modal();

    let result = rx
        .recv()
        .unwrap_or_else(|_| Err("Download interrupted".to_string()));

    window.show_progress(false);
    window.set_primary_enabled(true);

    result
}

fn download_model_with_progress(
    model_path: &PathBuf,
    model_url: &str,
    progress: &native_dialogs::SetupWindowHandle,
    percent_base: f64,
    percent_scale: f64,
) -> Result<(), String> {
    use std::process::Stdio;

    let mut child = Command::new("curl")
        .args([
            "-L",
            "--progress-bar",
            "-o",
            model_path.to_str().unwrap(),
            model_url,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start download: {}", e))?;

    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture download progress".to_string())?;

    let mut buffer = [0u8; 1024];
    let mut line = String::new();
    let mut last_percent = -1i32;

    loop {
        let read = stderr
            .read(&mut buffer)
            .map_err(|e| format!("Failed to read download progress: {}", e))?;
        if read == 0 {
            break;
        }

        let chunk = String::from_utf8_lossy(&buffer[..read]);
        for ch in chunk.chars() {
            if ch == '\r' || ch == '\n' {
                if let Some(percent) = extract_percent(&line) {
                    let overall = percent_base + percent * percent_scale;
                    let whole = overall.floor() as i32;
                    if whole != last_percent {
                        last_percent = whole;
                        progress.set_progress(overall);
                        progress.set_message(&format!("Downloading model... {}%", whole));
                    }
                }
                line.clear();
            } else {
                line.push(ch);
            }
        }
    }

    if let Some(percent) = extract_percent(&line) {
        let overall = percent_base + percent * percent_scale;
        progress.set_progress(overall);
        progress.set_message(&format!("Downloading model... {}%", overall.floor() as i32));
    }

    let status = child
        .wait()
        .map_err(|e| format!("Download failed to finish: {}", e))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("Download failed with status: {}", status))
    }
}

fn extract_percent(line: &str) -> Option<f64> {
    let percent_index = line.rfind('%')?;
    let bytes = line.as_bytes();
    let mut start = percent_index;
    while start > 0 {
        let c = bytes[start - 1] as char;
        if c.is_ascii_digit() || c == '.' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == percent_index {
        return None;
    }
    line[start..percent_index].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// Full streaming-loop check: partials while "recording", final on stop.
    /// Skips when the Parakeet model isn't downloaded.
    #[test]
    fn parakeet_stream_produces_partials_and_final() {
        let model_dir = WhisperTranscriber::app_support_dir()
            .join("models")
            .join("parakeet-tdt-0.6b-v3-int8");
        if !model_dir.join("encoder-model.int8.onnx").exists() {
            eprintln!("Parakeet model not downloaded; skipping");
            return;
        }

        // Generate speech at 16kHz mono so to_16k_mono is a no-op.
        let tmp = std::env::temp_dir();
        let aiff = tmp.join("asp_stream_test.aiff");
        let wav = tmp.join("asp_stream_test.wav");
        let ok = Command::new("say")
            .args(["-o", aiff.to_str().unwrap(), "Streaming dictation preview test."])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && Command::new("afconvert")
                .args([
                    "-f",
                    "WAVE",
                    "-d",
                    "LEI16@16000",
                    "-c",
                    "1",
                    aiff.to_str().unwrap(),
                    wav.to_str().unwrap(),
                ])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        assert!(ok, "failed to generate test audio");
        let samples = transcribe_rs::audio::read_wav_samples(&wav).expect("read wav");

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (partial_tx, partial_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();

        spawn_parakeet_stream(
            model_dir,
            buffer.clone(),
            1,
            16000,
            stop.clone(),
            partial_tx,
            result_tx,
        );

        // Simulate live capture: feed audio in chunks while the model loads
        // and the preview loop runs.
        for chunk in samples.chunks(samples.len() / 4 + 1) {
            buffer.lock().unwrap().extend_from_slice(chunk);
            std::thread::sleep(std::time::Duration::from_millis(700));
        }
        // Give the loop time to emit at least one partial after model load.
        std::thread::sleep(std::time::Duration::from_secs(4));
        stop.store(true, Ordering::Relaxed);

        let final_result = result_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("no final result");
        let final_text = match final_result {
            super::super::DictationResult::Transcribed(t) => t,
            super::super::DictationResult::Error(e) => panic!("final failed: {}", e),
        };
        eprintln!("Stream final: {:?}", final_text);
        assert!(final_text.to_lowercase().contains("streaming"));

        let partials: Vec<String> = partial_rx.try_iter().collect();
        eprintln!("Partials received: {}", partials.len());
        assert!(!partials.is_empty(), "expected at least one partial");

        let _ = fs::remove_file(&aiff);
        let _ = fs::remove_file(&wav);
    }
}

fn preferred_language() -> Option<String> {
    // Check settings first
    let settings = AppSettings::load();
    let lang = &settings.speech_to_text.language;
    if !lang.is_empty() && lang != "auto" {
        return Some(lang.clone());
    }

    // Fallback to environment/system detection
    preferred_language_from_env().or_else(preferred_language_from_system)
}

/// Get vocabulary words as a prompt string for whisper-cli
fn get_vocabulary_prompt() -> String {
    let settings = AppSettings::load();
    let words = &settings.speech_to_text.vocabulary_words;
    if words.is_empty() {
        return String::new();
    }

    // Join vocabulary words with spaces for the initial prompt
    words.join(" ")
}

fn preferred_language_from_env() -> Option<String> {
    let candidates = ["LC_ALL", "LC_CTYPE", "LANG"];
    for key in candidates {
        if let Ok(value) = env::var(key) {
            if let Some(code) = parse_language_code(&value) {
                return Some(code);
            }
        }
    }
    None
}

fn preferred_language_from_system() -> Option<String> {
    #[cfg(target_os = "macos")]
    unsafe {
        let languages: *mut objc::runtime::Object = msg_send![class!(NSLocale), preferredLanguages];
        let count: usize = msg_send![languages, count];
        if count == 0 {
            return None;
        }
        let first: *mut objc::runtime::Object = msg_send![languages, objectAtIndex: 0usize];
        let c_str: *const std::os::raw::c_char = msg_send![first, UTF8String];
        if c_str.is_null() {
            return None;
        }
        let lang = std::ffi::CStr::from_ptr(c_str).to_string_lossy();
        parse_language_code(&lang)
    }

    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn parse_language_code(value: &str) -> Option<String> {
    let trimmed = value.split('.').next().unwrap_or(value).trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut iter = trimmed.split(|c| c == '_' || c == '-');
    let primary = iter.next().unwrap_or("").trim();
    if primary.is_empty() || primary.eq_ignore_ascii_case("C") {
        return None;
    }

    Some(primary.to_lowercase())
}
