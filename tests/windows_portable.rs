// Exercise the Windows state machine and hook installation on every host.
// Windows already runs these modules' unit tests in the asp test binary.
#![cfg(not(windows))]

#[path = "../src/hook_config.rs"]
mod hook_config;
#[path = "../src/windows/hooks.rs"]
mod hooks;
#[path = "../src/windows/state.rs"]
mod state;

#[path = "../src/audio_samples.rs"]
mod audio_samples;
