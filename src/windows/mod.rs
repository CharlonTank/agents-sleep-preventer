mod dictation;
mod hooks;
mod speech_config;
mod speech_engine;
mod state;

use anyhow::{bail, ensure, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use fs2::FileExt;
use serde_json::json;
use state::{agent_kind, Mode, State};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, System};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    Icon, TrayIconBuilder,
};
use windows_sys::Win32::System::{
    Console::FreeConsole,
    Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED},
    Threading::CREATE_NO_WINDOW,
};

#[derive(Parser)]
#[command(
    name = "asp",
    version,
    about = "Keep Windows awake while coding agents are working"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    #[arg(long, global = true, hide = true)]
    asp_hook: bool,
    /// Store runtime state in a separate directory (useful for isolated tests)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Local dictation setup, settings, and microphone diagnostics
    Dictation {
        #[command(subcommand)]
        command: dictation::DictationCommand,
    },
    /// Launch the system tray app
    #[command(alias = "menubar")]
    Tray,
    /// Monitor agent activity without a tray icon
    #[command(alias = "agent")]
    Daemon {
        #[arg(short, long, default_value = "1", value_parser = clap::value_parser!(u64).range(1..=60))]
        interval: u64,
    },
    /// Register a working agent (normally invoked by a hook)
    Start {
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Refresh a working agent
    Refresh {
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Unregister a finished agent
    Stop {
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Mark an agent as waiting for input
    Attention {
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Show sleep prevention and monitor status
    Status,
    /// List registered agent sessions as JSON
    List,
    /// Remove exited or stale sessions
    Cleanup,
    /// Override automatic sleep prevention
    Force {
        #[arg(value_enum)]
        mode: Option<ForceMode>,
    },
    /// Clear all sessions and restore automatic sleep
    Reset,
    /// Stop the tray/daemon and release this app's sleep request
    Quit,
    /// Install for the current user and configure Claude Code/Codex hooks
    Install {
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove this app's hooks and login startup entry
    Uninstall,
}

#[derive(Clone, Copy, ValueEnum)]
enum ForceMode {
    Auto,
    Awake,
    Sleep,
}

struct Store {
    dir: PathBuf,
}

impl Store {
    fn new(dir: Option<PathBuf>) -> Result<Self> {
        let dir = match dir {
            Some(dir) => dir,
            None => dirs::data_local_dir()
                .context("Could not find LOCALAPPDATA")?
                .join("AgentsSleepPreventer"),
        };
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn lock_file(&self, name: &str) -> Result<File> {
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join(name))?)
    }

    fn update(&self, change: impl FnOnce(&mut State)) -> Result<State> {
        let lock = self.lock_file("state.lock")?;
        FileExt::lock_exclusive(&lock)?;
        let path = self.dir.join("state.json");
        let mut state: State = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context(
                "Invalid state.json; restore its backup or remove it while ASP is stopped",
            )?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(error.into()),
        };
        let before = state.clone();
        change(&mut state);
        if state != before || !path.exists() {
            let mut temp = tempfile::NamedTempFile::new_in(&self.dir)?;
            serde_json::to_writer_pretty(&mut temp, &state)?;
            temp.as_file().sync_all()?;
            temp.persist(&path)
                .context("Could not save sleep prevention state")?;
        }
        // Closing the file releases the lock on every return path.
        Ok(state)
    }

    fn monitor_lock(&self) -> Result<Option<File>> {
        let lock = self.lock_file("monitor.lock")?;
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => Ok(Some(lock)),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn is_running(&self) -> Result<bool> {
        Ok(self.monitor_lock()?.is_none())
    }

    fn ensure_monitor(&self) -> Result<()> {
        if !self.is_running()? {
            Command::new(std::env::current_exe()?)
                .arg("--data-dir")
                .arg(&self.dir)
                .arg("tray")
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("Could not launch the Windows tray app")?;
            let deadline = Instant::now() + Duration::from_secs(3);
            while !self.is_running()? {
                ensure!(
                    Instant::now() < deadline,
                    "ASP monitor failed to start; run asp tray for details"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(())
    }
}

// Owned by the monitor thread for its entire lifetime. Short-lived hooks
// cannot hold a Windows execution-state request after they exit.
struct SleepRequest {
    active: bool,
}

impl SleepRequest {
    fn set(&mut self, active: bool) -> Result<()> {
        if self.active != active {
            let flags = ES_CONTINUOUS | if active { ES_SYSTEM_REQUIRED } else { 0 };
            ensure!(
                unsafe { SetThreadExecutionState(flags) } != 0,
                "Windows rejected the sleep prevention request"
            );
            self.active = active;
        }
        Ok(())
    }
}

impl Drop for SleepRequest {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn process_snapshot(refresh: sysinfo::ProcessRefreshKind) -> System {
    // No disk, sensor, or network enumeration is needed for PID liveness.
    System::new_with_specifics(sysinfo::RefreshKind::new().with_processes(refresh))
}

fn prune(store: &Store) -> Result<State> {
    let system = process_snapshot(sysinfo::ProcessRefreshKind::new());
    store.update(|state| {
        state.prune(now(), |pid| {
            system.process(Pid::from_u32(pid)).map(|p| p.start_time())
        })
    })
}

fn agent_pid(system: &System, explicit: Option<u32>) -> Result<u32> {
    if let Some(pid) = explicit {
        ensure!(
            pid != 0 && pid != std::process::id(),
            "Choose a live agent PID"
        );
        return Ok(pid);
    }
    let mut current = Pid::from_u32(std::process::id());
    for _ in 0..64 {
        let Some(process) = system.process(current) else {
            break;
        };
        if current.as_u32() != std::process::id() {
            let args = process
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            if agent_kind(&process.name().to_string_lossy(), &args).is_some() {
                return Ok(current.as_u32());
            }
        }
        match process.parent() {
            Some(parent) if parent != current && parent.as_u32() != 0 => current = parent,
            _ => break,
        }
    }
    bail!("No native agent ancestor found; use --pid <agent PID> or asp force awake. WSL hooks need a Windows bridge.")
}

fn handle_hook(store: &Store, action: &str, pid: Option<u32>) -> Result<()> {
    let system = process_snapshot(
        sysinfo::ProcessRefreshKind::new().with_cmd(sysinfo::UpdateKind::OnlyIfNotSet),
    );
    let pid = agent_pid(&system, pid)?;
    let process = system.process(Pid::from_u32(pid));
    if action == "stop" {
        store.update(|state| {
            state.sessions.remove(&pid);
        })?;
        return Ok(());
    }
    let process = process.context("The selected agent process is no longer running")?;
    store.update(|state| {
        state.start(pid, process.start_time(), now());
        if action == "attention" {
            if let Some(session) = state.sessions.get_mut(&pid) {
                session.attention = true;
            }
        }
    })?;
    store.ensure_monitor()
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let store = Store::new(cli.data_dir)?;
    let result = match cli.command.unwrap_or(Commands::Tray) {
        Commands::Dictation { command } => dictation::command(&store.dir, command),
        Commands::Start { pid } | Commands::Refresh { pid } => handle_hook(&store, "start", pid),
        Commands::Stop { pid } => handle_hook(&store, "stop", pid),
        Commands::Attention { pid } => handle_hook(&store, "attention", pid),
        Commands::Tray => run_monitor(store, true, 1),
        Commands::Daemon { interval } => run_monitor(store, false, interval),
        Commands::Status => {
            let state = prune(&store)?;
            let running = store.is_running()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "platform": "windows", "monitor_running": running,
                    "sleep_prevention_requested": state.should_prevent_sleep(),
                    "working_agents": state.working_count(), "mode": state.mode,
                    "data_dir": store.dir,
                }))?
            );
            Ok(())
        }
        Commands::List => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &prune(&store)?.sessions.values().collect::<Vec<_>>()
                )?
            );
            Ok(())
        }
        Commands::Cleanup => prune(&store).map(|_| ()),
        Commands::Force { mode: Some(mode) } => {
            store.update(|state| {
                state.shutdown = false;
                state.mode = match mode {
                    ForceMode::Auto => Mode::Auto,
                    ForceMode::Awake => Mode::Awake,
                    ForceMode::Sleep => Mode::Sleep,
                };
            })?;
            store.ensure_monitor()
        }
        Commands::Force { mode: None } => {
            println!("{}", serde_json::to_string(&store.update(|_| {})?.mode)?);
            Ok(())
        }
        Commands::Reset => store
            .update(|state| {
                state.sessions.clear();
                state.mode = Mode::Auto;
            })
            .map(|_| ()),
        Commands::Quit => shutdown(&store),
        Commands::Install { yes } => install(&store, yes),
        Commands::Uninstall => uninstall(&store),
    };
    if cli.asp_hook {
        // A sleep utility must never block the agent's work. Persist hook
        // failures for diagnosis instead of emitting protocol output on stdout.
        if let Err(error) = result {
            if let Some(dir) = dirs::data_local_dir() {
                let log = dir.join("AgentsSleepPreventer/hook-errors.log");
                if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log) {
                    let _ = writeln!(file, "{}: {error:#}", now());
                }
            }
        }
        Ok(())
    } else {
        result
    }
}

fn run_monitor(store: Store, tray: bool, interval: u64) -> Result<()> {
    let Some(lock) = store.monitor_lock()? else {
        return Ok(());
    };
    store.update(|state| state.shutdown = false)?;
    let mut request = SleepRequest { active: false };
    request.set(prune(&store)?.should_prevent_sleep())?;
    if !tray {
        loop {
            let state = prune(&store)?;
            request.set(state.should_prevent_sleep())?;
            if state.shutdown {
                break;
            }
            std::thread::sleep(Duration::from_secs(interval));
        }
        return Ok(());
    }

    let event_loop = EventLoopBuilder::new().build();
    let mut dictation = dictation::Dictation::new(&store.dir);
    let mut agent_awake = request.active;
    let mut allow_dictation_awake = true;
    let menu = Menu::new();
    let status = MenuItem::new("Agents Sleep Preventer", false, None);
    let notice = MenuItem::new("Claude Code / Codex", false, None);
    let auto = MenuItem::new("Automatic", true, None);
    let awake = MenuItem::new("Keep awake", true, None);
    let sleep = MenuItem::new("Allow sleep", true, None);
    let reset = MenuItem::new("Clear sessions", true, None);
    let setup = MenuItem::new("Install agent hooks and start at login", true, None);
    let dictation_status = MenuItem::new(dictation.status(), false, None);
    let dictation_settings = MenuItem::new("Dictation Settings…", true, None);
    let dictation_setup = MenuItem::new("Download Dictation Model…", true, None);
    let dictation_cancel = MenuItem::new("Cancel Dictation", true, None);
    let dictation_copy = MenuItem::new("Copy Last Dictation", true, None);
    let dictation_history = MenuItem::new("Dictation History…", true, None);
    let microphone_settings = MenuItem::new("Microphone Permissions…", true, None);
    let updates = MenuItem::new("Check for Updates…", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append_items(&[
        &status,
        &notice,
        &auto,
        &awake,
        &sleep,
        &reset,
        &setup,
        &dictation_status,
        &dictation_settings,
        &dictation_setup,
        &dictation_cancel,
        &dictation_copy,
        &dictation_history,
        &microphone_settings,
        &updates,
        &quit,
    ])?;
    let icon = tray_icon(request.active)?;
    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Agents Sleep Preventer")
        .with_icon(icon)
        .build()?;
    // Detach only after startup succeeds, so CLI startup errors stay visible.
    unsafe {
        FreeConsole();
    }
    let mut icon_active = request.active;
    let mut next_refresh = Instant::now();
    event_loop.run(move |event, _, control_flow| {
        // Capturing the handle retains the single-monitor lock until exit.
        let monitor_guard = &lock;
        if matches!(event, Event::LoopDestroyed) {
            dictation.cancel();
            let _ = request.set(false);
            let _ = FileExt::unlock(monitor_guard);
            return;
        }
        dictation.tick();
        dictation_status.set_text(dictation.status());
        if let Err(error) = request.set(agent_awake || (allow_dictation_awake && dictation.busy()))
        {
            notice.set_text(error.to_string());
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let result = if event.id == dictation_settings.id() {
                dictation.settings_window()
            } else if event.id == dictation_setup.id() {
                dictation.setup()
            } else if event.id == dictation_cancel.id() {
                dictation.cancel();
                Ok(())
            } else if event.id == dictation_copy.id() {
                dictation.copy_last()
            } else if event.id == dictation_history.id() {
                dictation.open_history()
            } else if event.id == microphone_settings.id() {
                Command::new("explorer.exe")
                    .arg("ms-settings:privacy-microphone")
                    .spawn()
                    .map(|_| ())
                    .map_err(Into::into)
            } else if event.id == updates.id() {
                Command::new("explorer.exe")
                    .arg("https://github.com/CharlonTank/agents-sleep-preventer/releases/latest")
                    .spawn()
                    .map(|_| ())
                    .map_err(Into::into)
            } else if event.id == quit.id() {
                store
                    .update(|state| {
                        state.sessions.clear();
                        state.mode = Mode::Auto;
                        state.shutdown = true;
                    })
                    .map(|_| ())
            } else if event.id == setup.id() {
                // Installation copies a running binary into a stable location.
                // Its new tray process is started on the next user launch/login.
                install_files().map(|_| notice.set_text("Installed. Restart Claude Code / Codex."))
            } else {
                store
                    .update(|state| {
                        if event.id == auto.id() {
                            state.mode = Mode::Auto;
                        }
                        if event.id == awake.id() {
                            state.mode = Mode::Awake;
                        }
                        if event.id == sleep.id() {
                            state.mode = Mode::Sleep;
                        }
                        if event.id == reset.id() {
                            state.sessions.clear();
                            state.mode = Mode::Auto;
                        }
                    })
                    .map(|_| ())
            };
            if let Err(error) = result {
                notice.set_text(format!("Error: {error}"));
            }
            next_refresh = Instant::now();
        }
        if Instant::now() >= next_refresh {
            match prune(&store).and_then(|state| {
                agent_awake = state.should_prevent_sleep();
                allow_dictation_awake = state.mode != Mode::Sleep && !state.shutdown;
                request.set(agent_awake || (allow_dictation_awake && dictation.busy()))?;
                Ok(state)
            }) {
                Ok(state) => {
                    if state.shutdown {
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    let text = format!(
                        "{} · {} working · {:?}",
                        if request.active {
                            "Keeping awake"
                        } else {
                            "Sleep allowed"
                        },
                        state.working_count(),
                        state.mode
                    );
                    status.set_text(&text);
                    let _ = tray_icon.set_tooltip(Some(&text));
                    if request.active != icon_active {
                        if let Ok(icon) = self::tray_icon(request.active) {
                            let _ = tray_icon.set_icon(Some(icon));
                        }
                        icon_active = request.active;
                    }
                }
                Err(error) => {
                    // Release our request if monitoring fails instead of
                    // leaving the machine awake with stale state indefinitely.
                    agent_awake = false;
                    let _ = request.set(allow_dictation_awake && dictation.busy());
                    status.set_text(format!("Error: {error}"));
                }
            }
            next_refresh = Instant::now() + Duration::from_millis(500);
        }
        *control_flow =
            ControlFlow::WaitUntil(next_refresh.min(Instant::now() + Duration::from_millis(50)));
    });
}

fn tray_icon(active: bool) -> Result<Icon> {
    let mut rgba = vec![0u8; 32 * 32 * 4];
    for y in 0..32i32 {
        for x in 0..32i32 {
            if (x - 16).pow(2) + (y - 16).pow(2) < 13 * 13 {
                let offset = ((y * 32 + x) * 4) as usize;
                let color = if active {
                    [54, 190, 115, 255]
                } else {
                    [110, 130, 155, 255]
                };
                rgba[offset..offset + 4].copy_from_slice(&color);
            }
        }
    }
    Ok(Icon::from_rgba(rgba, 32, 32)?)
}

fn shutdown(store: &Store) -> Result<()> {
    store.update(|state| {
        state.sessions.clear();
        state.mode = Mode::Auto;
        state.shutdown = true;
    })?;
    let deadline = Instant::now() + Duration::from_secs(65);
    while store.is_running()? {
        ensure!(
            Instant::now() < deadline,
            "Monitor did not stop; close it before uninstalling"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn installed_binary() -> Result<PathBuf> {
    Ok(dirs::data_local_dir()
        .context("Could not find LOCALAPPDATA")?
        .join("Programs/AgentsSleepPreventer/asp.exe"))
}

fn run_powershell(script: &str, vars: &[(&str, &Path)]) -> Result<()> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .envs(vars.iter().map(|(name, path)| (name, path.as_os_str())))
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn install_files() -> Result<PathBuf> {
    let binary = installed_binary()?;
    let source = std::env::current_exe()?;
    fs::create_dir_all(binary.parent().unwrap())?;
    if fs::canonicalize(&source)? != fs::canonicalize(&binary).unwrap_or_default() {
        fs::copy(&source, &binary)
            .context("Could not install asp.exe; quit the installed app before updating")?;
    }
    let speech_source = source
        .parent()
        .context("Missing executable directory")?
        .join("speech");
    let speech_destination = binary.parent().unwrap().join("speech");
    ensure!(
        speech_source.join("whisper-cli.exe").is_file()
            && speech_source.join("parakeet-cli.exe").is_file(),
        "Extract the complete Windows ZIP, including the speech folder, before installing"
    );
    if fs::canonicalize(&speech_source)?
        != fs::canonicalize(&speech_destination).unwrap_or_default()
    {
        fs::create_dir_all(&speech_destination)?;
        for entry in fs::read_dir(&speech_source)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::copy(entry.path(), speech_destination.join(entry.file_name()))?;
            }
        }
    }
    let home = dirs::home_dir().context("Could not find user profile")?;
    hooks::install(&home, &binary)?;
    run_powershell(
        include_str!("../../windows/shortcuts.ps1"),
        &[("ASP_INSTALL_BINARY", &binary)],
    )?;
    Ok(binary)
}

fn install(store: &Store, yes: bool) -> Result<()> {
    if !yes {
        println!(
            "Install to {}, configure Claude Code/Codex hooks, and start at login? [y/N]",
            installed_binary()?.display()
        );
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Ok(());
        }
    }
    shutdown(store)?;
    let binary = install_files()?;
    store.update(|state| state.shutdown = false)?;
    Command::new(&binary)
        .arg("tray")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    println!(
        "Installed {}. Restart Claude Code and Codex to load the hooks.",
        binary.display()
    );
    Ok(())
}

fn uninstall(store: &Store) -> Result<()> {
    shutdown(store)?;
    hooks::uninstall(&dirs::home_dir().context("Could not find user profile")?)?;
    run_powershell(
        "$ErrorActionPreference = 'Stop'; foreach ($folder in @([Environment]::GetFolderPath('Startup'), [Environment]::GetFolderPath('Programs'))) { $link = Join-Path $folder 'Agents Sleep Preventer.lnk'; if (Test-Path -LiteralPath $link) { Remove-Item -LiteralPath $link } }",
        &[],
    )?;
    println!(
        "Removed agent hooks and login startup. After this command exits, you can delete {}.",
        installed_binary()?.parent().unwrap().display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_lock_is_exclusive_and_released_on_exit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(Some(dir.path().into())).unwrap();
        let guard = store.monitor_lock().unwrap().unwrap();
        assert!(store.is_running().unwrap());
        assert!(store.monitor_lock().unwrap().is_none());
        drop(guard);
        assert!(!store.is_running().unwrap());
    }

    #[test]
    fn concurrent_hooks_do_not_lose_sessions_or_corrupt_state() {
        let dir = tempfile::tempdir().unwrap();
        let threads = (1..=16)
            .map(|pid| {
                let path = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    Store::new(Some(path))
                        .unwrap()
                        .update(|state| state.start(pid, 10, 20))
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        let store = Store::new(Some(dir.path().into())).unwrap();
        assert_eq!(store.update(|_| {}).unwrap().working_count(), 16);
    }
}
