#[cfg(windows)]
mod audio_samples;

mod hook_config;

// Keep the existing macOS modules at crate scope for their shared UI state.
#[cfg(target_os = "macos")]
include!("macos.rs");

#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows::run()
}

#[cfg(not(any(target_os = "macos", windows)))]
compile_error!("Agents Sleep Preventer supports macOS and Windows.");
