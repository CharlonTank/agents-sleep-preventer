use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::logging;
use crate::native_dialogs::{
    self, PermissionToggle, PermissionsAction, PermissionsWindow, PermissionsWindowHandle,
};

use super::audio::{
    check_microphone_permission, request_microphone_permission_sync, MicrophonePermission,
};
use super::globe_key::{check_input_monitoring_permission, request_input_monitoring_permission};
use super::text_injection::check_accessibility_permission;
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

    let welcome_message = r#"Privacy is at the core of Agents Sleep Preventer.
Allow these permissions to enable local voice dictation."#;

    let window = PermissionsWindow::new("Set Up Permissions", welcome_message);
    window.set_primary_button("Continue Setup");
    window.set_secondary_button("Later");
    window.set_secondary_visible(true);
    window.set_progress(25.0);

    loop {
        update_permission_toggles(&window);
        let refresh_flag = Arc::new(AtomicBool::new(true));
        let refresh_flag_thread = refresh_flag.clone();
        let window_handle = window.handle();
        let refresh_thread = std::thread::spawn(move || {
            while refresh_flag_thread.load(Ordering::Relaxed) {
                update_permission_toggles_handle(&window_handle);
                std::thread::sleep(Duration::from_millis(500));
            }
        });

        let action = window.wait_for_action();
        refresh_flag.store(false, Ordering::Relaxed);
        let _ = refresh_thread.join();

        match action {
            PermissionsAction::Primary => {
                window.close();
                break;
            }
            PermissionsAction::Secondary => {
                window.close();
                logging::log("[onboarding] User skipped onboarding");
                return;
            }
            PermissionsAction::Toggle(toggle) => handle_permission_toggle(toggle),
        }
    }

    let model_window = native_dialogs::SetupWindow::new("Whisper Model", "Checking model...");
    setup_whisper_model(&model_window);

    if auto_dismiss_final {
        model_window.close();
        mark_onboarding_complete();
        logging::log("[onboarding] Setup complete (auto-dismiss)");
        return;
    }
    let final_message = if WhisperTranscriber::new().setup_status() == DictationSetupStatus::Ready {
        "Setup complete.\n\nPress Fn+Shift to dictate text."
    } else {
        "Setup complete.\n\nTo enable dictation, open Settings and download the Whisper model."
    };
    model_window.set_title("Ready!");
    model_window.show_progress(true);
    model_window.set_progress(100.0);
    model_window.set_message(final_message);
    model_window.set_primary_button("OK");
    model_window.set_secondary_visible(false);
    model_window.wait_for_action();
    model_window.close();

    mark_onboarding_complete();
    logging::log("[onboarding] Setup complete");
}

fn permission_button_label(granted: bool) -> &'static str {
    if granted {
        "Allowed"
    } else {
        "Allow"
    }
}

fn update_permission_toggles(window: &PermissionsWindow) {
    let input_ok = check_input_monitoring_permission();
    let mic_ok = matches!(check_microphone_permission(), MicrophonePermission::Granted);
    let accessibility_ok = check_accessibility_permission();

    window.set_toggle(
        PermissionToggle::InputMonitoring,
        permission_button_label(input_ok),
        input_ok,
    );
    window.set_toggle(
        PermissionToggle::Microphone,
        permission_button_label(mic_ok),
        mic_ok,
    );
    window.set_toggle(
        PermissionToggle::Accessibility,
        permission_button_label(accessibility_ok),
        accessibility_ok,
    );
}

fn update_permission_toggles_handle(handle: &PermissionsWindowHandle) {
    let input_ok = check_input_monitoring_permission();
    let mic_ok = matches!(check_microphone_permission(), MicrophonePermission::Granted);
    let accessibility_ok = check_accessibility_permission();

    handle.set_toggle(
        PermissionToggle::InputMonitoring,
        permission_button_label(input_ok),
        input_ok,
    );
    handle.set_toggle(
        PermissionToggle::Microphone,
        permission_button_label(mic_ok),
        mic_ok,
    );
    handle.set_toggle(
        PermissionToggle::Accessibility,
        permission_button_label(accessibility_ok),
        accessibility_ok,
    );
}

fn handle_permission_toggle(toggle: PermissionToggle) {
    match toggle {
        PermissionToggle::InputMonitoring => {
            if !check_input_monitoring_permission() {
                if !request_input_monitoring_permission() {
                    open_input_monitoring_settings();
                    native_dialogs::show_dialog(
                        "In System Settings > Privacy & Security > Input Monitoring,\nclick +, select AgentsSleepPreventer.app in /Applications, then enable the switch.",
                        "Input Monitoring",
                    );
                }
            }
        }
        PermissionToggle::Microphone => {
            let mut status = check_microphone_permission();
            if status == MicrophonePermission::NotDetermined {
                let granted = request_microphone_permission_sync();
                status = if granted {
                    MicrophonePermission::Granted
                } else {
                    MicrophonePermission::Denied
                };
            }
            if status != MicrophonePermission::Granted {
                open_microphone_settings();
            }
        }
        PermissionToggle::Accessibility => {
            open_accessibility_settings();
        }
    }
}

fn open_input_monitoring_settings() {
    let _ = Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent")
        .spawn();
}

fn open_microphone_settings() {
    let _ = Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
        .spawn();
}

fn open_accessibility_settings() {
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
    let window = native_dialogs::SetupWindow::new("Whisper Model", "Checking model...");
    setup_whisper_model(&window);
    window.close();
}

fn setup_whisper_model(window: &native_dialogs::SetupWindow) {
    window.show_progress(true);
    window.set_progress(66.0);

    if WhisperTranscriber::selected_model_downloaded() {
        logging::log("[onboarding] Selected Whisper model already available");
        return;
    }

    let model_label = crate::settings::AppSettings::load().selected_model().label;
    let message = format!(
        "Dictation uses a local Whisper model.\n\n{}\n\nDownload it now?",
        model_label
    );

    window.set_title("Whisper Model");
    window.set_message(&message);
    window.set_primary_button("Download");
    window.set_secondary_button("Later");
    window.set_secondary_visible(true);

    if window.wait_for_action() == native_dialogs::SetupAction::Secondary {
        logging::log("[onboarding] User skipped Whisper model download");
        return;
    }

    match super::transcription::download_model_with_window(window) {
        Ok(()) => {
            window.set_title("Download Complete");
            window.show_progress(true);
            window.set_progress(100.0);
            window.set_message("The Whisper model has been downloaded.");
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
