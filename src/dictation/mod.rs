mod audio;
mod globe_key;
mod onboarding;
mod overlay;
mod text_injection;
mod transcription;

use crate::logging;
use audio::{
    check_microphone_permission, request_microphone_permission_sync, AudioRecorder,
    MicrophonePermission,
};
use globe_key::{GlobeKeyEvent, GlobeKeyManager};
pub use onboarding::{ensure_selected_model_downloaded, run_onboarding_if_needed};
use overlay::{OverlayMode, RecordingOverlay};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use transcription::WhisperTranscriber;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DictationState {
    Idle,
    Recording,
    Transcribing,
}

pub enum DictationResult {
    Transcribed(String),
    Error(String),
}

pub struct DictationManager {
    state: DictationState,
    globe_key: GlobeKeyManager,
    recorder: Option<AudioRecorder>,
    transcriber: WhisperTranscriber,
    overlay: RecordingOverlay,
    result_rx: Option<Receiver<DictationResult>>,
    enabled: bool,
    last_diag_log: Instant,
    last_flags_seen: Instant,
    last_no_flags_log: Instant,
}

impl DictationManager {
    pub fn new() -> Self {
        Self {
            state: DictationState::Idle,
            globe_key: GlobeKeyManager::new(),
            recorder: None,
            transcriber: WhisperTranscriber::new(),
            overlay: RecordingOverlay::new(),
            result_rx: None,
            enabled: true,
            last_diag_log: Instant::now(),
            last_flags_seen: Instant::now(),
            last_no_flags_log: Instant::now(),
        }
    }

    pub fn start(&mut self) -> Result<(), String> {
        if !self.transcriber.is_available() {
            return Err(
                "Whisper model not found. Use 'Setup Dictation...' from the menu to download it."
                    .to_string(),
            );
        }

        if !globe_key::check_input_monitoring_permission() {
            logging::log(
                "[dictation] Input Monitoring not granted; skipping globe key listener start",
            );
            return Ok(());
        }

        // Check/request microphone permission
        let mut mic_permission = check_microphone_permission();
        if mic_permission == MicrophonePermission::NotDetermined {
            logging::log("[dictation] Microphone permission not yet determined, requesting...");
            mic_permission = if request_microphone_permission_sync() {
                MicrophonePermission::Granted
            } else {
                MicrophonePermission::Denied
            };
        }

        logging::log(&format!(
            "[dictation] Microphone permission: {:?}",
            mic_permission
        ));
        if mic_permission == MicrophonePermission::Denied {
            logging::log("[dictation] Microphone permission denied");
        }

        self.globe_key.start()
    }

    pub fn stop(&mut self) {
        self.globe_key.stop();
        self.overlay.hide();
        self.state = DictationState::Idle;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_available(&self) -> bool {
        self.transcriber.is_available()
    }

    /// Re-evaluate which Whisper model is on disk (e.g. after the user changed
    /// the model in Settings or downloaded one).
    pub fn reload_transcriber(&mut self) {
        self.transcriber = WhisperTranscriber::new();
    }

    pub fn update(&mut self) {
        if !self.enabled {
            return;
        }

        // Check for globe key events
        while let Some(event) = self.globe_key.try_recv() {
            match event {
                GlobeKeyEvent::Ready => {
                    logging::log("[dictation] Globe key listener ready (event)");
                }
                GlobeKeyEvent::DictateStart => {
                    logging::log("[dictation] DictateStart event received");
                    if self.state == DictationState::Idle {
                        self.start_recording();
                    }
                }
                GlobeKeyEvent::DictateStop => {
                    logging::log("[dictation] DictateStop event received");
                    if self.state == DictationState::Recording {
                        self.stop_and_transcribe();
                    }
                }
            }
        }

        if self.last_diag_log.elapsed() >= Duration::from_secs(2) {
            self.last_diag_log = Instant::now();
            let diag = globe_key::take_diagnostics();
            if diag.flags_events > 0 {
                self.last_flags_seen = Instant::now();
                if let Some(keycode) = diag.last_keycode {
                    logging::log(&format!(
                        "[globe_key] flags events={}, last keycode={}, raw flags=0x{:x}",
                        diag.flags_events, keycode, diag.last_flags_raw
                    ));
                } else {
                    logging::log(&format!(
                        "[globe_key] flags events={}, raw flags=0x{:x}",
                        diag.flags_events, diag.last_flags_raw
                    ));
                }
            }

            if diag.disabled_timeout > 0 || diag.disabled_user_input > 0 {
                logging::log(&format!(
                    "[globe_key] tap disabled: timeout={}, user_input={}",
                    diag.disabled_timeout, diag.disabled_user_input
                ));
            }

            if self.last_flags_seen.elapsed() >= Duration::from_secs(15)
                && self.last_no_flags_log.elapsed() >= Duration::from_secs(15)
            {
                self.last_no_flags_log = Instant::now();
                logging::log(
                    "[globe_key] No modifier events seen for 15s. Check Input Monitoring permission.",
                );
            }
        }

        // Check for transcription results
        if self.state == DictationState::Transcribing {
            if let Some(rx) = &self.result_rx {
                match rx.try_recv() {
                    Ok(DictationResult::Transcribed(text)) => {
                        logging::log(&format!("[dictation] Transcription: {}", text));
                        self.overlay.hide();
                        if let Err(e) = text_injection::inject_text(&text) {
                            logging::log(&format!("[dictation] Failed to inject text: {}", e));
                        }
                        self.state = DictationState::Idle;
                        self.result_rx = None;
                    }
                    Ok(DictationResult::Error(e)) => {
                        logging::log(&format!("[dictation] Transcription error: {}", e));
                        self.overlay.hide();
                        self.state = DictationState::Idle;
                        self.result_rx = None;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        // Still processing
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        logging::log("[dictation] Transcription channel disconnected");
                        self.overlay.hide();
                        self.state = DictationState::Idle;
                        self.result_rx = None;
                    }
                }
            }
        }
    }

    fn start_recording(&mut self) {
        // Initialize recorder
        match AudioRecorder::new() {
            Ok(mut recorder) => {
                if let Err(e) = recorder.start_recording() {
                    logging::log(&format!("[dictation] Failed to start recording: {}", e));
                    return;
                }
                self.recorder = Some(recorder);
            }
            Err(e) => {
                logging::log(&format!("[dictation] Failed to create recorder: {}", e));
                return;
            }
        }

        // Show overlay
        self.overlay.show();
        self.state = DictationState::Recording;
        logging::log("[dictation] Recording started");
    }

    fn stop_and_transcribe(&mut self) {
        // Switch overlay to transcribing mode (orange)
        self.overlay.set_mode(OverlayMode::Transcribing);

        // Get samples from recorder
        let samples = match self.recorder.as_mut() {
            Some(recorder) => recorder.stop_recording(),
            None => {
                logging::log("[dictation] No recorder available");
                self.overlay.hide();
                self.state = DictationState::Idle;
                return;
            }
        };

        if samples.is_empty() {
            logging::log("[dictation] No audio recorded");
            self.overlay.hide();
            self.state = DictationState::Idle;
            return;
        }

        // Log audio stats
        let duration_secs = samples.len() as f32 / 48000.0; // Assuming 48kHz
        logging::log(&format!(
            "[dictation] Audio: {} samples, ~{:.1}s duration",
            samples.len(),
            duration_secs
        ));

        // Save to temp file
        let temp_dir = std::env::temp_dir();
        let audio_path = temp_dir.join(format!("dictation_{}.wav", std::process::id()));

        let recorder = self.recorder.take().unwrap();
        if let Err(e) = recorder.save_to_wav(&samples, &audio_path) {
            logging::log(&format!("[dictation] Failed to save audio: {}", e));
            self.overlay.hide();
            self.state = DictationState::Idle;
            return;
        }

        // Start transcription in background thread
        let (tx, rx): (Sender<DictationResult>, Receiver<DictationResult>) = mpsc::channel();
        self.result_rx = Some(rx);
        self.state = DictationState::Transcribing;

        let transcriber = WhisperTranscriber::new();
        thread::spawn(move || {
            let result = match transcriber.transcribe(&audio_path) {
                Ok(text) => DictationResult::Transcribed(text),
                Err(e) => DictationResult::Error(e),
            };

            // Clean up audio file
            let _ = std::fs::remove_file(&audio_path);

            let _ = tx.send(result);
        });

        logging::log("[dictation] Recording stopped, transcribing...");
    }
}

impl Drop for DictationManager {
    fn drop(&mut self) {
        self.stop();
    }
}
