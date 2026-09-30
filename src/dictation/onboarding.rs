use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::logging;
use crate::native_dialogs;

use super::transcription::{DictationSetupStatus, WhisperTranscriber};

/// Check if this is the first launch (no preferences file exists)
fn is_first_launch() -> bool {
    !get_prefs_path().exists() && !get_legacy_prefs_path().exists()
}

/// Mark that onboarding has been completed
fn mark_onboarding_complete() {
    let prefs_path = get_prefs_path();
    if let Some(parent) = prefs_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&prefs_path, "onboarding_complete=true\n");
}

fn get_prefs_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("AgentsSleepPreventer")
        .join("preferences.txt")
}

fn get_legacy_prefs_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ClaudeSleepPreventer")
        .join("preferences.txt")
}

/// Run the onboarding flow if this is the first launch.
/// If `auto_dismiss_final` is true, the final "Ready!" modal will auto-close.
pub fn run_onboarding_if_needed(auto_dismiss_final: bool) {
    if !is_first_launch() {
        return;
    }

    logging::log("[onboarding] First launch detected, starting setup...");

    // Permissions are asked by the menu bar app's single panel, which
    // re-checks live and moves the app to /Applications first; onboarding
    // only offers the dictation model.
    let model_window = native_dialogs::SetupWindow::new("Dictation Model", "Checking model...");
    setup_whisper_model(&model_window);

    if auto_dismiss_final {
        model_window.close();
        mark_onboarding_complete();
        logging::log("[onboarding] Setup complete (auto-dismiss)");
        return;
    }
    let final_message = if WhisperTranscriber::new().setup_status() == DictationSetupStatus::Ready {
        let hotkey_label = crate::settings::AppSettings::load().selected_hotkey().label;
        format!("Setup complete.\n\nPress {} to dictate text.", hotkey_label)
    } else {
        "Setup complete.\n\nTo enable dictation, open Settings and download the dictation model."
            .to_string()
    };
    model_window.set_title("Ready!");
    model_window.show_progress(true);
    model_window.set_progress(100.0);
    model_window.set_message(&final_message);
    model_window.set_primary_button("OK");
    model_window.set_secondary_visible(false);
    model_window.wait_for_action();
    model_window.close();

    mark_onboarding_complete();
    logging::log("[onboarding] Setup complete");
}

pub(super) fn open_accessibility_settings() {
    let _ = Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
}

/// Ensure the model currently selected in settings is downloaded, prompting the
/// user if it is missing. Used after the user changes the model in Settings.
pub fn ensure_selected_model_downloaded() {
    if WhisperTranscriber::selected_model_downloaded() {
        return;
    }
    let window = native_dialogs::SetupWindow::new("Dictation Model", "Checking model...");
    setup_whisper_model(&window);
    window.close();
}

fn setup_whisper_model(window: &native_dialogs::SetupWindow) {
    window.show_progress(true);
    window.set_progress(66.0);

    if WhisperTranscriber::selected_model_downloaded() {
        logging::log("[onboarding] Selected dictation model already available");
        return;
    }

    let model_label = crate::settings::AppSettings::load().selected_model().label;
    let message = format!(
        "Dictation uses a local speech model.\n\n{}\n\nDownload it now?",
        model_label
    );

    window.set_title("Dictation Model");
    window.set_message(&message);
    window.set_primary_button("Download");
    window.set_secondary_button("Later");
    window.set_secondary_visible(true);

    if window.wait_for_action() == native_dialogs::SetupAction::Secondary {
        logging::log("[onboarding] User skipped dictation model download");
        return;
    }

    match super::transcription::download_model_with_window(window) {
        Ok(()) => {
            window.set_title("Download Complete");
            window.show_progress(true);
            window.set_progress(100.0);
            window.set_message("The dictation model has been downloaded.");
            window.set_primary_button("Continue");
            window.set_secondary_visible(false);
            window.wait_for_action();
        }
        Err(e) => {
            window.set_title("Download Failed");
            window.set_message(&format!("Download failed:\n\n{}", e));
            window.set_primary_button("OK");
            window.set_secondary_visible(false);
            window.wait_for_action();
        }
    }
}
