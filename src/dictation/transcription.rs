use std::env;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;

use objc::{class, msg_send, sel, sel_impl};

use crate::native_dialogs;
use crate::settings::AppSettings;

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
    model_path: Option<PathBuf>,
    whisper_path: PathBuf,
}

impl WhisperTranscriber {
    pub fn new() -> Self {
        let whisper_path = Self::find_whisper_cli();
        let model_path = Self::find_model();

        Self {
            model_path,
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
        if self.model_path.is_some() {
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

    /// The model file name the user has selected in settings.
    fn selected_model_filename() -> String {
        // Dev override: WHISPER_MODEL=large-v3-turbo -> ggml-large-v3-turbo.bin
        if let Ok(name) = env::var("WHISPER_MODEL") {
            return format!("ggml-{}.bin", name);
        }
        AppSettings::load().selected_model().filename.to_string()
    }

    /// Whether the model currently selected in settings is already downloaded.
    pub fn selected_model_downloaded() -> bool {
        let filename = Self::selected_model_filename();
        Self::model_dirs()
            .iter()
            .any(|dir| dir.join(&filename).exists())
    }

    fn find_model() -> Option<PathBuf> {
        // 1. The selected model, if downloaded.
        let selected = Self::selected_model_filename();
        for dir in Self::model_dirs() {
            let path = dir.join(&selected);
            if path.exists() {
                return Some(path);
            }
        }

        // 2. Any other known model we downloaded previously, so dictation keeps
        //    working for existing installs that have a different model on disk.
        let known: Vec<String> = AppSettings::supported_models()
            .iter()
            .map(|m| m.filename.to_string())
            .chain(LEGACY_MODEL_FILENAMES.iter().map(|s| s.to_string()))
            .collect();
        for dir in Self::model_dirs() {
            for filename in &known {
                let path = dir.join(filename);
                if path.exists() {
                    return Some(path);
                }
            }
        }

        // 3. Homebrew location (if the user installed whisper-cpp before).
        let homebrew_dir = PathBuf::from("/opt/homebrew/share/whisper-cpp/models");
        for filename in &known {
            let path = homebrew_dir.join(filename);
            if path.exists() {
                return Some(path);
            }
        }
        for stem in ["ggml-large-v3-turbo", "ggml-medium", "ggml-base"] {
            let quantized = homebrew_dir.join(format!("{}-q5_0.bin", stem));
            if quantized.exists() {
                return Some(quantized);
            }
        }

        None
    }

    pub fn is_available(&self) -> bool {
        self.model_path.is_some()
    }

    pub fn transcribe(&self, audio_path: &PathBuf) -> Result<String, String> {
        let model_path = self
            .model_path
            .as_ref()
            .ok_or("No Whisper model found. Use Setup Dictation to download.")?;

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

pub(crate) fn download_model_with_window(
    window: &native_dialogs::SetupWindow,
) -> Result<(), String> {
    window.set_title("Downloading Whisper Model");
    window.set_message("Downloading Whisper model... 0%");
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
    let model_path = models_dir.join(model.filename);
    let model_url = model.url();
    let model_path_for_thread = model_path.clone();
    let handle = window.handle();
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let result = download_model_with_progress(&model_path_for_thread, &model_url, &handle);
        let _ = tx.send(result);
        handle.stop_modal();
    });

    window.run_modal();

    let result = rx
        .recv()
        .unwrap_or_else(|_| Err("Download interrupted".to_string()));

    if result.is_err() {
        let _ = fs::remove_file(&model_path);
    }

    window.show_progress(false);
    window.set_primary_enabled(true);

    result
}

fn download_model_with_progress(
    model_path: &PathBuf,
    model_url: &str,
    progress: &native_dialogs::SetupWindowHandle,
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
                    let whole = percent.floor() as i32;
                    if whole != last_percent {
                        last_percent = whole;
                        progress.set_progress(percent);
                        progress.set_message(&format!("Downloading Whisper model... {}%", whole));
                    }
                }
                line.clear();
            } else {
                line.push(ch);
            }
        }
    }

    if let Some(percent) = extract_percent(&line) {
        progress.set_progress(percent);
        progress.set_message(&format!(
            "Downloading Whisper model... {}%",
            percent.floor() as i32
        ));
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
