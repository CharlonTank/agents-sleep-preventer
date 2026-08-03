//! Dictation feedback sounds: a bloop when the hotkey starts recording and
//! the reversed bloop when it is released.
//!
//! NSSound instances are preloaded once and reused, so pressing the hotkey
//! plays instantly (spawning `afplay` would add ~100ms of startup latency).

use objc::runtime::{BOOL, NO, YES};
use objc::{class, msg_send, sel, sel_impl};
use std::env;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::objc_utils::{nsstring, Id};
use crate::settings::AppSettings;

#[derive(Clone, Copy)]
pub enum Cue {
    Start,
    Stop,
}

impl Cue {
    fn filename(self) -> &'static str {
        match self {
            Self::Start => "dictation-start.wav",
            Self::Stop => "dictation-stop.wav",
        }
    }
}

struct LoadedSounds {
    start: Option<Id>,
    stop: Option<Id>,
}

// Only touched behind the mutex below, from the agent's update loop.
unsafe impl Send for LoadedSounds {}

fn sound_path(filename: &str) -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let bundled = exe
        .parent() // MacOS
        .and_then(|p| p.parent()) // Contents
        .map(|p| p.join("Resources").join(filename))?;
    if bundled.exists() {
        return Some(bundled);
    }
    // Dev fallback: running from the cargo target dir
    let repo_asset = PathBuf::from("sounds").join(filename);
    repo_asset.exists().then_some(repo_asset)
}

fn load(filename: &str) -> Option<Id> {
    let path = sound_path(filename)?;
    unsafe {
        let sound: Id = msg_send![class!(NSSound), alloc];
        let sound: Id = msg_send![
            sound,
            initWithContentsOfFile: nsstring(&path.to_string_lossy())
            byReference: NO
        ];
        (!sound.is_null()).then_some(sound)
    }
}

fn sounds() -> &'static Mutex<LoadedSounds> {
    static SOUNDS: OnceLock<Mutex<LoadedSounds>> = OnceLock::new();
    SOUNDS.get_or_init(|| {
        Mutex::new(LoadedSounds {
            start: load(Cue::Start.filename()),
            stop: load(Cue::Stop.filename()),
        })
    })
}

/// Play a dictation cue at the configured volume (0 = muted). Silently does
/// nothing when the asset is missing.
pub fn play(cue: Cue) {
    // Read fresh so a Settings change applies to the next cue.
    let speech = AppSettings::load().speech_to_text;
    if speech.sound_muted {
        return;
    }
    let volume = speech.sound_volume.clamp(0.0, 1.0);
    if volume <= 0.0 {
        return;
    }

    let guard = sounds().lock().unwrap();
    let sound = match cue {
        Cue::Start => guard.start,
        Cue::Stop => guard.stop,
    };
    let Some(sound) = sound else {
        return;
    };
    unsafe {
        // Restart from the beginning if the previous cue is still playing.
        let playing: BOOL = msg_send![sound, isPlaying];
        if playing == YES {
            let _: BOOL = msg_send![sound, stop];
        }
        // setVolume: takes a C float (f32); passing a CGFloat/f64 corrupts the
        // value on arm64 (the callee reads only the low 32 bits of the double).
        let _: () = msg_send![sound, setVolume: volume];
        let _: BOOL = msg_send![sound, play];
    }
}
