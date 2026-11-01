#[cfg(windows)]
mod audio_samples;

#[cfg(not(target_os = "linux"))]
mod hook_config;
#[cfg(not(target_os = "linux"))]
mod vocabulary;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod voice;

// Keep the existing macOS modules at crate scope for their shared UI state.
#[cfg(target_os = "macos")]
include!("macos.rs");

#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows::run()
}

// Server mode: Telegram, RAM and disk watch, workshop in tmux.
#[cfg(target_os = "linux")]
include!("linux.rs");

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
compile_error!("Agents Sleep Preventer supports macOS, Linux and Windows.");
