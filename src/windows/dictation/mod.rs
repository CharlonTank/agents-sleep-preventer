mod audio;
mod input;
mod overlay;

use super::{
    speech_config::{Hotkey, Settings},
    speech_engine,
};
use crate::audio_samples;
use anyhow::{ensure, Context, Result};
use clap::Subcommand;
use input::{Shortcut, Target};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

#[derive(Subcommand)]
pub enum DictationCommand {
    /// Download and verify the selected local speech model
    Setup {
        #[arg(long)]
        model: Option<String>,
    },
    /// Show dictation settings and readiness
    Status,
    /// Change dictation preferences (the tray reloads them automatically)
    Configure {
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        hotkey: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        vocabulary: Option<String>,
        #[arg(long, hide = true)]
        settings_file: Option<PathBuf>,
    },
    /// Transcribe a WAV file locally, without typing into another app
    Transcribe { file: PathBuf },
    /// Record from the default microphone and print a local transcript
    Record {
        #[arg(long, default_value = "5", value_parser = clap::value_parser!(u64).range(1..=120))]
        seconds: u64,
    },
}

pub fn command(dir: &Path, command: DictationCommand) -> Result<()> {
    let mut settings = Settings::load(dir)?;
    match command {
        DictationCommand::Setup { model } => {
            if let Some(model) = model {
                settings.model = model;
                settings.save(dir)?;
            }
            let mut previous = 101;
            speech_engine::setup(dir, &settings, &AtomicBool::new(false), |bytes, total| {
                let percent = bytes * 100 / total.max(1);
                if percent != previous {
                    eprintln!("Downloading model: {percent}%");
                    previous = percent;
                }
            })?;
            println!(
                "Dictation is ready. Use {} in the tray app.",
                settings.hotkey
            );
        }
        DictationCommand::Status => println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"settings":settings, "model_ready":settings.ready(dir), "engines_ready":speech_engine::runtime_ready()})
            )?
        ),
        DictationCommand::Configure {
            language,
            hotkey,
            model,
            vocabulary,
            settings_file,
        } => {
            if let Some(file) = settings_file {
                settings = serde_json::from_slice(&fs::read(file)?)?;
            }
            if let Some(value) = language {
                settings.language = value;
            }
            if let Some(value) = hotkey {
                settings.hotkey = value;
            }
            if let Some(value) = model {
                settings.model = value;
            }
            if let Some(value) = vocabulary {
                settings.vocabulary = value;
            }
            settings.save(dir)?;
        }
        DictationCommand::Transcribe { file } => {
            let samples = audio_samples::read_wav(&file)?;
            println!(
                "{}",
                speech_engine::transcribe(dir, &settings, &samples, &AtomicBool::new(false))?
            );
        }
        DictationCommand::Record { seconds } => {
            ensure!(
                settings.ready(dir),
                "Download a model with asp dictation setup first"
            );
            let recorder = audio::Recorder::start()?;
            eprintln!("Recording for {seconds} seconds...");
            thread::sleep(Duration::from_secs(seconds));
            println!(
                "{}",
                speech_engine::transcribe(
                    dir,
                    &settings,
                    &recorder.stop()?,
                    &AtomicBool::new(false)
                )?
            );
        }
    }
    Ok(())
}

enum Event {
    Toggle,
    Progress(u64, u64, u64),
    SetupDone(u64, std::result::Result<(), String>),
    Transcript(u64, std::result::Result<String, String>),
}

struct Job {
    cancel: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

pub struct Dictation {
    dir: PathBuf,
    settings: Settings,
    shortcut: Option<Shortcut>,
    overlay: Option<overlay::Overlay>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    recorder: Option<audio::Recorder>,
    started: Instant,
    target: Option<Target>,
    pending: Option<(Target, String, Instant)>,
    job: Option<Job>,
    generation: u64,
    transcribing: bool,
    last_text: String,
    status: String,
    hide_at: Option<Instant>,
    next_settings: Instant,
}

impl Dictation {
    pub fn new(dir: &Path) -> Self {
        let loaded = Settings::load(dir);
        let status = loaded
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let mut value = Self {
            dir: dir.into(),
            settings: loaded.unwrap_or_default(),
            shortcut: None,
            overlay: overlay::Overlay::new().ok(),
            tx,
            rx,
            recorder: None,
            started: Instant::now(),
            target: None,
            pending: None,
            job: None,
            generation: 0,
            transcribing: false,
            last_text: String::new(),
            status,
            hide_at: None,
            next_settings: Instant::now(),
        };
        if let Err(error) = value.register_shortcut() {
            value.status = format!("Shortcut unavailable: {error}");
        }
        if value.status.is_empty() {
            value.idle_status();
        }
        value
    }

    pub fn status(&self) -> &str {
        &self.status
    }
    pub fn busy(&self) -> bool {
        self.recorder.is_some() || self.transcribing
    }

    fn register_shortcut(&mut self) -> Result<()> {
        if !self.settings.enabled {
            self.shortcut = None;
            return Ok(());
        }
        let tx = self.tx.clone();
        self.shortcut = Some(
            Shortcut::register(Hotkey::parse(&self.settings.hotkey)?, move || {
                let _ = tx.send(Event::Toggle);
            })
            .context("choose a different hotkey in Dictation Settings")?,
        );
        Ok(())
    }

    fn idle_status(&mut self) {
        self.status = if !self.settings.enabled {
            "Dictation disabled".into()
        } else if !self.settings.ready(&self.dir) {
            "Dictation: download a model to begin".into()
        } else {
            format!("Dictation ready · {}", self.settings.hotkey)
        };
    }

    fn display(&mut self, text: String, temporary: bool) {
        if let Some(overlay) = &self.overlay {
            overlay.show(&text);
        }
        self.status = text;
        self.hide_at = temporary.then(|| Instant::now() + Duration::from_secs(6));
    }

    fn cue(&self, start: bool) {
        if self.settings.sounds {
            let sound: &[u8] = if start {
                include_bytes!("../../../sounds/dictation-start.wav")
            } else {
                include_bytes!("../../../sounds/dictation-stop.wav")
            };
            unsafe {
                PlaySoundW(
                    sound.as_ptr().cast(),
                    std::ptr::null_mut(),
                    SND_ASYNC | SND_MEMORY | SND_NODEFAULT,
                );
            }
        }
    }

    fn toggle(&mut self) -> Result<()> {
        if self.recorder.is_some() {
            return self.finish_recording();
        }
        ensure!(
            self.job.is_none(),
            "Wait for the current operation, or choose Cancel Dictation"
        );
        ensure!(
            self.settings.enabled,
            "Enable dictation in Dictation Settings"
        );
        ensure!(
            self.settings.ready(&self.dir),
            "Choose Download Dictation Model in the tray menu first"
        );
        ensure!(
            speech_engine::runtime_ready(),
            "Speech engines are missing. Reinstall the complete ZIP."
        );
        self.pending = None;
        self.target = Some(Target::capture());
        self.cue(true);
        self.recorder = Some(audio::Recorder::start()?);
        self.started = Instant::now();
        self.display(
            format!("Recording · {} to finish", self.settings.hotkey),
            false,
        );
        Ok(())
    }

    fn finish_recording(&mut self) -> Result<()> {
        let Some(recorder) = self.recorder.take() else {
            return Ok(());
        };
        let samples = recorder.stop()?;
        self.cue(false);
        ensure!(
            audio_samples::has_audio(&samples),
            "No speech detected. Check the microphone and try again."
        );
        self.display("Transcribing locally…".into(), false);
        self.generation += 1;
        let generation = self.generation;
        let dir = self.dir.clone();
        let settings = self.settings.clone();
        let tx = self.tx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        self.transcribing = true;
        let thread = thread::spawn(move || {
            let result = speech_engine::transcribe(&dir, &settings, &samples, &flag)
                .map_err(|e| e.to_string());
            let _ = tx.send(Event::Transcript(generation, result));
        });
        self.job = Some(Job { cancel, thread });
        Ok(())
    }

    pub fn setup(&mut self) -> Result<()> {
        ensure!(
            self.job.is_none() && self.recorder.is_none(),
            "Finish or cancel the current dictation first"
        );
        self.settings = Settings::load(&self.dir)?;
        self.generation += 1;
        let generation = self.generation;
        let dir = self.dir.clone();
        let settings = self.settings.clone();
        let tx = self.tx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        self.display(
            format!("Preparing {}…", self.settings.selected_model()?.label),
            false,
        );
        let thread = thread::spawn(move || {
            let result = speech_engine::setup(&dir, &settings, &flag, |bytes, total| {
                let _ = tx.send(Event::Progress(generation, bytes, total));
            })
            .map_err(|e| e.to_string());
            let _ = tx.send(Event::SetupDone(generation, result));
        });
        self.job = Some(Job { cancel, thread });
        Ok(())
    }

    fn join_job(&mut self) {
        if let Some(job) = self.job.take() {
            let _ = job.thread.join();
        }
        self.transcribing = false;
    }

    pub fn cancel(&mut self) {
        self.generation += 1;
        self.recorder = None;
        self.pending = None;
        self.target = None;
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.join_job();
        if let Some(overlay) = &self.overlay {
            overlay.hide();
        }
        self.idle_status();
    }

    pub fn copy_last(&self) -> Result<()> {
        ensure!(!self.last_text.is_empty(), "No dictation to copy yet");
        input::copy_text(&self.last_text)
    }

    pub fn settings_window(&self) -> Result<()> {
        let script = self.dir.join("dictation-settings.ps1");
        fs::write(
            &script,
            include_str!("../../../windows/dictation-settings.ps1"),
        )?;
        use std::os::windows::process::CommandExt;
        Command::new("powershell.exe")
            .args(["-NoProfile", "-STA", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script)
            .env("ASP_DATA_DIR", &self.dir)
            .env("ASP_BINARY", std::env::current_exe()?)
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .spawn()?;
        Ok(())
    }

    pub fn open_history(&self) -> Result<()> {
        let path = self.dir.join("dictation-history.txt");
        if !path.exists() {
            fs::write(&path, "")?;
        }
        Command::new("notepad.exe").arg(path).spawn()?;
        Ok(())
    }

    fn save_history(&self, text: &str) -> Result<()> {
        let path = self.dir.join("dictation-history.txt");
        let history = fs::read_to_string(&path).unwrap_or_default();
        let mut lines = history.lines().rev().take(99).collect::<Vec<_>>();
        lines.reverse();
        lines.push(text);
        fs::write(path, lines.join("\n"))?;
        Ok(())
    }

    pub fn tick(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Toggle => {
                    if let Err(error) = self.toggle() {
                        self.display(error.to_string(), true);
                    }
                }
                Event::Progress(id, bytes, total) if id == self.generation => self.display(
                    format!(
                        "Downloading model · {}% · {} / {} MB",
                        bytes * 100 / total.max(1),
                        bytes / 1_000_000,
                        total / 1_000_000
                    ),
                    false,
                ),
                Event::SetupDone(id, result) if id == self.generation => {
                    self.join_job();
                    self.display(
                        match result {
                            Ok(()) => format!("Dictation ready · {}", self.settings.hotkey),
                            Err(error) => error,
                        },
                        true,
                    );
                }
                Event::Transcript(id, result) if id == self.generation => {
                    self.join_job();
                    match result {
                        Ok(text) => {
                            self.last_text = text.clone();
                            let _ = self.save_history(&text);
                            if let Some(target) = self.target.take() {
                                self.pending = Some((target, text, Instant::now()));
                            }
                        }
                        Err(error) => self.display(error, true),
                    }
                }
                _ => {}
            }
        }
        if let Some(recorder) = &self.recorder {
            if let Some(error) = recorder.error() {
                self.recorder = None;
                self.display(error, true);
            } else if self.started.elapsed().as_secs() >= audio_samples::MAX_RECORDING_SECS as u64 {
                if let Err(error) = self.finish_recording() {
                    self.display(error.to_string(), true);
                }
            }
        }
        if let Some((target, text, since)) = &self.pending {
            if !input::modifiers_down() || since.elapsed() > Duration::from_secs(5) {
                let result = target.insert(text);
                self.pending = None;
                self.display(
                    match result {
                        Ok(()) => "Text inserted".into(),
                        Err(error) => error.to_string(),
                    },
                    true,
                );
            }
        }
        if self.hide_at.is_some_and(|time| Instant::now() >= time) {
            if let Some(overlay) = &self.overlay {
                overlay.hide();
            }
            self.hide_at = None;
        }
        if self.next_settings <= Instant::now() && !self.busy() && self.job.is_none() {
            self.next_settings = Instant::now() + Duration::from_secs(1);
            if let Ok(settings) = Settings::load(&self.dir) {
                if settings != self.settings {
                    let reregister = settings.hotkey != self.settings.hotkey
                        || settings.enabled != self.settings.enabled;
                    self.settings = settings;
                    if reregister {
                        self.shortcut = None;
                        if let Err(error) = self.register_shortcut() {
                            self.display(error.to_string(), true);
                            return;
                        }
                    }
                    self.idle_status();
                }
            }
        }
    }
}

impl Drop for Dictation {
    fn drop(&mut self) {
        self.cancel();
    }
}
