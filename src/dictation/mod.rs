mod audio;
mod globe_key;
mod onboarding;
mod overlay;
mod text_injection;
mod transcription;

use crate::logging;
use crate::native_dialogs;
use crate::settings::AppSettings;
use audio::{
    check_microphone_permission, request_microphone_permission_sync, AudioRecorder,
    MicrophonePermission,
};
use globe_key::{GlobeKeyEvent, GlobeKeyManager};
pub use onboarding::{ensure_selected_model_downloaded, run_onboarding_if_needed};
use crate::settings::ModelEngine;
use overlay::{OverlayMode, RecordingOverlay};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
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

/// Live Parakeet transcription running while the hotkey is held.
struct StreamingSession {
    stop: Arc<AtomicBool>,
    partial_rx: Receiver<String>,
    result_rx: Receiver<DictationResult>,
}

pub struct DictationManager {
    state: DictationState,
    globe_key: GlobeKeyManager,
    recorder: Option<AudioRecorder>,
    transcriber: WhisperTranscriber,
    overlay: RecordingOverlay,
    result_rx: Option<Receiver<DictationResult>>,
    streaming: Option<StreamingSession>,
    enabled: bool,
    last_diag_log: Instant,
    last_flags_seen: Instant,
    last_no_flags_log: Instant,
    accessibility_granted: bool,
    accessibility_alert_shown: bool,
    last_permission_check: Instant,
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
            streaming: None,
            enabled: true,
            last_diag_log: Instant::now(),
            last_flags_seen: Instant::now(),
            last_no_flags_log: Instant::now(),
            accessibility_granted: false,
            accessibility_alert_shown: false,
            last_permission_check: Instant::now(),
        }
    }

    pub fn start(&mut self) -> Result<(), String> {
        if !self.transcriber.is_available() {
            return Err(
                "Whisper model not found. Use 'Setup Dictation...' from the menu to download it."
                    .to_string(),
            );
        }

        // Accessibility covers both the CGEventTap listener (TCC treats it as
        // a superset of Input Monitoring) and CGEventPost text injection, so
        // it is the only key-related permission the app needs.
        self.accessibility_granted = text_injection::check_accessibility_permission();
        if !self.accessibility_granted {
            logging::log(
                "[dictation] Accessibility not granted; skipping globe key listener start",
            );
            return Ok(());
        }

        self.reload_hotkey();

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
        if let Some(session) = self.streaming.take() {
            session.stop.store(true, AtomicOrdering::Relaxed);
        }
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

    /// Re-read the configured dictation hotkey from settings (e.g. after the
    /// user changed it in Settings). Takes effect immediately, no restart needed.
    pub fn reload_hotkey(&mut self) {
        let mask = AppSettings::load().selected_hotkey().mask;
        globe_key::set_required_mask(mask);
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
                    "[globe_key] No modifier events seen for 15s. Check Accessibility permission.",
                );
            }
        }

        self.recheck_accessibility_permission();

        // Live preview: show the latest partial transcription while recording
        if self.state == DictationState::Recording {
            if let Some(session) = &self.streaming {
                let mut latest = None;
                while let Ok(text) = session.partial_rx.try_recv() {
                    latest = Some(text);
                }
                if let Some(text) = latest {
                    self.overlay.set_preview_text(&text);
                }
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

    /// Detect Accessibility permission being revoked or granted while the
    /// app is running (e.g. a macOS update silently resets TCC grants, or the
    /// user fixes it from the alert below) and react without requiring a
    /// relaunch: warn once on revoke, auto-(re)start the listener on grant.
    fn recheck_accessibility_permission(&mut self) {
        if self.last_permission_check.elapsed() < Duration::from_secs(10) {
            return;
        }
        self.last_permission_check = Instant::now();

        let granted = text_injection::check_accessibility_permission();
        if granted == self.accessibility_granted {
            return;
        }
        self.accessibility_granted = granted;

        if granted {
            logging::log("[dictation] Accessibility permission granted, starting listener");
            self.accessibility_alert_shown = false;
            if let Err(e) = self.start() {
                logging::log(&format!(
                    "[dictation] Failed to start after permission grant: {}",
                    e
                ));
            }
            return;
        }

        logging::log("[dictation] Accessibility permission was revoked; dictation shortcut disabled");
        self.globe_key.stop();
        if !self.accessibility_alert_shown {
            self.accessibility_alert_shown = true;
            if native_dialogs::show_confirm_dialog(
                "macOS revoked Accessibility access for Agents Sleep Preventer, so the dictation shortcut no longer works.\n\nRe-enable it in System Settings > Privacy & Security > Accessibility.",
                "Dictation Shortcut Disabled",
                "Open Settings",
                "Later",
            ) {
                onboarding::open_accessibility_settings();
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

        // Parakeet streams live: load the model now (while the user speaks)
        // and re-transcribe the buffer periodically for the preview.
        if let Some((ModelEngine::Parakeet, model_dir)) = self.transcriber.model_info() {
            let recorder = self.recorder.as_ref().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let (partial_tx, partial_rx) = mpsc::channel();
            let (result_tx, result_rx) = mpsc::channel();
            transcription::spawn_parakeet_stream(
                model_dir,
                recorder.live_buffer(),
                recorder.channels(),
                recorder.sample_rate(),
                stop.clone(),
                partial_tx,
                result_tx,
            );
            self.streaming = Some(StreamingSession {
                stop,
                partial_rx,
                result_rx,
            });
            logging::log("[dictation] Streaming session started (Parakeet)");
        }

        // Show overlay
        self.overlay.show();
        self.state = DictationState::Recording;
        logging::log("[dictation] Recording started");
    }

    fn stop_and_transcribe(&mut self) {
        // Switch overlay to transcribing mode (orange)
        self.overlay.set_mode(OverlayMode::Transcribing);

        // Streaming path: the model is already loaded in the streaming
        // thread; it produces the final transcription on the result channel.
        if let Some(session) = self.streaming.take() {
            if let Some(recorder) = self.recorder.as_mut() {
                recorder.stop_stream();
            }
            self.recorder = None;
            session.stop.store(true, AtomicOrdering::Relaxed);
            self.result_rx = Some(session.result_rx);
            self.state = DictationState::Transcribing;
            logging::log("[dictation] Recording stopped, finalizing stream...");
            return;
        }

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
