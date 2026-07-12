mod authorization;
mod dictation;
mod logging;
mod native_dialogs;
mod notifications;
mod objc_utils;
mod popover;
mod settings;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use core_foundation::base::{kCFAllocatorDefault, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::runloop::{
    kCFRunLoopDefaultMode, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRun,
};
use core_foundation::string::CFString;
use dictation::{run_onboarding_if_needed, DictationManager};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use io_kit_sys::types::*;
use io_kit_sys::*;
use mach2::port::MACH_PORT_NULL;
use objc::{class, msg_send, sel, sel_impl};
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use sysinfo::System;

#[link(name = "IOKit", kind = "framework")]
extern "C" {}
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};

static LID_JUST_CLOSED: AtomicBool = AtomicBool::new(false);
static LID_WAS_CLOSED: AtomicBool = AtomicBool::new(false);
static CURRENT_PID_INDEX: AtomicUsize = AtomicUsize::new(0);
static CURRENT_INACTIVE_INDEX: AtomicUsize = AtomicUsize::new(0);
static MANUAL_SLEEP_PREVENTION: AtomicBool = AtomicBool::new(true);

const PIDS_DIR: &str = "/tmp/agents_working_pids";
const LEGACY_PIDS_DIR: &str = "/tmp/claude_working_pids";
/// One marker file per agent PID currently waiting for user input.
const ATTENTION_DIR: &str = "/tmp/asp_attention";
/// Give Claude Code time to update its session registry after firing a hook.
const ATTENTION_RECONCILE_GRACE_SECS: u64 = 10;
/// Fallback for agents without an authoritative session status.
const ATTENTION_FALLBACK_EXPIRY_SECS: u64 = 6 * 60 * 60;
const IDLE_TIMEOUT_SECS: u64 = 30;
const IDLE_CPU_THRESHOLD: f32 = 0.5;
const APP_BUNDLE_PATH: &str = "/Applications/AgentsSleepPreventer.app";
const APP_BINARY_PATH: &str = "/Applications/AgentsSleepPreventer.app/Contents/MacOS/asp";
const OWNED_HOOK_MARKERS: [&str; 4] = [
    "AgentsSleepPreventer.app/Contents/MacOS/asp",
    "/usr/local/bin/asp",
    "/usr/local/bin/agents-sleep-preventer",
    "claude-sleep-preventer",
];

#[derive(Parser)]
#[command(name = "asp")]
#[command(about = "Keep your Mac awake while coding agents are working")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Register current agent process and disable sleep
    Start {
        /// Explicit agent PID for managed heartbeat hooks
        #[arg(long, hide = true)]
        pid: Option<u32>,
    },
    /// Unregister current agent process and re-enable sleep if no others
    Stop {
        /// Explicit agent PID for managed heartbeat hooks
        #[arg(long, hide = true)]
        pid: Option<u32>,
    },
    /// Show current status
    Status,
    /// List active/inactive instances as JSON
    List,
    /// Focus an agent instance by PID
    Focus { pid: u32 },
    /// Clean up stale PIDs (interrupted sessions)
    Cleanup,
    /// Run as daemon with cleanup + thermal monitoring
    Daemon {
        #[arg(short, long, default_value = "1")]
        interval: u64,
    },
    /// Agent needs attention (hook): reads the hook JSON on stdin
    #[command(hide = true)]
    Attention,
    /// Run background agent (dictation + permissions, no UI)
    Agent,
    /// Run native menu bar app
    Menubar,
    /// Force reset: clear all PIDs and re-enable sleep
    Reset,
    /// Check thermal state
    Thermal,
    /// Install hooks and configure supported coding agents
    Install {
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Uninstall hooks and restore defaults
    Uninstall {
        /// Keep Whisper model data (~1.5 GB)
        #[arg(short = 'k', long)]
        keep_model: bool,
        /// Keep coding agent hooks
        #[arg(long)]
        keep_hooks: bool,
        /// Keep app data and logs
        #[arg(long)]
        keep_data: bool,
    },
    /// Open the settings window
    Settings,
    /// Debug: list process names
    Debug,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Menubar) {
        Commands::Start { pid } => cmd_start(pid)?,
        Commands::Stop { pid } => cmd_stop(pid)?,
        Commands::Status => cmd_status()?,
        Commands::List => cmd_list()?,
        Commands::Focus { pid } => cmd_focus(pid)?,
        Commands::Cleanup => cmd_cleanup()?,
        Commands::Daemon { interval } => cmd_daemon(interval)?,
        Commands::Attention => cmd_attention()?,
        Commands::Agent => cmd_agent()?,
        Commands::Menubar => cmd_menubar()?,
        Commands::Reset => cmd_reset()?,
        Commands::Thermal => cmd_thermal()?,
        Commands::Install { yes } => cmd_install(yes)?,
        Commands::Uninstall {
            keep_model,
            keep_hooks,
            keep_data,
        } => cmd_uninstall(keep_model, keep_hooks, keep_data)?,
        Commands::Settings => cmd_settings()?,
        Commands::Debug => cmd_debug()?,
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentKind {
    Claude,
    Codex,
    Hermes,
}

#[derive(Debug, Clone)]
struct ProcessInfo {
    pid: u32,
    ppid: u32,
    comm: String,
    args: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum AgentState {
    Attention,
    Working,
    Idle,
}

impl AgentState {
    fn priority(self) -> u8 {
        match self {
            AgentState::Attention => 0,
            AgentState::Working => 1,
            AgentState::Idle => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct AgentListItem {
    pid: u32,
    kind: String,
    project: String,
    branch: String,
    cwd: String,
    state: AgentState,
    #[serde(skip_serializing_if = "Option::is_none")]
    age_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProjectInfo {
    project: String,
    branch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeAttentionStatus {
    Waiting,
    NotWaiting,
}

fn load_process_table() -> Vec<ProcessInfo> {
    Command::new("ps")
        .args(["-eo", "pid=,ppid=,comm=,args="])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.lines().filter_map(parse_process_line).collect())
        .unwrap_or_default()
}

fn parse_process_line(line: &str) -> Option<ProcessInfo> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse().ok()?;
    let ppid = parts.next()?.parse().ok()?;
    let comm = parts.next()?.to_string();
    let args = parts.collect::<Vec<_>>().join(" ");
    Some(ProcessInfo {
        pid,
        ppid,
        comm,
        args,
    })
}

fn executable_basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn executable_name_is(token: &str, expected: &str) -> bool {
    let basename = executable_basename(token);
    basename == expected || basename == format!("{}.exe", expected)
}

fn process_tokens(process: &ProcessInfo) -> Vec<&str> {
    process.args.split_whitespace().collect()
}

fn codex_command_index(tokens: &[&str]) -> Option<usize> {
    tokens
        .iter()
        .position(|token| executable_name_is(token, "codex"))
}

fn is_codex_app_server(tokens: &[&str]) -> bool {
    codex_command_index(tokens)
        .and_then(|idx| tokens.get(idx + 1))
        .map(|arg| *arg == "app-server")
        .unwrap_or(false)
}

fn is_codex_wrapper_process(process: &ProcessInfo) -> bool {
    let tokens = process_tokens(process);
    tokens
        .first()
        .map(|arg0| executable_name_is(arg0, "node"))
        .unwrap_or(false)
        && tokens
            .get(1)
            .map(|arg1| executable_name_is(arg1, "codex"))
            .unwrap_or(false)
        && !is_codex_app_server(&tokens)
}

fn is_codex_native_process(process: &ProcessInfo) -> bool {
    let tokens = process_tokens(process);
    let arg0 = tokens.first().copied().unwrap_or(&process.comm);
    (executable_name_is(arg0, "codex") || executable_name_is(&process.comm, "codex"))
        && !is_codex_app_server(&tokens)
}

fn is_python_executable(token: &str) -> bool {
    let basename = executable_basename(token);
    basename == "python" || basename == "python3" || basename == "Python"
}

fn is_hermes_python_module(tokens: &[&str]) -> bool {
    tokens.windows(2).any(|window| {
        window[0] == "-m" && (window[1] == "hermes_cli.main" || window[1] == "hermes")
    })
}

fn is_hermes_process(process: &ProcessInfo) -> bool {
    let tokens = process_tokens(process);
    let arg0 = tokens.first().copied().unwrap_or(&process.comm);

    executable_name_is(arg0, "hermes")
        || executable_name_is(&process.comm, "hermes")
        || ((is_python_executable(arg0) || is_python_executable(&process.comm))
            && is_hermes_python_module(&tokens))
}

fn classify_agent_process(process: &ProcessInfo) -> Option<AgentKind> {
    let tokens = process_tokens(process);
    let arg0 = tokens.first().copied().unwrap_or(&process.comm);

    if executable_name_is(arg0, "claude") || executable_name_is(&process.comm, "claude") {
        return Some(AgentKind::Claude);
    }

    if is_codex_native_process(process) || is_codex_wrapper_process(process) {
        return Some(AgentKind::Codex);
    }

    if is_hermes_process(process) {
        return Some(AgentKind::Hermes);
    }

    None
}

fn hermes_profile_from_args(args: &str) -> Option<String> {
    let mut tokens = args.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "--profile" {
            return tokens
                .next()
                .filter(|profile| !profile.is_empty())
                .map(ToOwned::to_owned);
        }
        if let Some(profile) = token.strip_prefix("--profile=") {
            if !profile.is_empty() {
                return Some(profile.to_string());
            }
        }
    }
    None
}

fn is_hermes_gateway_process(process: &ProcessInfo) -> bool {
    let tokens = process_tokens(process);
    tokens
        .windows(2)
        .any(|window| window[0] == "gateway" && window[1] == "run")
}

fn hermes_project_name(process: &ProcessInfo) -> Option<String> {
    if classify_agent_process(process) != Some(AgentKind::Hermes) {
        return None;
    }

    hermes_profile_from_args(&process.args)
        .or_else(|| is_hermes_gateway_process(process).then(|| "default".to_string()))
}

fn select_agent_processes(processes: &[ProcessInfo]) -> Vec<ProcessInfo> {
    let codex_native_parents: HashSet<u32> = processes
        .iter()
        .filter(|process| is_codex_native_process(process))
        .map(|process| process.ppid)
        .collect();
    let mut seen_pids = HashSet::new();
    let mut agents = processes
        .iter()
        .filter(|process| match classify_agent_process(process) {
            Some(AgentKind::Claude) => true,
            Some(AgentKind::Codex) => {
                !(is_codex_wrapper_process(process) && codex_native_parents.contains(&process.pid))
            }
            Some(AgentKind::Hermes) => true,
            None => false,
        })
        .filter(|process| seen_pids.insert(process.pid))
        .cloned()
        .collect::<Vec<_>>();
    agents.sort_by_key(|process| process.pid);
    agents
}

fn find_agent_ancestor() -> Option<u32> {
    let processes = load_process_table();
    let by_pid: HashMap<u32, ProcessInfo> = processes
        .into_iter()
        .map(|process| (process.pid, process))
        .collect();
    let this_pid = std::process::id();
    let mut current_pid = this_pid;

    for _ in 0..20 {
        let Some(process) = by_pid.get(&current_pid) else {
            break;
        };

        if current_pid != this_pid && classify_agent_process(process).is_some() {
            return Some(current_pid);
        }

        if process.ppid == 0 || process.ppid == current_pid {
            break;
        }
        current_pid = process.ppid;
    }

    Some(std::os::unix::process::parent_id())
}

fn ensure_pids_dir() -> Result<()> {
    fs::create_dir_all(PIDS_DIR).context("Failed to create PIDs directory")?;
    Ok(())
}

fn set_attention_marker(pid: u32) {
    if fs::create_dir_all(ATTENTION_DIR).is_ok() {
        let _ = fs::write(PathBuf::from(ATTENTION_DIR).join(pid.to_string()), "");
    }
}

fn clear_attention_marker(pid: u32) {
    let _ = fs::remove_file(PathBuf::from(ATTENTION_DIR).join(pid.to_string()));
}

fn parse_claude_attention_status(content: &str) -> Option<ClaudeAttentionStatus> {
    let status = serde_json::from_str::<serde_json::Value>(content)
        .ok()?
        .get("status")?
        .as_str()?
        .trim()
        .to_ascii_lowercase();

    match status.as_str() {
        "waiting" | "attention" | "needs_attention" | "needs-attention" => {
            Some(ClaudeAttentionStatus::Waiting)
        }
        "busy" | "idle" => Some(ClaudeAttentionStatus::NotWaiting),
        _ => None,
    }
}

fn claude_attention_status(home: &Path, pid: u32) -> Option<ClaudeAttentionStatus> {
    let session_path = home.join(format!(".claude/sessions/{pid}.json"));
    fs::read_to_string(session_path)
        .ok()
        .and_then(|content| parse_claude_attention_status(&content))
}

fn should_keep_attention_marker(
    marker_age_secs: u64,
    claude_status: Option<ClaudeAttentionStatus>,
) -> bool {
    if marker_age_secs <= ATTENTION_RECONCILE_GRACE_SECS {
        return true;
    }

    match claude_status {
        Some(ClaudeAttentionStatus::Waiting) => true,
        Some(ClaudeAttentionStatus::NotWaiting) => false,
        None => marker_age_secs < ATTENTION_FALLBACK_EXPIRY_SECS,
    }
}

/// PIDs of live agents currently waiting for user input. Claude Code's own
/// session status wins after a short hook/update grace period; other agents
/// retain fresh markers but cannot stay stuck indefinitely.
fn attention_pids(processes: &[ProcessInfo]) -> HashSet<u32> {
    let mut pids = HashSet::new();
    let process_by_pid = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect::<HashMap<_, _>>();
    let home = resolve_user_home().ok();

    if let Ok(entries) = fs::read_dir(ATTENTION_DIR) {
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if !is_process_alive(pid) {
                let _ = fs::remove_file(entry.path());
                continue;
            }

            let Some(process) = process_by_pid.get(&pid).copied() else {
                let _ = fs::remove_file(entry.path());
                continue;
            };
            let Some(kind) = classify_agent_process(process) else {
                let _ = fs::remove_file(entry.path());
                continue;
            };

            let marker_age = get_file_age(&entry.path()).unwrap_or(ATTENTION_FALLBACK_EXPIRY_SECS);
            let status = (kind == AgentKind::Claude)
                .then_some(())
                .and_then(|_| home.as_deref())
                .and_then(|home| claude_attention_status(home, pid));

            if should_keep_attention_marker(marker_age, status) {
                pids.insert(pid);
            } else {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    pids
}

fn get_pid_file(pid: u32) -> PathBuf {
    PathBuf::from(PIDS_DIR).join(pid.to_string())
}

fn working_pid_ages() -> HashMap<u32, u64> {
    let mut working = HashMap::new();
    if let Ok(entries) = fs::read_dir(PIDS_DIR) {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            working.insert(pid, get_file_age(&entry.path()).unwrap_or(0));
        }
    }
    working
}

fn count_active_pids() -> usize {
    fs::read_dir(PIDS_DIR)
        .map(|entries| entries.filter_map(|e| e.ok()).count())
        .unwrap_or(0)
}

fn set_sleep_disabled(disabled: bool) -> Result<()> {
    let value = if disabled { "1" } else { "0" };
    let output = Command::new("sudo")
        .args(["pmset", "-a", "disablesleep", value])
        .output()
        .context("Failed to run pmset")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        logging::log(&format!(
            "[pmset] Failed to set disablesleep={}: {}",
            value,
            stderr.trim()
        ));
    }
    Ok(())
}

fn sleep_prevention_enabled_from_settings() -> bool {
    settings::AppSettings::load().sleep_prevention.enabled
}

fn sync_sleep_state(source: &str, manual_enabled: bool) -> Result<()> {
    let active = count_active_pids();
    let sleep_disabled = is_sleep_disabled();
    let thermal_warning = check_thermal_warning();
    let should_prevent = manual_enabled && active > 0 && !thermal_warning;

    if should_prevent && !sleep_disabled {
        set_sleep_disabled(true)?;
        logging::log(&format!(
            "[{}] Sleep disabled (active PIDs: {})",
            source, active
        ));
    } else if !should_prevent && sleep_disabled {
        enable_sleep_and_trigger_if_lid_closed()?;
        logging::log(&format!("[{}] Sleep re-enabled", source));
    }

    Ok(())
}

fn menubar_sync_sleep() {
    let manual_enabled = MANUAL_SLEEP_PREVENTION.load(Ordering::SeqCst);
    let _ = sync_sleep_state("sync", manual_enabled);
}

fn cleanup_stale_pids() {
    let entries = match fs::read_dir(PIDS_DIR) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut removed = 0;
    let mut total = 0;

    for entry in entries.filter_map(|e| e.ok()) {
        let pid: u32 = match entry.file_name().to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        total += 1;
        let path = entry.path();

        if !is_process_alive(pid) {
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
            continue;
        }

        let age = get_file_age(&path).unwrap_or(0);
        if age >= IDLE_TIMEOUT_SECS {
            let cpu = get_process_cpu(pid);
            if cpu < IDLE_CPU_THRESHOLD {
                if fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }

    if removed > 0 {
        logging::log(&format!(
            "[cleanup] Removed {}/{} stale PIDs",
            removed, total
        ));
    }
}

fn is_sleep_disabled() -> bool {
    unsafe {
        let service_name = b"IOPMrootDomain\0";
        let matching = IOServiceMatching(service_name.as_ptr() as *const i8);
        if matching.is_null() {
            return false;
        }

        let root_domain = IOServiceGetMatchingService(kIOMasterPortDefault, matching);
        if root_domain == MACH_PORT_NULL {
            return false;
        }

        let key = CFString::new("SleepDisabled");
        let property = IORegistryEntryCreateCFProperty(
            root_domain,
            key.as_concrete_TypeRef(),
            kCFAllocatorDefault,
            0,
        );

        IOObjectRelease(root_domain);

        if property.is_null() {
            return false;
        }

        let result = CFBoolean::wrap_under_create_rule(property as _).into();
        result
    }
}

fn check_thermal_warning() -> bool {
    Command::new("pmset")
        .args(["-g", "therm"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| {
            (s.contains("CPU_Scheduler_Limit") && !s.contains("No CPU"))
                || (s.contains("thermal warning level") && !s.contains("No thermal warning"))
        })
        .unwrap_or(false)
}

fn get_process_cwd(pid: u32) -> Option<String> {
    Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with('n'))
                .map(|l| l[1..].to_string())
        })
}

fn parse_lsof_cwds(output: &str) -> HashMap<u32, String> {
    let mut cwds = HashMap::new();
    let mut current_pid = None;

    for line in output.lines() {
        if let Some(pid) = line
            .strip_prefix('p')
            .and_then(|value| value.parse::<u32>().ok())
        {
            current_pid = Some(pid);
        } else if let (Some(pid), Some(cwd)) = (current_pid, line.strip_prefix('n')) {
            if !cwd.is_empty() {
                cwds.insert(pid, cwd.to_string());
            }
        }
    }

    cwds
}

fn get_process_cwds(pids: &[u32]) -> HashMap<u32, String> {
    const LSOF_BATCH_SIZE: usize = 128;
    let mut cwds = HashMap::new();

    for batch in pids.chunks(LSOF_BATCH_SIZE) {
        if batch.is_empty() {
            continue;
        }
        let pid_list = batch
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let Some(stdout) = Command::new("lsof")
            .args(["-a", "-d", "cwd", "-p", &pid_list, "-Fpn"])
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
        else {
            continue;
        };
        cwds.extend(parse_lsof_cwds(&stdout));
    }

    cwds
}

fn path_display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn resolve_git_dir(repo_root: &Path) -> Option<PathBuf> {
    let marker = repo_root.join(".git");
    if marker.is_dir() {
        return Some(marker);
    }

    let content = fs::read_to_string(marker).ok()?;
    let git_dir = content.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = Path::new(git_dir);
    Some(if git_dir.is_absolute() {
        git_dir.to_path_buf()
    } else {
        repo_root.join(git_dir)
    })
}

fn branch_from_git_head(head: &str) -> String {
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .unwrap_or_default()
        .to_string()
}

fn project_info_from_cwd(cwd: &str) -> ProjectInfo {
    if cwd.is_empty() {
        return ProjectInfo::default();
    }

    let cwd_path = Path::new(cwd);
    let repo_root = cwd_path
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists());
    let project_path = repo_root.unwrap_or(cwd_path);
    let branch = repo_root
        .and_then(resolve_git_dir)
        .and_then(|git_dir| fs::read_to_string(git_dir.join("HEAD")).ok())
        .map(|head| branch_from_git_head(&head))
        .unwrap_or_default();

    ProjectInfo {
        project: path_display_name(project_path),
        branch,
    }
}

fn format_project_location(project: &str, branch: &str) -> String {
    if branch.is_empty() {
        project.to_string()
    } else {
        format!("{} git:({})", project, branch)
    }
}

fn get_git_branch(path: &str) -> Option<String> {
    Command::new("git")
        .args(["-C", path, "branch", "--show-current"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn format_process_location(pid: u32) -> String {
    get_process_cwd(pid)
        .map(|cwd| {
            let dir_name = std::path::Path::new(&cwd)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| cwd.clone());
            match get_git_branch(&cwd) {
                Some(branch) => format!("{} git:({})", dir_name, branch),
                None => dir_name,
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn cmd_start(pid: Option<u32>) -> Result<()> {
    logging::init_quiet();
    ensure_pids_dir()?;
    let agent_pid = pid
        .or_else(find_agent_ancestor)
        .unwrap_or_else(std::process::id);
    let pid_file = get_pid_file(agent_pid);

    fs::write(&pid_file, "working").context("Failed to write PID file")?;
    // The agent is working again, so it is no longer waiting on the user.
    clear_attention_marker(agent_pid);

    sync_sleep_state("hook-start", sleep_prevention_enabled_from_settings())
}

fn cmd_stop(pid: Option<u32>) -> Result<()> {
    logging::init_quiet();
    let agent_pid = pid
        .or_else(find_agent_ancestor)
        .unwrap_or_else(std::process::id);
    let pid_file = get_pid_file(agent_pid);

    notify_task_done_if_long(agent_pid, &pid_file);
    let _ = fs::remove_file(&pid_file);
    clear_attention_marker(agent_pid);

    sync_sleep_state("hook-stop", sleep_prevention_enabled_from_settings())
}

/// Notify when a finished task ran long enough that the user likely
/// walked away (short tasks mean they are still watching the terminal).
fn notify_task_done_if_long(agent_pid: u32, pid_file: &Path) {
    let Ok(created) = fs::metadata(pid_file).and_then(|m| m.created()) else {
        return;
    };
    let Ok(elapsed) = created.elapsed() else {
        return;
    };
    if elapsed.as_secs() < notifications::TASK_DONE_MIN_SECS {
        return;
    }

    let agent_name = load_process_table()
        .iter()
        .find(|p| p.pid == agent_pid)
        .and_then(classify_agent_process)
        .map(agent_kind_name)
        .unwrap_or("Agent");

    notifications::spool(
        &format!("{} finished", agent_name),
        &format!(
            "Task completed after {}.",
            notifications::format_duration(elapsed.as_secs())
        ),
        Some(agent_pid),
    );
}

fn agent_kind_name(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "Claude Code",
        AgentKind::Codex => "Codex",
        AgentKind::Hermes => "Hermes",
    }
}

/// Claude Code "Notification" hook: fired when the agent is waiting for
/// input or needs a permission. The hook JSON arrives on stdin.
fn cmd_attention() -> Result<()> {
    logging::init_quiet();

    let mut input = String::new();
    use std::io::Read as _;
    let _ = std::io::stdin().read_to_string(&mut input);

    let message = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "Waiting for your input.".to_string());

    let agent_pid = find_agent_ancestor();
    let agent_name = agent_pid
        .and_then(|pid| {
            load_process_table()
                .iter()
                .find(|p| p.pid == pid)
                .and_then(classify_agent_process)
        })
        .map(agent_kind_name)
        .unwrap_or("Agent");

    if let Some(pid) = agent_pid {
        set_attention_marker(pid);
    }
    notifications::spool(
        &format!("{} needs attention", agent_name),
        &message,
        agent_pid,
    );
    Ok(())
}

fn get_all_agent_processes() -> Vec<ProcessInfo> {
    let processes = load_process_table();
    select_agent_processes(&processes)
}

fn count_agent_processes() -> usize {
    get_all_agent_processes().len()
}

fn get_all_agent_pids() -> Vec<u32> {
    get_all_agent_processes()
        .into_iter()
        .map(|process| process.pid)
        .collect()
}

fn get_inactive_agent_pids() -> Vec<u32> {
    let all_pids = get_all_agent_pids();
    let active_pids: HashSet<u32> = get_instance_items()
        .iter()
        .map(|(pid, _, _, _)| *pid)
        .collect();
    all_pids
        .into_iter()
        .filter(|pid| !active_pids.contains(pid))
        .collect()
}

fn cmd_status() -> Result<()> {
    let sleep_disabled = is_sleep_disabled();
    let active_count = count_active_pids();
    let thermal_warning = check_thermal_warning();
    let agent_count = count_agent_processes();

    println!("Agents Sleep Preventer v{}", env!("CARGO_PKG_VERSION"));
    println!("==========================================");
    println!("Working instances: {}", active_count);
    println!("Agent processes: {}", agent_count);
    println!(
        "Sleep disabled: {}",
        if sleep_disabled { "Yes" } else { "No" }
    );
    println!(
        "Thermal warning: {}",
        if thermal_warning { "YES!" } else { "No" }
    );

    if active_count > 0 {
        println!("\nActive PIDs:");
        if let Ok(entries) = fs::read_dir(PIDS_DIR) {
            for entry in entries.filter_map(|e| e.ok()) {
                let pid: u32 = entry.file_name().to_string_lossy().parse().unwrap_or(0);
                if pid > 0 {
                    let age = get_file_age(&entry.path()).unwrap_or(0);
                    let cpu = get_process_cpu(pid);
                    let alive = is_process_alive(pid);
                    println!(
                        "  PID {}: age={}s, cpu={:.1}%, alive={}",
                        pid, age, cpu, alive
                    );
                }
            }
        }
    }

    Ok(())
}

fn agent_state(is_attention: bool, is_working: bool) -> AgentState {
    if is_attention {
        AgentState::Attention
    } else if is_working {
        AgentState::Working
    } else {
        AgentState::Idle
    }
}

fn build_agent_list_items(
    processes: &[ProcessInfo],
    agent_processes: &[ProcessInfo],
    working: &HashMap<u32, u64>,
    attention: &HashSet<u32>,
    cwds: &HashMap<u32, String>,
) -> Vec<AgentListItem> {
    let process_by_pid = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect::<HashMap<_, _>>();
    let mut all_pids = agent_processes
        .iter()
        .map(|process| process.pid)
        .collect::<BTreeSet<_>>();
    all_pids.extend(working.keys().copied());
    all_pids.extend(attention.iter().copied());

    let mut project_cache = HashMap::<String, ProjectInfo>::new();
    let mut agents = all_pids
        .into_iter()
        .map(|pid| {
            let process = process_by_pid.get(&pid).copied();
            let cwd = cwds.get(&pid).cloned().unwrap_or_default();
            let mut project_info = project_cache
                .entry(cwd.clone())
                .or_insert_with(|| project_info_from_cwd(&cwd))
                .clone();

            if let Some(project) = process.and_then(hermes_project_name) {
                project_info.project = project;
                // A Hermes gateway's own cwd is the Hermes source checkout,
                // not the configured profile project, so its branch is unrelated.
                project_info.branch.clear();
            }

            let state = agent_state(attention.contains(&pid), working.contains_key(&pid));
            AgentListItem {
                pid,
                kind: process
                    .and_then(|process| classify_agent_process(process))
                    .map(agent_kind_name)
                    .unwrap_or("Agent")
                    .to_string(),
                project: project_info.project,
                branch: project_info.branch,
                cwd,
                state,
                age_secs: (state == AgentState::Working)
                    .then(|| working.get(&pid).copied())
                    .flatten(),
            }
        })
        .collect::<Vec<_>>();

    agents.sort_by(|left, right| {
        left.state
            .priority()
            .cmp(&right.state.priority())
            .then_with(|| left.project.cmp(&right.project))
            .then_with(|| left.pid.cmp(&right.pid))
    });
    agents
}

fn cmd_list() -> Result<()> {
    let processes = load_process_table();
    let agent_processes = select_agent_processes(&processes);
    let working = working_pid_ages();
    let attention = attention_pids(&processes);
    let mut all_pids = agent_processes
        .iter()
        .map(|process| process.pid)
        .collect::<BTreeSet<_>>();
    all_pids.extend(working.keys().copied());
    all_pids.extend(attention.iter().copied());
    let cwd_by_pid = get_process_cwds(&all_pids.into_iter().collect::<Vec<_>>());
    let agents = build_agent_list_items(
        &processes,
        &agent_processes,
        &working,
        &attention,
        &cwd_by_pid,
    );
    let agent_by_pid = agents
        .iter()
        .map(|agent| (agent.pid, agent))
        .collect::<HashMap<_, _>>();

    let mut working_pids = working.keys().copied().collect::<Vec<_>>();
    working_pids.sort_unstable();

    let active = working_pids
        .into_iter()
        .filter_map(|pid| {
            let agent = agent_by_pid.get(&pid)?;
            let location = if agent.project.is_empty() {
                "unknown".to_string()
            } else {
                format_project_location(&agent.project, &agent.branch)
            };
            Some(json!({
                "pid": pid,
                "age_secs": working.get(&pid).copied().unwrap_or(0),
                "cpu": get_process_cpu(pid),
                "location": location,
                "kind": agent.kind.as_str(),
                "attention": attention.contains(&pid),
            }))
        })
        .collect::<Vec<_>>();

    let inactive = agent_processes
        .into_iter()
        .filter(|process| !working.contains_key(&process.pid))
        .filter_map(|process| {
            let agent = agent_by_pid.get(&process.pid)?;
            let location = if agent.project.is_empty() {
                "unknown".to_string()
            } else {
                format_project_location(&agent.project, &agent.branch)
            };
            Some(json!({
                "pid": process.pid,
                "location": location,
                "kind": agent.kind.as_str(),
                "attention": attention.contains(&process.pid),
            }))
        })
        .collect::<Vec<_>>();

    let sleep_disabled = is_sleep_disabled();
    let payload = json!({
        "agents": agents,
        "active": active,
        "inactive": inactive,
        "sleep_disabled": sleep_disabled,
        "manual_enabled": sleep_prevention_enabled_from_settings(),
        "thermal_warning": check_thermal_warning(),
    });
    println!("{}", payload);
    Ok(())
}

fn get_file_age(path: &PathBuf) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
        .map(|d| d.as_secs())
}

fn get_process_cpu(pid: u32) -> f32 {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "%cpu="])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0.0)
}

fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn is_lid_closed() -> bool {
    unsafe {
        let service_name = b"IOPMrootDomain\0";
        let matching = IOServiceMatching(service_name.as_ptr() as *const i8);
        if matching.is_null() {
            return false;
        }

        let root_domain = IOServiceGetMatchingService(kIOMasterPortDefault, matching);
        if root_domain == MACH_PORT_NULL {
            return false;
        }

        let key = CFString::new("AppleClamshellState");
        let property = IORegistryEntryCreateCFProperty(
            root_domain,
            key.as_concrete_TypeRef(),
            kCFAllocatorDefault,
            0,
        );

        IOObjectRelease(root_domain);

        if property.is_null() {
            return false;
        }

        let result = CFBoolean::wrap_under_create_rule(property as _).into();
        result
    }
}

fn force_sleep_now() {
    let _ = Command::new("sudo").args(["pmset", "sleepnow"]).output();
}

fn enable_sleep_and_trigger_if_lid_closed() -> Result<()> {
    set_sleep_disabled(false)?;
    if is_lid_closed() {
        force_sleep_now();
    }
    Ok(())
}

fn play_lid_close_sound() {
    std::thread::spawn(|| {
        let current_vol = Command::new("osascript")
            .args(["-e", "output volume of (get volume settings)"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(50);

        let _ = Command::new("osascript")
            .args(["-e", "set volume output volume 100"])
            .output();

        let _ = Command::new("afplay")
            .args(["/System/Library/Sounds/Pop.aiff"])
            .output();

        let _ = Command::new("osascript")
            .args(["-e", &format!("set volume output volume {}", current_vol)])
            .output();
    });
}

unsafe extern "C" fn clamshell_notification_callback(
    _refcon: *mut std::ffi::c_void,
    _service: io_service_t,
    _message_type: u32,
    _message_argument: *mut std::ffi::c_void,
) {
    let lid_closed = is_lid_closed();
    let was_closed = LID_WAS_CLOSED.swap(lid_closed, Ordering::SeqCst);

    if lid_closed && !was_closed {
        LID_JUST_CLOSED.store(true, Ordering::SeqCst);
    }
}

fn start_clamshell_notifications() {
    std::thread::spawn(|| unsafe {
        let notify_port = IONotificationPortCreate(kIOMasterPortDefault);
        if notify_port.is_null() {
            eprintln!("Failed to create IONotificationPort");
            return;
        }

        let run_loop_source = IONotificationPortGetRunLoopSource(notify_port);
        if run_loop_source.is_null() {
            eprintln!("Failed to get run loop source");
            IONotificationPortDestroy(notify_port);
            return;
        }

        CFRunLoopAddSource(
            CFRunLoopGetCurrent(),
            run_loop_source as *mut _,
            kCFRunLoopDefaultMode,
        );

        let service_name = b"IOPMrootDomain\0";
        let matching = IOServiceMatching(service_name.as_ptr() as *const i8);
        if matching.is_null() {
            eprintln!("Failed to create matching dictionary");
            IONotificationPortDestroy(notify_port);
            return;
        }

        let root_domain = IOServiceGetMatchingService(kIOMasterPortDefault, matching);
        if root_domain == MACH_PORT_NULL {
            eprintln!("Failed to find IOPMrootDomain");
            IONotificationPortDestroy(notify_port);
            return;
        }

        let interest_type = b"IOGeneralInterest\0";
        let mut notification: io_object_t = 0;

        let result = IOServiceAddInterestNotification(
            notify_port,
            root_domain,
            interest_type.as_ptr() as *const i8,
            clamshell_notification_callback,
            ptr::null_mut(),
            &mut notification,
        );

        IOObjectRelease(root_domain);

        if result != 0 {
            eprintln!("Failed to add interest notification: {}", result);
            IONotificationPortDestroy(notify_port);
            return;
        }

        CFRunLoopRun();
    });
}

fn cmd_cleanup() -> Result<()> {
    if let Ok(entries) = fs::read_dir(PIDS_DIR) {
        for entry in entries.filter_map(|e| e.ok()) {
            let pid: u32 = match entry.file_name().to_string_lossy().parse() {
                Ok(p) => p,
                Err(_) => continue,
            };

            let path = entry.path();

            if !is_process_alive(pid) {
                let _ = fs::remove_file(&path);
                continue;
            }

            let age = get_file_age(&path).unwrap_or(0);
            if age >= IDLE_TIMEOUT_SECS {
                let cpu = get_process_cpu(pid);
                if cpu < IDLE_CPU_THRESHOLD {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }

    // Fix sleep state
    let active = count_active_pids();
    let sleep_disabled = is_sleep_disabled();

    if active > 0 && !sleep_disabled {
        set_sleep_disabled(true)?;
    } else if active == 0 && sleep_disabled {
        enable_sleep_and_trigger_if_lid_closed()?;
    }

    Ok(())
}

fn cmd_reset() -> Result<()> {
    let _ = fs::remove_dir_all(PIDS_DIR);
    let _ = fs::create_dir_all(PIDS_DIR);
    enable_sleep_and_trigger_if_lid_closed()?;
    println!("Reset complete. Sleep re-enabled.");
    Ok(())
}

fn cmd_thermal() -> Result<()> {
    let warning = check_thermal_warning();
    if warning {
        println!("THERMAL WARNING DETECTED!");
        // Force reset if thermal warning
        cmd_reset()?;
    } else {
        println!("Thermal state: OK");
    }
    Ok(())
}

fn cmd_daemon(interval: u64) -> Result<()> {
    eprintln!(
        "Daemon started (interval: {}s, thermal check: 30s)",
        interval
    );

    let mut thermal_counter = 0u64;

    loop {
        // Cleanup every interval
        let _ = cmd_cleanup();

        // Thermal check every 30 seconds
        thermal_counter += interval;
        if thermal_counter >= 30 {
            thermal_counter = 0;
            if check_thermal_warning() {
                eprintln!("Thermal warning! Forcing sleep re-enable.");
                let _ = cmd_reset();
            }
        }

        std::thread::sleep(Duration::from_secs(interval));
    }
}

fn create_tray_title(count: usize, manual_enabled: bool) -> String {
    if manual_enabled && count > 0 {
        format!("ON {}", count)
    } else {
        "Zz".to_string()
    }
}

fn resolve_user_home() -> Result<PathBuf> {
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        let sudo_user = sudo_user.trim();
        if !sudo_user.is_empty() && sudo_user != "root" {
            let user_record = format!("/Users/{}", sudo_user);
            if let Ok(output) = Command::new("dscl")
                .args([".", "-read", &user_record, "NFSHomeDirectory"])
                .output()
            {
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    for line in stdout.lines() {
                        if let Some(home) = line.trim().strip_prefix("NFSHomeDirectory:") {
                            let home = home.trim();
                            if !home.is_empty() {
                                return Ok(PathBuf::from(home));
                            }
                        }
                    }
                }
            }
            return Ok(PathBuf::from(user_record));
        }
    }

    dirs::home_dir().context("Could not find home directory")
}

#[cfg(unix)]
fn fix_user_ownership(path: &Path) {
    let Ok(sudo_user) = std::env::var("SUDO_USER") else {
        return;
    };
    let sudo_user = sudo_user.trim();
    if sudo_user.is_empty() || sudo_user == "root" {
        return;
    }
    let Some(path) = path.to_str() else {
        return;
    };
    let _ = Command::new("chown").args(["-R", sudo_user, path]).status();
}

fn toml_section_name(line: &str) -> Option<&str> {
    let code = line.split('#').next().unwrap_or("").trim();
    if !code.starts_with('[') || !code.ends_with(']') {
        return None;
    }
    Some(code.trim_matches(&['[', ']'][..]).trim())
}

fn set_toml_feature_true(content: &str, feature: &str) -> String {
    let mut lines = content.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    let mut features_start = None;
    let mut features_end = lines.len();

    for (idx, line) in lines.iter().enumerate() {
        let Some(section) = toml_section_name(line) else {
            continue;
        };
        if section == "features" {
            features_start = Some(idx);
            features_end = lines.len();
        } else if features_start.is_some() {
            features_end = idx;
            break;
        }
    }

    if let Some(start) = features_start {
        for line in lines.iter_mut().take(features_end).skip(start + 1) {
            let code = line.split('#').next().unwrap_or("").trim_start();
            if let Some(rest) = code.strip_prefix(feature) {
                if rest.trim_start().starts_with('=') {
                    let indent = line
                        .chars()
                        .take_while(|ch| ch.is_whitespace())
                        .collect::<String>();
                    *line = format!("{}{} = true", indent, feature);
                    return format!("{}\n", lines.join("\n"));
                }
            }
        }
        let mut insert_at = features_end;
        while insert_at > start + 1
            && lines
                .get(insert_at - 1)
                .map(|line| line.trim().is_empty())
                .unwrap_or(false)
        {
            insert_at -= 1;
        }
        lines.insert(insert_at, format!("{} = true", feature));
    } else {
        if !lines.is_empty()
            && lines
                .last()
                .map(|line| !line.trim().is_empty())
                .unwrap_or(false)
        {
            lines.push(String::new());
        }
        lines.push("[features]".to_string());
        lines.push(format!("{} = true", feature));
    }

    format!("{}\n", lines.join("\n"))
}

fn remove_toml_feature(content: &str, feature: &str) -> String {
    let mut changed = false;
    let mut in_features = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        if let Some(section) = toml_section_name(line) {
            in_features = section == "features";
        }

        if in_features {
            let code = line.split('#').next().unwrap_or("").trim_start();
            if let Some(rest) = code.strip_prefix(feature) {
                if rest.trim_start().starts_with('=') {
                    changed = true;
                    continue;
                }
            }
        }

        lines.push(line.to_string());
    }

    if changed {
        format!("{}\n", lines.join("\n"))
    } else {
        content.to_string()
    }
}

fn set_codex_hooks_feature(content: &str) -> String {
    let without_legacy = remove_toml_feature(content, "codex_hooks");
    set_toml_feature_true(&without_legacy, "hooks")
}

fn enable_codex_hooks_feature(config_file: &Path) -> Result<()> {
    let content = fs::read_to_string(config_file).unwrap_or_default();
    let updated = set_codex_hooks_feature(&content);
    if updated != content {
        fs::write(config_file, updated)
            .with_context(|| format!("Failed to write {}", config_file.display()))?;
    }
    Ok(())
}

fn hook_value_contains_owned_command(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => OWNED_HOOK_MARKERS
            .iter()
            .any(|marker| text.contains(marker)),
        serde_json::Value::Array(values) => values.iter().any(hook_value_contains_owned_command),
        serde_json::Value::Object(map) => map.values().any(hook_value_contains_owned_command),
        _ => false,
    }
}

fn remove_owned_hooks_from_group(group: &mut serde_json::Value) -> bool {
    let Some(hooks) = group
        .get_mut("hooks")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };

    let before = hooks.len();
    hooks.retain(|hook| !hook_value_contains_owned_command(hook));
    before != hooks.len()
}

fn remove_owned_hook_groups(hooks: &mut serde_json::Value) -> bool {
    let Some(events) = hooks.as_object_mut() else {
        return false;
    };

    let mut changed = false;
    for groups in events.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };

        for group in groups.iter_mut() {
            if remove_owned_hooks_from_group(group) {
                changed = true;
            }
        }

        let before = groups.len();
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(serde_json::Value::as_array)
                .map(|hooks| !hooks.is_empty())
                .unwrap_or(true)
        });
        changed |= before != groups.len();
    }

    changed
}

fn prune_empty_hook_events(hooks: &mut serde_json::Value) {
    if let Some(events) = hooks.as_object_mut() {
        events.retain(|_, groups| {
            groups
                .as_array()
                .map(|groups| !groups.is_empty())
                .unwrap_or(true)
        });
    }
}

fn command_hook_group(command: &str, matcher: Option<&str>) -> serde_json::Value {
    let mut group = json!({
        "hooks": [
            {
                "type": "command",
                "command": command,
                "timeout": 5
            }
        ]
    });
    if let Some(matcher) = matcher {
        group["matcher"] = json!(matcher);
    }
    group
}

fn append_codex_hook_group(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    event_name: &str,
    group: serde_json::Value,
) {
    let event = hooks
        .entry(event_name.to_string())
        .or_insert_with(|| json!([]));
    if !event.is_array() {
        *event = json!([]);
    }
    if let Some(groups) = event.as_array_mut() {
        groups.push(group);
    }
}

fn install_codex_hooks(home: &Path, app_binary: &str) -> Result<()> {
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir)
        .with_context(|| format!("Failed to create {}", codex_dir.display()))?;

    let config_file = codex_dir.join("config.toml");
    enable_codex_hooks_feature(&config_file)?;

    let hooks_file = codex_dir.join("hooks.json");
    let mut hooks_json = if hooks_file.exists() {
        let content = fs::read_to_string(&hooks_file)
            .with_context(|| format!("Failed to read {}", hooks_file.display()))?;
        serde_json::from_str::<serde_json::Value>(&content)
            .with_context(|| format!("Failed to parse {}", hooks_file.display()))?
    } else {
        json!({})
    };

    if !hooks_json.is_object() {
        hooks_json = json!({});
    }
    if !hooks_json
        .get("hooks")
        .map(serde_json::Value::is_object)
        .unwrap_or(false)
    {
        hooks_json["hooks"] = json!({});
    }

    if let Some(hooks) = hooks_json.get_mut("hooks") {
        remove_owned_hook_groups(hooks);
        prune_empty_hook_events(hooks);
    }

    let start_command =
        format!("[ -x \"{app_binary}\" ] && \"{app_binary}\" start 2>/dev/null || true");
    let stop_command =
        format!("[ -x \"{app_binary}\" ] && \"{app_binary}\" stop 2>/dev/null || true");

    let hooks = hooks_json
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
        .context("Failed to prepare Codex hooks object")?;
    append_codex_hook_group(
        hooks,
        "UserPromptSubmit",
        command_hook_group(&start_command, None),
    );
    append_codex_hook_group(
        hooks,
        "PreToolUse",
        command_hook_group(&start_command, Some("*")),
    );
    append_codex_hook_group(
        hooks,
        "PostToolUse",
        command_hook_group(&start_command, Some("*")),
    );
    append_codex_hook_group(hooks, "Stop", command_hook_group(&stop_command, None));

    fs::write(&hooks_file, serde_json::to_string_pretty(&hooks_json)?)
        .with_context(|| format!("Failed to write {}", hooks_file.display()))?;

    #[cfg(unix)]
    fix_user_ownership(&codex_dir);

    println!("  Updated {}", config_file.display());
    println!("  Updated {}", hooks_file.display());

    Ok(())
}

const HERMES_HOOK_EVENTS: [&str; 7] = [
    "pre_llm_call",
    "post_llm_call",
    "pre_tool_call",
    "post_tool_call",
    "on_session_end",
    "on_session_finalize",
    "on_session_reset",
];

fn hermes_hooks_yaml(script_path: &str) -> String {
    let mut yaml = String::from("hooks:\n");
    for event in HERMES_HOOK_EVENTS {
        yaml.push_str(&format!(
            "  {event}:\n    - command: \"{script_path}\"\n      timeout: 5\n"
        ));
    }
    yaml
}

fn is_top_level_yaml_line(line: &str) -> bool {
    !line.trim().is_empty()
        && !line.trim_start().starts_with('#')
        && line.chars().next().is_some_and(|ch| !ch.is_whitespace())
}

fn yaml_value_before_comment(value: &str) -> &str {
    value.split('#').next().unwrap_or(value).trim()
}

fn root_hooks_line_value(line: &str) -> Option<&str> {
    if !is_top_level_yaml_line(line) {
        return None;
    }
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("hooks")?.trim_start();
    let value = rest.strip_prefix(':')?.trim_start();
    Some(value)
}

fn hermes_event_line(line: &str) -> Option<(&'static str, bool, bool)> {
    let trimmed = line.trim();
    HERMES_HOOK_EVENTS.iter().copied().find_map(|event| {
        let rest = trimmed.strip_prefix(event)?.trim_start();
        let value = rest.strip_prefix(':')?.trim_start();
        let value_before_comment = yaml_value_before_comment(value);
        let can_append_entry = value_before_comment.is_empty() || value_before_comment == "[]";
        let normalize_empty_list = value_before_comment == "[]";
        Some((event, can_append_entry, normalize_empty_list))
    })
}

fn hermes_hook_entry_yaml(script_path: &str) -> [String; 2] {
    [
        format!("    - command: \"{script_path}\""),
        "      timeout: 5".to_string(),
    ]
}

fn set_hermes_hooks_config(content: &str, script_path: &str) -> String {
    if content.contains(script_path) {
        return content.to_string();
    }

    let block = hermes_hooks_yaml(script_path);
    if content.trim().is_empty() {
        return block;
    }

    let lines = content.lines().collect::<Vec<_>>();
    if let Some(hooks_idx) = lines
        .iter()
        .position(|line| root_hooks_line_value(line).is_some())
    {
        let hooks_value = root_hooks_line_value(lines[hooks_idx]).unwrap_or_default();
        let hooks_value_before_comment = yaml_value_before_comment(hooks_value);
        let normalize_empty_hooks_map = hooks_value_before_comment == "{}";
        let hooks_end = lines[hooks_idx + 1..]
            .iter()
            .position(|line| is_top_level_yaml_line(line))
            .map(|offset| hooks_idx + 1 + offset)
            .unwrap_or(lines.len());
        let mut seen_events = HashSet::new();
        let mut output = Vec::new();

        output.extend(lines[..hooks_idx].iter().map(|line| (*line).to_string()));
        if normalize_empty_hooks_map {
            output.push("hooks:".to_string());
        } else {
            output.push(lines[hooks_idx].to_string());
        }

        for line in &lines[hooks_idx + 1..hooks_end] {
            if normalize_empty_hooks_map {
                output.push((*line).to_string());
                continue;
            }
            if let Some((event, can_append_entry, normalize_empty_list)) = hermes_event_line(line) {
                seen_events.insert(event);
                if normalize_empty_list {
                    output.push(format!("  {event}:"));
                } else {
                    output.push((*line).to_string());
                }
                if can_append_entry {
                    for entry_line in hermes_hook_entry_yaml(script_path) {
                        output.push(entry_line);
                    }
                }
            } else {
                output.push((*line).to_string());
            }
        }

        for event in HERMES_HOOK_EVENTS {
            if seen_events.insert(event) {
                output.push(format!("  {event}:"));
                for entry_line in hermes_hook_entry_yaml(script_path) {
                    output.push(entry_line);
                }
            }
        }

        output.extend(lines[hooks_end..].iter().map(|line| (*line).to_string()));
        return format!("{}\n", output.join("\n"));
    }

    let separator = if content.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{content}{separator}{block}")
}

fn remove_hermes_hooks_config(content: &str, script_path: &str) -> String {
    let command_line = format!("command: \"{script_path}\"");
    let mut output = Vec::new();
    let mut lines = content.lines().peekable();

    while let Some(line) = lines.next() {
        if line.trim_start().starts_with('-') && line.contains(&command_line) {
            if lines
                .peek()
                .is_some_and(|next| next.trim_start().starts_with("timeout:"))
            {
                lines.next();
            }
            continue;
        }
        output.push(line.to_string());
    }

    if content.ends_with('\n') {
        format!("{}\n", output.join("\n"))
    } else {
        output.join("\n")
    }
}

fn hermes_hook_script(app_binary: &str) -> String {
    format!(
        r#"#!/bin/bash
set -euo pipefail
ASP="{app_binary}"
[ -x "$ASP" ] || ASP="/usr/local/bin/asp"
[ -x "$ASP" ] || exit 0

payload="$(cat 2>/dev/null || true)"
event="${{HERMES_HOOK_EVENT:-${{hook_event_name:-}}}}"
if [ -z "$event" ]; then
  event="$(printf '%s' "$payload" | /usr/bin/sed -n 's/.*"hook_event_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | /usr/bin/head -n 1)"
fi
if [ -z "$event" ]; then
  event="$(printf '%s' "$payload" | /usr/bin/sed -n 's/.*"event"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | /usr/bin/head -n 1)"
fi

find_hermes_pid() {{
  local pid="${{PPID:-}}"
  local depth=0
  while [ -n "$pid" ] && [ "$pid" != "0" ] && [ "$depth" -lt 30 ]; do
    local line
    line="$(/bin/ps -p "$pid" -o pid=,comm=,args= 2>/dev/null || true)"
    case "$line" in
      *hermes*|*hermes_cli.main*) printf '%s\n' "$pid"; return 0 ;;
    esac
    pid="$(/bin/ps -p "$pid" -o ppid= 2>/dev/null | /usr/bin/tr -d ' ' || true)"
    depth=$((depth + 1))
  done
  return 1
}}

agent_pid="${{HERMES_ASP_AGENT_PID:-}}"
[ -n "$agent_pid" ] || agent_pid="$(find_hermes_pid || true)"
[ -n "$agent_pid" ] || exit 0
state_dir="${{TMPDIR:-/tmp}}/agents-sleep-preventer-hermes"
/bin/mkdir -p "$state_dir"
heartbeat_pid_file="$state_dir/$agent_pid.pid"
stop_file="$state_dir/$agent_pid.stop"

start_heartbeat() {{
  /bin/rm -f "$stop_file"
  if [ -f "$heartbeat_pid_file" ] && /bin/kill -0 "$(cat "$heartbeat_pid_file")" 2>/dev/null; then
    return 0
  fi
  (
    while /bin/kill -0 "$agent_pid" 2>/dev/null && [ ! -f "$stop_file" ]; do
      "$ASP" start --pid "$agent_pid" 2>/dev/null || true
      /bin/sleep 10
    done
  ) >/dev/null 2>&1 &
  printf '%s\n' "$!" > "$heartbeat_pid_file"
}}

stop_heartbeat() {{
  /usr/bin/touch "$stop_file"
  if [ -f "$heartbeat_pid_file" ]; then
    /bin/kill "$(cat "$heartbeat_pid_file")" 2>/dev/null || true
    /bin/rm -f "$heartbeat_pid_file"
  fi
  "$ASP" stop --pid "$agent_pid" 2>/dev/null || true
}}

case "$event" in
  on_session_end|on_session_finalize|on_session_reset|agent:end|subagent_stop) stop_heartbeat ;;
  *) "$ASP" start --pid "$agent_pid" 2>/dev/null || true; start_heartbeat ;;
esac
"#
    )
}

fn hermes_config_paths(home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let default_config = home.join(".hermes/config.yaml");
    if default_config.exists() {
        paths.push(default_config);
    }

    let profiles_dir = home.join(".hermes/profiles");
    if let Ok(entries) = fs::read_dir(profiles_dir) {
        for entry in entries.flatten() {
            let config = entry.path().join("config.yaml");
            if config.exists() {
                paths.push(config);
            }
        }
    }

    paths
}

fn install_hermes_hooks(home: &Path, app_binary: &str) -> Result<usize> {
    let config_paths = hermes_config_paths(home);
    let mut updated = 0;

    for config_path in config_paths {
        let Some(config_dir) = config_path.parent() else {
            continue;
        };
        let hooks_dir = config_dir.join("agent-hooks");
        fs::create_dir_all(&hooks_dir)
            .with_context(|| format!("Failed to create {}", hooks_dir.display()))?;
        let script_path = hooks_dir.join("asp-sleep.sh");
        fs::write(&script_path, hermes_hook_script(app_binary))
            .with_context(|| format!("Failed to write {}", script_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))?;
            fix_user_ownership(&hooks_dir);
        }

        let content = fs::read_to_string(&config_path).unwrap_or_default();
        let updated_content = set_hermes_hooks_config(&content, &script_path.to_string_lossy());
        if updated_content != content {
            fs::write(&config_path, updated_content)
                .with_context(|| format!("Failed to write {}", config_path.display()))?;
        }
        updated += 1;
        println!("  Updated {}", config_path.display());
    }

    Ok(updated)
}

fn remove_hermes_hooks(home: &Path) -> Result<usize> {
    let mut removed = 0;
    for config_path in hermes_config_paths(home) {
        if let Some(config_dir) = config_path.parent() {
            let script_path = config_dir.join("agent-hooks/asp-sleep.sh");
            let script_path_text = script_path.to_string_lossy();
            let content = fs::read_to_string(&config_path).unwrap_or_default();
            let updated_content = remove_hermes_hooks_config(&content, &script_path_text);
            if updated_content != content {
                fs::write(&config_path, updated_content)
                    .with_context(|| format!("Failed to write {}", config_path.display()))?;
            }
            let _ = fs::remove_file(script_path);
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hermes_process(args: &str) -> ProcessInfo {
        ProcessInfo {
            pid: 42,
            ppid: 1,
            comm: "Python".to_string(),
            args: args.to_string(),
        }
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("asp-{label}-{}-{id}", std::process::id()))
    }

    #[test]
    fn set_codex_hooks_feature_adds_current_flag() {
        let updated = set_codex_hooks_feature("model = \"gpt-5.5\"\n");

        assert_eq!(updated, "model = \"gpt-5.5\"\n\n[features]\nhooks = true\n");
    }

    #[test]
    fn set_codex_hooks_feature_replaces_deprecated_flag() {
        let config = "\
model = \"gpt-5.5\"

[features]
unified_exec = true
codex_hooks = true

[plugins.github]
enabled = true
";

        let updated = set_codex_hooks_feature(config);

        assert!(!updated.contains("codex_hooks"));
        assert!(updated.contains("[features]\nunified_exec = true\nhooks = true"));
        assert!(updated.contains("[plugins.github]\nenabled = true"));
    }

    #[test]
    fn set_codex_hooks_feature_updates_existing_hooks_flag() {
        let config = "\
[features]
hooks = false
";

        let updated = set_codex_hooks_feature(config);

        assert_eq!(updated, "[features]\nhooks = true\n");
    }

    #[test]
    fn classify_agent_process_detects_hermes_binary() {
        let process = ProcessInfo {
            pid: 42,
            ppid: 1,
            comm: "hermes".to_string(),
            args: "/opt/homebrew/bin/hermes --profile agents-sleep-preventer gateway run"
                .to_string(),
        };

        assert_eq!(classify_agent_process(&process), Some(AgentKind::Hermes));
    }

    #[test]
    fn classify_agent_process_detects_python_module_hermes() {
        let process = ProcessInfo {
            pid: 42,
            ppid: 1,
            comm: "Python".to_string(),
            args:
                "/usr/bin/python3 -m hermes_cli.main --profile agents-sleep-preventer gateway run"
                    .to_string(),
        };

        assert_eq!(classify_agent_process(&process), Some(AgentKind::Hermes));
    }

    #[test]
    fn classify_agent_process_ignores_unrelated_python() {
        let process = ProcessInfo {
            pid: 42,
            ppid: 1,
            comm: "Python".to_string(),
            args: "/usr/bin/python3 -m http.server".to_string(),
        };

        assert_eq!(classify_agent_process(&process), None);
    }

    #[test]
    fn hermes_profile_parser_supports_separate_and_equals_forms() {
        assert_eq!(
            hermes_profile_from_args(
                "/usr/bin/python3 -m hermes_cli.main --profile cleemo gateway run"
            )
            .as_deref(),
            Some("cleemo")
        );
        assert_eq!(
            hermes_profile_from_args("hermes --profile=personal-branding gateway run").as_deref(),
            Some("personal-branding")
        );
        assert_eq!(hermes_profile_from_args("hermes gateway run"), None);
    }

    #[test]
    fn hermes_project_uses_profile_and_names_default_gateway() {
        let profiled = hermes_process(
            "/usr/bin/python3 -m hermes_cli.main --profile agents-sleep-preventer gateway run",
        );
        let default = hermes_process("/usr/bin/python3 -m hermes_cli.main gateway run");

        assert_eq!(
            hermes_project_name(&profiled).as_deref(),
            Some("agents-sleep-preventer")
        );
        assert_eq!(hermes_project_name(&default).as_deref(), Some("default"));
    }

    #[test]
    fn parse_lsof_cwds_associates_each_path_with_its_pid() {
        let output = "p1097\nfcwd\nn/Users/example/.hermes/hermes-agent\np12110\nfcwd\nn/Users/example/projects/cleemo-lamdera\n";

        let parsed = parse_lsof_cwds(output);

        assert_eq!(
            parsed.get(&1097).map(String::as_str),
            Some("/Users/example/.hermes/hermes-agent")
        );
        assert_eq!(
            parsed.get(&12110).map(String::as_str),
            Some("/Users/example/projects/cleemo-lamdera")
        );
    }

    #[test]
    fn project_info_uses_git_root_and_head_branch() {
        let repo = unique_test_dir("project-info");
        let nested = repo.join("src/deep");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/menu\n").unwrap();

        let info = project_info_from_cwd(nested.to_str().unwrap());

        assert_eq!(info.project, path_display_name(&repo));
        assert_eq!(info.branch, "feature/menu");
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn agent_state_prioritizes_attention_over_working() {
        assert_eq!(agent_state(true, true), AgentState::Attention);
        assert_eq!(agent_state(false, true), AgentState::Working);
        assert_eq!(agent_state(false, false), AgentState::Idle);
    }

    #[test]
    fn claude_attention_status_recognizes_waiting_busy_and_idle() {
        assert_eq!(
            parse_claude_attention_status(r#"{"status":"waiting"}"#),
            Some(ClaudeAttentionStatus::Waiting)
        );
        assert_eq!(
            parse_claude_attention_status(r#"{"status":"attention"}"#),
            Some(ClaudeAttentionStatus::Waiting)
        );
        assert_eq!(
            parse_claude_attention_status(r#"{"status":"busy"}"#),
            Some(ClaudeAttentionStatus::NotWaiting)
        );
        assert_eq!(
            parse_claude_attention_status(r#"{"status":"idle"}"#),
            Some(ClaudeAttentionStatus::NotWaiting)
        );
        assert_eq!(
            parse_claude_attention_status(r#"{"status":"starting"}"#),
            None
        );
    }

    #[test]
    fn attention_reconciliation_preserves_fresh_hooks_and_expires_unknown_markers() {
        assert!(should_keep_attention_marker(
            ATTENTION_RECONCILE_GRACE_SECS,
            Some(ClaudeAttentionStatus::NotWaiting)
        ));
        assert!(!should_keep_attention_marker(
            ATTENTION_RECONCILE_GRACE_SECS + 1,
            Some(ClaudeAttentionStatus::NotWaiting)
        ));
        assert!(should_keep_attention_marker(
            ATTENTION_FALLBACK_EXPIRY_SECS - 1,
            None
        ));
        assert!(!should_keep_attention_marker(
            ATTENTION_FALLBACK_EXPIRY_SECS,
            None
        ));
        assert!(should_keep_attention_marker(
            ATTENTION_FALLBACK_EXPIRY_SECS * 2,
            Some(ClaudeAttentionStatus::Waiting)
        ));
    }

    #[test]
    fn claude_attention_status_reads_pid_session_file() {
        let home = unique_test_dir("claude-session");
        let sessions = home.join(".claude/sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("42.json"), r#"{"status":"waiting"}"#).unwrap();

        assert_eq!(
            claude_attention_status(&home, 42),
            Some(ClaudeAttentionStatus::Waiting)
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn set_hermes_hooks_config_replaces_empty_hooks_map() {
        let updated = set_hermes_hooks_config(
            "hooks: {}\nmodel:\n  provider: openrouter\n",
            "/tmp/asp-sleep.sh",
        );

        assert!(updated.contains("hooks:\n  pre_llm_call:"));
        assert!(updated.contains("command: \"/tmp/asp-sleep.sh\""));
        assert!(updated.contains("model:\n  provider: openrouter"));
    }

    #[test]
    fn set_hermes_hooks_config_handles_commented_hooks_header() {
        let updated = set_hermes_hooks_config(
            "hooks: # shell integrations\n  pre_llm_call: [] # none yet\nmodel:\n  provider: openrouter\n",
            "/tmp/asp-sleep.sh",
        );

        assert_eq!(updated.matches("hooks:").count(), 1);
        assert_eq!(updated.matches("  pre_llm_call:").count(), 1);
        assert!(updated.contains("  pre_llm_call:\n    - command: \"/tmp/asp-sleep.sh\""));
        assert!(updated.contains("model:\n  provider: openrouter"));
    }

    #[test]
    fn set_hermes_hooks_config_replaces_commented_empty_hooks_map() {
        let updated = set_hermes_hooks_config(
            "hooks: {} # none configured\nmodel:\n  provider: openrouter\n",
            "/tmp/asp-sleep.sh",
        );

        assert_eq!(updated.matches("hooks:").count(), 1);
        assert!(updated.contains("hooks:\n  pre_llm_call:"));
        assert!(updated.contains("model:\n  provider: openrouter"));
    }

    #[test]
    fn set_hermes_hooks_config_is_idempotent() {
        let once = set_hermes_hooks_config("model:\n  provider: openrouter\n", "/tmp/asp-sleep.sh");
        let twice = set_hermes_hooks_config(&once, "/tmp/asp-sleep.sh");

        assert_eq!(
            twice.matches("/tmp/asp-sleep.sh").count(),
            once.matches("/tmp/asp-sleep.sh").count()
        );
    }

    #[test]
    fn set_hermes_hooks_config_merges_existing_events() {
        let updated = set_hermes_hooks_config(
            "model:\n  provider: openrouter\nhooks:\n  pre_llm_call:\n    - command: \"/tmp/existing.sh\"\n      timeout: 7\n  post_tool_call:\n    - command: \"/tmp/tool.sh\"\nsettings:\n  theme: dark\n",
            "/tmp/asp-sleep.sh",
        );

        assert_eq!(updated.matches("  pre_llm_call:").count(), 1);
        assert_eq!(updated.matches("  post_tool_call:").count(), 1);
        assert!(updated.contains("command: \"/tmp/existing.sh\""));
        assert!(updated.contains("command: \"/tmp/tool.sh\""));
        assert!(updated.contains("command: \"/tmp/asp-sleep.sh\""));
        assert!(updated.contains("settings:\n  theme: dark"));
    }

    #[test]
    fn set_hermes_hooks_config_handles_inline_empty_event() {
        let updated = set_hermes_hooks_config(
            "hooks:\n  pre_llm_call: []\n  post_llm_call: # keep comment\nsettings:\n  theme: dark\n",
            "/tmp/asp-sleep.sh",
        );

        assert_eq!(updated.matches("  pre_llm_call:").count(), 1);
        assert_eq!(updated.matches("  post_llm_call:").count(), 1);
        assert!(updated.contains("hooks:\n  pre_llm_call:\n    - command: \"/tmp/asp-sleep.sh\""));
        assert!(updated
            .contains("  post_llm_call: # keep comment\n    - command: \"/tmp/asp-sleep.sh\""));
    }

    #[test]
    fn remove_hermes_hooks_config_removes_only_asp_entries() {
        let config = "hooks:\n  pre_llm_call:\n    - command: \"/tmp/asp-sleep.sh\"\n      timeout: 5\n    - command: \"/tmp/existing.sh\"\n      timeout: 7\n  on_session_end:\n    - command: \"/tmp/asp-sleep.sh\"\n      timeout: 5\nmodel:\n  provider: openrouter\n";

        let updated = remove_hermes_hooks_config(config, "/tmp/asp-sleep.sh");

        assert!(!updated.contains("/tmp/asp-sleep.sh"));
        assert!(updated.contains("command: \"/tmp/existing.sh\""));
        assert!(updated.contains("model:\n  provider: openrouter"));
    }

    #[test]
    fn hermes_hook_script_reads_hook_event_name() {
        let script = hermes_hook_script("/usr/local/bin/asp");

        assert!(script.contains("hook_event_name"));
        assert!(script.contains("on_session_end|on_session_finalize|on_session_reset"));
    }
}

fn remove_codex_hooks(home: &Path) -> Result<bool> {
    let hooks_file = home.join(".codex/hooks.json");
    if !hooks_file.exists() {
        return Ok(false);
    }

    let content = fs::read_to_string(&hooks_file)
        .with_context(|| format!("Failed to read {}", hooks_file.display()))?;
    let Ok(mut hooks_json) = serde_json::from_str::<serde_json::Value>(&content) else {
        eprintln!(
            "Warning: could not parse {}, leaving it unchanged",
            hooks_file.display()
        );
        return Ok(false);
    };

    let changed = hooks_json
        .get_mut("hooks")
        .map(remove_owned_hook_groups)
        .unwrap_or(false);
    if !changed {
        return Ok(false);
    }

    if let Some(hooks) = hooks_json.get_mut("hooks") {
        prune_empty_hook_events(hooks);
    }

    if let Some(root) = hooks_json.as_object_mut() {
        let hooks_empty = root
            .get("hooks")
            .and_then(serde_json::Value::as_object)
            .map(|hooks| hooks.is_empty())
            .unwrap_or(false);
        if hooks_empty {
            root.remove("hooks");
        }
        if root.is_empty() {
            fs::remove_file(&hooks_file)
                .with_context(|| format!("Failed to remove {}", hooks_file.display()))?;
            return Ok(true);
        }
    }

    fs::write(&hooks_file, serde_json::to_string_pretty(&hooks_json)?)
        .with_context(|| format!("Failed to write {}", hooks_file.display()))?;

    Ok(true)
}

fn is_codex_hooks_installed(home: &Path) -> bool {
    let hooks_file = home.join(".codex/hooks.json");
    fs::read_to_string(hooks_file)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .map(|hooks_json| hook_value_contains_owned_command(&hooks_json))
        .unwrap_or(false)
}

fn is_claude_hooks_installed(home: &Path) -> bool {
    // agent-attention.sh checked too so upgrades from pre-notification
    // versions re-run setup and pick up the Notification hook.
    home.join(".claude/hooks/prevent-sleep.sh").exists()
        && home.join(".claude/hooks/agent-attention.sh").exists()
}

fn is_installed() -> bool {
    let home = resolve_user_home().unwrap_or_default();
    is_claude_hooks_installed(&home) && is_codex_hooks_installed(&home)
}

fn run_first_time_setup() -> Result<()> {
    let message = "Agents Sleep Preventer needs to be configured for supported coding agents.

This will:
• Install the CLI tool
• Configure coding agent hooks
• Set up automatic startup

Administrator password required.";

    if !native_dialogs::show_confirm_dialog(message, "Agents Sleep Preventer", "Set Up", "Cancel") {
        return Ok(());
    }

    let script = "/Applications/AgentsSleepPreventer.app/Contents/MacOS/asp install -y";

    match authorization::execute_script_with_privileges(script) {
        Ok(true) => {
            native_dialogs::show_dialog(
                "Setup complete!\n\nRestart your coding agents to activate sleep prevention.",
                "Agents Sleep Preventer",
            );
            relaunch_app_after_install();
        }
        Ok(false) => {
            // User cancelled
        }
        Err(e) => {
            native_dialogs::show_dialog(&format!("Setup failed: {}", e), "Agents Sleep Preventer");
        }
    }

    Ok(())
}

/// macOS Gatekeeper "App Translocation" runs a quarantined .app from a
/// randomized read-only path whenever it's launched from outside
/// /Applications (straight off a mounted DMG, ~/Downloads, etc). TCC
/// permission grants (Accessibility, Microphone) are tied to that path,
/// so every such launch gets a fresh random path and looks like the OS
/// silently revoked previously-granted permissions. The fix is to move the
/// bundle into /Applications (a stable path) and relaunch from there.
fn ensure_running_from_applications() {
    let Some(bundle_path) = objc_utils::main_bundle_path() else {
        return;
    };
    if !bundle_path.ends_with(".app") || bundle_path == APP_BUNDLE_PATH {
        return;
    }

    logging::log(&format!(
        "[main] App running from non-standard location: {}",
        bundle_path
    ));

    let message = format!(
        "Agents Sleep Preventer is running from:\n{}\n\nmacOS permissions (Accessibility, Microphone) only stay granted reliably when the app runs from /Applications — otherwise they can silently reset and the dictation shortcut stops working.\n\nMove it to /Applications and relaunch now?",
        bundle_path
    );

    if !native_dialogs::show_confirm_dialog(
        &message,
        "Move to Applications?",
        "Move & Relaunch",
        "Not Now",
    ) {
        logging::log("[main] User declined moving app to /Applications");
        return;
    }

    if Path::new(APP_BUNDLE_PATH).exists() {
        let _ = fs::remove_dir_all(APP_BUNDLE_PATH);
    }

    let copied = Command::new("ditto")
        .args([bundle_path.as_str(), APP_BUNDLE_PATH])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !copied {
        logging::log("[main] Failed to copy app to /Applications");
        native_dialogs::show_dialog(
            "Couldn't move the app automatically. Please drag AgentsSleepPreventer.app to your Applications folder yourself, then relaunch it.",
            "Move Failed",
        );
        return;
    }

    let _ = Command::new("xattr")
        .args(["-dr", "com.apple.quarantine", APP_BUNDLE_PATH])
        .status();

    logging::log("[main] Moved app to /Applications, relaunching...");
    match Command::new("open").args(["-n", APP_BUNDLE_PATH]).status() {
        Ok(status) if status.success() => std::process::exit(0),
        Ok(status) => logging::log(&format!("[main] Relaunch failed with status: {}", status)),
        Err(e) => logging::log(&format!("[main] Relaunch failed: {}", e)),
    }
}

fn relaunch_app_after_install() {
    logging::log("[main] Relaunching app after install...");
    match Command::new("open")
        .args(["-n", "/Applications/AgentsSleepPreventer.app"])
        .status()
    {
        Ok(status) if status.success() => {
            std::process::exit(0);
        }
        Ok(status) => {
            logging::log(&format!("[main] Relaunch failed with status: {}", status));
        }
        Err(e) => {
            logging::log(&format!("[main] Relaunch failed: {}", e));
        }
    }
}

fn get_process_tty(pid: u32) -> Option<String> {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "tty="])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "??")
}

fn focus_terminal_by_tty(tty: &str) {
    let tty_path = if tty.starts_with("/dev/") {
        tty.to_string()
    } else {
        format!("/dev/{}", tty)
    };

    // Try Terminal.app first
    let terminal_script = format!(
        r#"
        tell application "Terminal"
            set windowList to every window
            repeat with w in windowList
                set tabList to every tab of w
                repeat with t in tabList
                    if tty of t is "{}" then
                        set frontmost of w to true
                        set selected of t to true
                        activate
                        return true
                    end if
                end repeat
            end repeat
        end tell
        return false
        "#,
        tty_path
    );

    let result = Command::new("osascript")
        .args(["-e", &terminal_script])
        .output();

    if let Ok(output) = result {
        if String::from_utf8_lossy(&output.stdout).trim() == "true" {
            return;
        }
    }

    // Try iTerm2 - double activate to ensure space switch
    let iterm_script = format!(
        r#"
        tell application "iTerm2"
            set windowList to every window
            repeat with w in windowList
                set tabList to every tab of w
                repeat with t in tabList
                    set sessionList to every session of t
                    repeat with s in sessionList
                        if tty of s is "{}" then
                            activate
                            delay 0.1
                            select w
                            select t
                            select s
                            activate
                            return true
                        end if
                    end repeat
                end repeat
            end repeat
        end tell
        return false
        "#,
        tty_path
    );

    let _ = Command::new("osascript")
        .args(["-e", &iterm_script])
        .output();
}

fn focus_terminal_by_pid(pid: u32) {
    if let Some(tty) = get_process_tty(pid) {
        focus_terminal_by_tty(&tty);
    }
}

fn cmd_focus(pid: u32) -> Result<()> {
    logging::init();
    logging::log(&format!("[focus] requested pid={}", pid));
    focus_terminal_by_pid(pid);
    Ok(())
}

fn get_instance_items() -> Vec<(u32, u64, f32, String)> {
    let mut items = Vec::new();
    if let Ok(entries) = fs::read_dir(PIDS_DIR) {
        for entry in entries.filter_map(|e| e.ok()) {
            let pid: u32 = entry.file_name().to_string_lossy().parse().unwrap_or(0);
            if pid > 0 {
                let age = get_file_age(&entry.path()).unwrap_or(0);
                let cpu = get_process_cpu(pid);
                let location = format_process_location(pid);
                items.push((pid, age, cpu, location));
            }
        }
    }
    items
}

fn quit_app() {
    unsafe {
        let app: objc_utils::Id = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let _: () = msg_send![app, terminate: objc_utils::NIL];
    }
}

fn cmd_menubar() -> Result<()> {
    // Initialize logging
    logging::init();
    logging::log("[main] Starting menubar app");

    ensure_running_from_applications();

    if !is_installed() {
        run_first_time_setup()?;
        if !is_installed() {
            return Ok(());
        }
    }

    // Load settings and initialize sleep prevention state
    let app_settings = settings::AppSettings::load();
    MANUAL_SLEEP_PREVENTION.store(app_settings.sleep_prevention.enabled, Ordering::SeqCst);
    logging::log(&format!(
        "[main] Loaded settings: sleep_prevention={}",
        app_settings.sleep_prevention.enabled
    ));

    let mut event_loop = EventLoopBuilder::new().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let tick_proxy = event_loop.create_proxy();

    let initial_instances = get_instance_items();
    let manual_enabled = MANUAL_SLEEP_PREVENTION.load(Ordering::SeqCst);

    notifications::init_click_handler();

    // Initialize dictation manager
    let mut dictation_manager = DictationManager::new();

    // Create a minimal menu for right-click, but show popover on left-click
    let minimal_menu = Menu::new();
    let settings_item = MenuItem::new("Settings...", true, None);
    let settings_item_id = settings_item.id().clone();
    let _ = minimal_menu.append(&settings_item);
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_item_id = quit_item.id().clone();
    let _ = minimal_menu.append(&quit_item);

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(minimal_menu))
        .with_menu_on_left_click(false) // Left-click shows popover, right-click shows menu
        .with_title(&create_tray_title(initial_instances.len(), manual_enabled))
        .with_tooltip("Agents Sleep Preventer")
        .build()?;

    let menu_channel = MenuEvent::receiver();
    let tray_event_channel = TrayIconEvent::receiver();

    // Initialize popover window
    let mut popover = popover::PopoverWindow::new();

    // Register global hotkeys
    let hotkey_manager = GlobalHotKeyManager::new().unwrap();
    let hotkey_active = HotKey::new(
        Some(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER),
        Code::KeyJ,
    );
    let hotkey_inactive = HotKey::new(
        Some(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER),
        Code::KeyL,
    );
    let hotkey_active_id = hotkey_active.id();
    let hotkey_inactive_id = hotkey_inactive.id();
    hotkey_manager.register(hotkey_active).unwrap();
    hotkey_manager.register(hotkey_inactive).unwrap();
    let hotkey_receiver = GlobalHotKeyEvent::receiver();

    start_clamshell_notifications();

    std::thread::spawn(move || {
        let mut tick_counter = 0u64;
        let _ = tick_proxy.send_event(());

        loop {
            std::thread::sleep(Duration::from_millis(100));
            tick_counter += 1;

            if LID_JUST_CLOSED.swap(false, Ordering::SeqCst) {
                let active = count_active_pids();
                if active > 0 {
                    play_lid_close_sound();
                }
            }

            if tick_counter % 100 == 0 {
                cleanup_stale_pids();
                menubar_sync_sleep();
            }

            if tick_counter % 300 == 0 {
                if check_thermal_warning() {
                    let _ = set_sleep_disabled(false);
                    logging::log("[thermal] Sleep re-enabled due to thermal warning");
                }
            }

            let _ = tick_proxy.send_event(());
        }
    });

    let mut last_update = std::time::Instant::now() - Duration::from_secs(10);

    let mut click_check_counter = 0u64;
    let mut onboarding_checked = false;
    event_loop.run(move |event, _, control_flow| {
        // Drive updates from the tick thread to avoid relying on user input events.
        *control_flow = ControlFlow::Wait;

        if !onboarding_checked {
            if let Event::NewEvents(StartCause::Init) = event {
                onboarding_checked = true;
                run_onboarding_if_needed(false);
                let dictation_available = dictation_manager.is_available();
                let dictation_enabled = dictation_manager.is_enabled();
                logging::log(&format!(
                    "[dictation] available={}, enabled={}",
                    dictation_available, dictation_enabled
                ));
                if dictation_available {
                    if let Err(e) = dictation_manager.start() {
                        logging::log(&format!("[dictation] Failed to start: {}", e));
                    }
                }
            }
        }

        click_check_counter += 1;
        if click_check_counter % 100 == 0 {
            eprintln!("[DEBUG] Event loop iteration #{}", click_check_counter);
        }
        while let Ok(menu_event) = menu_channel.try_recv() {
            if menu_event.id == settings_item_id {
                logging::log("[menu] Settings selected");
                popover.hide();
                let was_available = dictation_manager.is_available();
                if let Some(new_settings) = settings::window::show_settings() {
                    // Update manual sleep prevention based on settings
                    MANUAL_SLEEP_PREVENTION
                        .store(new_settings.sleep_prevention.enabled, Ordering::SeqCst);
                    menubar_sync_sleep();
                    logging::log(&format!(
                        "[menu] Settings saved: sleep_enabled={}, model={}",
                        new_settings.sleep_prevention.enabled, new_settings.speech_to_text.model
                    ));

                    // If the selected dictation model isn't downloaded yet,
                    // offer to download it now, then pick it up.
                    dictation::ensure_selected_model_downloaded();
                    dictation_manager.reload_transcriber();
                    dictation_manager.reload_hotkey();
                    if !was_available && dictation_manager.is_available() {
                        if let Err(e) = dictation_manager.start() {
                            logging::log(&format!("[dictation] Failed to start: {}", e));
                        }
                    }
                }
            } else if menu_event.id == quit_item_id {
                logging::log("[menu] Quit selected");
                dictation_manager.stop();
                quit_app();
                logging::log("[menu] Forcing process exit");
                unsafe {
                    libc::_exit(0);
                }
            }
        }

        // Check for tray events every iteration
        while let Ok(tray_event) = tray_event_channel.try_recv() {
            eprintln!("[TRAY EVENT] {:?}", tray_event);
            logging::log(&format!("[tray] EVENT: {:?}", tray_event));
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Down,
                rect,
                ..
            } = tray_event
            {
                logging::log(&format!("[tray] Left click at rect: {:?}", rect));
                if popover.is_visible() {
                    popover.hide();
                } else {
                    let icon_rect = objc_utils::tray_rect_to_appkit((
                        rect.position.x,
                        rect.position.y,
                        rect.size.width as f64,
                        rect.size.height as f64,
                    ));
                    let pstate = popover::PopoverState {
                        manual_enabled: MANUAL_SLEEP_PREVENTION.load(Ordering::SeqCst),
                        instances: get_instance_items(),
                        inactive: get_inactive_agent_pids(),
                        thermal_warning: check_thermal_warning(),
                        dictation_enabled: dictation_manager.is_enabled(),
                        dictation_available: dictation_manager.is_available(),
                        sleep_disabled: is_sleep_disabled(),
                    };
                    popover.show(icon_rect, &pstate);
                }
            }
        }

        // Update dictation manager (handles Fn+Space events)
        dictation_manager.update();

        if last_update.elapsed() >= Duration::from_secs(2) {
            last_update = std::time::Instant::now();

            let instances = get_instance_items();
            let manual_enabled = MANUAL_SLEEP_PREVENTION.load(Ordering::SeqCst);

            tray.set_title(Some(&create_tray_title(instances.len(), manual_enabled)));

            notifications::drain_and_post();
        }

        // Menu events commented out - we use popover instead
        // The menu handling code has been removed since we no longer attach a menu

        // Handle global hotkeys
        if let Ok(event) = hotkey_receiver.try_recv() {
            if event.state == global_hotkey::HotKeyState::Pressed {
                logging::log(&format!("[hotkey] Pressed id={}", event.id));
                if event.id == hotkey_active_id {
                    let instances = get_instance_items();
                    if !instances.is_empty() {
                        let idx =
                            CURRENT_PID_INDEX.fetch_add(1, Ordering::SeqCst) % instances.len();
                        focus_terminal_by_pid(instances[idx].0);
                    }
                } else if event.id == hotkey_inactive_id {
                    let inactive = get_inactive_agent_pids();
                    if !inactive.is_empty() {
                        let idx =
                            CURRENT_INACTIVE_INDEX.fetch_add(1, Ordering::SeqCst) % inactive.len();
                        focus_terminal_by_pid(inactive[idx]);
                    }
                }
            }
        }
    });
}

fn cmd_agent() -> Result<()> {
    logging::init();
    logging::log("[agent] Starting background agent");

    let _agent_lock = match acquire_agent_lock() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            logging::log("[agent] Another agent is already running; exiting");
            return Ok(());
        }
        Err(e) => {
            logging::log(&format!("[agent] Failed to acquire lock: {}", e));
            return Ok(());
        }
    };

    unsafe {
        let _: objc_utils::Id = msg_send![class!(NSApplication), sharedApplication];
    }
    ensure_running_from_applications();

    run_onboarding_if_needed(true);

    // Load settings and initialize sleep prevention state
    let app_settings = settings::AppSettings::load();
    MANUAL_SLEEP_PREVENTION.store(app_settings.sleep_prevention.enabled, Ordering::SeqCst);
    logging::log(&format!(
        "[agent] Loaded settings: sleep_prevention={}",
        app_settings.sleep_prevention.enabled
    ));

    start_clamshell_notifications();
    notifications::init_click_handler();

    let mut dictation_manager = DictationManager::new();
    let dictation_available = dictation_manager.is_available();
    let dictation_enabled = dictation_manager.is_enabled();
    logging::log(&format!(
        "[agent] dictation available={}, enabled={}",
        dictation_available, dictation_enabled
    ));
    if dictation_available {
        if let Err(e) = dictation_manager.start() {
            logging::log(&format!("[agent] Failed to start dictation: {}", e));
        }
    }

    let mut tick_counter = 0u64;

    loop {
        dictation_manager.update();
        objc_utils::pump_run_loop_once();
        std::thread::sleep(Duration::from_millis(50));
        tick_counter += 1;

        // Every 1s: cleanup stale PIDs, sync sleep state, post notifications
        if tick_counter % 20 == 0 {
            cleanup_stale_pids();
            menubar_sync_sleep();
            notifications::drain_and_post();
        }

        // Every 3s: thermal check + lid close
        if tick_counter % 60 == 0 {
            if check_thermal_warning() {
                let _ = set_sleep_disabled(false);
                logging::log("[agent][thermal] Sleep re-enabled due to thermal warning");
            }

            if LID_JUST_CLOSED.swap(false, Ordering::SeqCst) {
                let active = count_active_pids();
                if active > 0 {
                    play_lid_close_sound();
                }
            }
        }

        // Every 30s: reload settings from disk
        if tick_counter % 600 == 0 {
            let new_settings = settings::AppSettings::load();
            MANUAL_SLEEP_PREVENTION.store(new_settings.sleep_prevention.enabled, Ordering::SeqCst);
        }
    }
}

fn acquire_agent_lock() -> Result<Option<std::fs::File>> {
    let lock_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("AgentsSleepPreventer");
    fs::create_dir_all(&lock_dir)?;
    let lock_path = lock_dir.join("agent.lock");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    let fd = file.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Ok(None);
    }
    let _ = file.set_len(0);
    let _ = writeln!(file, "pid={}", std::process::id());
    Ok(Some(file))
}

fn ask_yes_no(prompt: &str) -> bool {
    use std::io::{self, BufRead};
    print!("{} [Y/n]: ", prompt);
    let _ = io::Write::flush(&mut io::stdout());
    let stdin = io::stdin();
    let answer = stdin
        .lock()
        .lines()
        .next()
        .and_then(|l| l.ok())
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    answer.is_empty() || answer == "y" || answer == "yes"
}

fn sync_installed_cli() -> Result<bool> {
    let current_exe = std::env::current_exe().context("Could not find current executable")?;
    fs::create_dir_all("/usr/local/bin")?;

    let mut updated = false;
    for target in [
        Path::new("/usr/local/bin/asp"),
        Path::new("/usr/local/bin/agents-sleep-preventer"),
    ] {
        if current_exe == target {
            continue;
        }

        fs::copy(&current_exe, target).with_context(|| {
            format!(
                "Failed to copy {} to {}",
                current_exe.display(),
                target.display()
            )
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
        }
        updated = true;
    }

    let _ = fs::remove_file("/usr/local/bin/claude-sleep-preventer");

    Ok(updated)
}

fn cmd_install(auto_yes: bool) -> Result<()> {
    let home = resolve_user_home()?;
    let hooks_dir = home.join(".claude").join("hooks");
    let settings_file = home.join(".claude").join("settings.json");
    let launch_agents_dir = home.join("Library/LaunchAgents");

    match sync_installed_cli() {
        Ok(true) => println!("Updated /usr/local/bin/asp"),
        Ok(false) => {}
        Err(e) => eprintln!("Warning: could not update /usr/local/bin/asp: {}", e),
    }

    fs::create_dir_all(&hooks_dir)?;

    let prevent_script = format!(
        "#!/bin/bash\n[ -x \"{}\" ] && \"{}\" start 2>/dev/null || true\n",
        APP_BINARY_PATH, APP_BINARY_PATH
    );
    let allow_script = format!(
        "#!/bin/bash\n[ -x \"{}\" ] && \"{}\" stop 2>/dev/null || true\n",
        APP_BINARY_PATH, APP_BINARY_PATH
    );
    // Forwards the hook JSON (with the "message" field) from stdin.
    let attention_script = format!(
        "#!/bin/bash\n[ -x \"{}\" ] && cat | \"{}\" attention 2>/dev/null || true\n",
        APP_BINARY_PATH, APP_BINARY_PATH
    );

    fs::write(hooks_dir.join("prevent-sleep.sh"), prevent_script)?;
    fs::write(hooks_dir.join("allow-sleep.sh"), allow_script)?;
    fs::write(hooks_dir.join("agent-attention.sh"), attention_script)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for script in ["prevent-sleep.sh", "allow-sleep.sh", "agent-attention.sh"] {
            fs::set_permissions(hooks_dir.join(script), fs::Permissions::from_mode(0o755))?;
        }

        fix_user_ownership(&hooks_dir);
    }

    println!("Setting up passwordless sudo for pmset...");
    // Get the real user (not root) for sudoers entry
    let real_user = std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    let sudoers_content = format!("{} ALL=(ALL) NOPASSWD: /usr/bin/pmset\n", real_user);

    // Write directly if we're root, otherwise use sudo
    let is_root = unsafe { libc::geteuid() == 0 };
    let mut child = if is_root {
        Command::new("tee")
            .arg("/etc/sudoers.d/agents-pmset")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?
    } else {
        Command::new("sudo")
            .args(["tee", "/etc/sudoers.d/agents-pmset"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?
    };

    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(sudoers_content.as_bytes())?;
    }
    child.wait()?;

    if is_root {
        Command::new("chmod")
            .args(["440", "/etc/sudoers.d/agents-pmset"])
            .output()?;
        let _ = Command::new("rm")
            .args(["-f", "/etc/sudoers.d/claude-pmset"])
            .output();
    } else {
        Command::new("sudo")
            .args(["chmod", "440", "/etc/sudoers.d/agents-pmset"])
            .output()?;
        let _ = Command::new("sudo")
            .args(["rm", "-f", "/etc/sudoers.d/claude-pmset"])
            .output();
    }

    println!("Configuring coding agent hooks...");

    let prevent_path = hooks_dir.join("prevent-sleep.sh");
    let allow_path = hooks_dir.join("allow-sleep.sh");
    let attention_path = hooks_dir.join("agent-attention.sh");
    let hooks_json = format!(
        r#"{{
    "UserPromptSubmit": [{{ "hooks": [{{ "type": "command", "command": "{prevent}" }}] }}],
    "PreToolUse": [{{ "hooks": [{{ "type": "command", "command": "{prevent}" }}] }}],
    "PreCompact": [{{ "hooks": [{{ "type": "command", "command": "{prevent}" }}] }}],
    "Notification": [{{ "hooks": [{{ "type": "command", "command": "{attention}" }}] }}],
    "Stop": [{{ "hooks": [{{ "type": "command", "command": "{allow}" }}] }}]
}}"#,
        prevent = prevent_path.display(),
        allow = allow_path.display(),
        attention = attention_path.display(),
    );

    let hooks: serde_json::Value =
        serde_json::from_str(&hooks_json).context("Failed to parse hooks JSON")?;

    if settings_file.exists() {
        let content = fs::read_to_string(&settings_file)
            .with_context(|| format!("Failed to read {}", settings_file.display()))?;
        let mut json: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", settings_file.display()))?;
        json["hooks"] = hooks;
        fs::write(&settings_file, serde_json::to_string_pretty(&json)?)
            .with_context(|| format!("Failed to write {}", settings_file.display()))?;
        println!("  Updated {}", settings_file.display());
    } else {
        fs::create_dir_all(settings_file.parent().unwrap())?;
        let mut json = serde_json::json!({});
        json["hooks"] = hooks;
        fs::write(&settings_file, serde_json::to_string_pretty(&json)?)
            .with_context(|| format!("Failed to write {}", settings_file.display()))?;
        println!("  Created {}", settings_file.display());
    }

    #[cfg(unix)]
    fix_user_ownership(&settings_file);

    println!("Configuring CLI agent hooks...");
    install_codex_hooks(&home, APP_BINARY_PATH)?;

    println!("Configuring Hermes hooks...");
    let hermes_profiles = install_hermes_hooks(&home, APP_BINARY_PATH)?;
    if hermes_profiles == 0 {
        println!("  No Hermes config.yaml found; skipping Hermes hook configuration");
    }

    if is_root {
        Command::new("pmset").args(["-a", "sleep", "5"]).output()?;
        Command::new("pmset")
            .args(["-a", "disablesleep", "0"])
            .output()?;
    } else {
        Command::new("sudo")
            .args(["pmset", "-a", "sleep", "5"])
            .output()?;
        Command::new("sudo")
            .args(["pmset", "-a", "disablesleep", "0"])
            .output()?;
    }

    println!();
    if auto_yes || ask_yes_no("Launch menu bar app at login?") {
        fs::create_dir_all(&launch_agents_dir)?;

        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.charlontank.agents-sleep-preventer</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/open</string>
        <string>/Applications/AgentsSleepPreventer.app</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
</dict>
</plist>"#;

        let plist_path = launch_agents_dir.join("com.charlontank.agents-sleep-preventer.plist");
        fs::write(&plist_path, plist)?;

        println!("  Created LaunchAgent for login startup");
        println!("  Note: Copy AgentsSleepPreventer.app to /Applications");
    }

    println!("\n✅ Installation complete!");
    println!("\nRestart your coding agents to activate.");
    println!("\nCommands:");
    println!("  asp status   - Show current state");
    println!("  asp cleanup  - Clean up stale PIDs");
    println!("  asp reset    - Force enable sleep");
    println!("  asp menubar  - Run native menu bar");
    println!("  asp daemon   - Run background daemon");

    // Try to launch the app
    let _ = Command::new("open")
        .arg("/Applications/AgentsSleepPreventer.app")
        .spawn();

    Ok(())
}

fn cmd_uninstall(keep_model: bool, keep_hooks: bool, keep_data: bool) -> Result<()> {
    let home = resolve_user_home()?;
    let hooks_dir = home.join(".claude").join("hooks");
    let settings_file = home.join(".claude").join("settings.json");
    let launch_agents_dir = home.join("Library/LaunchAgents");

    // Remove hook scripts (unless keeping hooks)
    if !keep_hooks {
        let _ = fs::remove_file(hooks_dir.join("prevent-sleep.sh"));
        let _ = fs::remove_file(hooks_dir.join("allow-sleep.sh"));
        let _ = fs::remove_file(hooks_dir.join("agent-attention.sh"));

        // Remove hooks from settings.json
        if settings_file.exists() {
            if let Ok(content) = fs::read_to_string(&settings_file) {
                if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) {
                    if json.get("hooks").is_some() {
                        json.as_object_mut().unwrap().remove("hooks");
                        let _ =
                            fs::write(&settings_file, serde_json::to_string_pretty(&json).unwrap());
                        println!("Removed hooks from settings.json");
                    }
                }
            }
        }
        if remove_codex_hooks(&home)? {
            println!("Removed Codex hooks");
        }
        let hermes_removed = remove_hermes_hooks(&home)?;
        if hermes_removed > 0 {
            println!("Removed Hermes hook scripts");
        }
        println!("Removed coding agent hooks");
    }

    // Remove LaunchAgents
    for label in [
        "com.charlontank.agents-sleep-preventer.plist",
        "com.charlontank.claude-sleep-preventer.plist",
    ] {
        let plist_path = launch_agents_dir.join(label);
        if plist_path.exists() {
            let _ = Command::new("launchctl")
                .args(["unload", plist_path.to_str().unwrap()])
                .output();
            let _ = fs::remove_file(&plist_path);
            println!("Removed LaunchAgent {}", label);
        }
    }

    // Remove sudoers config
    for sudoers_path in ["/etc/sudoers.d/agents-pmset", "/etc/sudoers.d/claude-pmset"] {
        Command::new("sudo")
            .args(["rm", "-f", sudoers_path])
            .output()?;
    }

    // Remove PID tracking and notification spool directories
    let _ = fs::remove_dir_all(PIDS_DIR);
    let _ = fs::remove_dir_all(LEGACY_PIDS_DIR);
    let _ = fs::remove_dir_all("/tmp/asp_notifications");
    let _ = fs::remove_dir_all(ATTENTION_DIR);

    // Reset sleep settings
    Command::new("sudo")
        .args(["pmset", "-a", "disablesleep", "0"])
        .output()?;

    // Remove app data and preferences (unless keeping data)
    if !keep_data {
        for app_support in [
            home.join("Library/Application Support/AgentsSleepPreventer"),
            home.join("Library/Application Support/ClaudeSleepPreventer"),
        ] {
            if app_support.exists() {
                if keep_model {
                    if let Ok(entries) = fs::read_dir(&app_support) {
                        for entry in entries.flatten() {
                            let name = entry.file_name();
                            if name == std::ffi::OsStr::new("models") {
                                continue;
                            }
                            let path = entry.path();
                            if path.is_dir() {
                                let _ = fs::remove_dir_all(&path);
                            } else {
                                let _ = fs::remove_file(&path);
                            }
                        }
                    }
                    println!("Removed app data (kept Whisper model)");
                } else {
                    let _ = fs::remove_dir_all(&app_support);
                    println!("Removed app data and Whisper model");
                }
            }
        }

        // Remove logs
        let _ = fs::remove_dir_all(home.join("Library/Logs/AgentsSleepPreventer"));
        let _ = fs::remove_dir_all(home.join("Library/Logs/ClaudeSleepPreventer"));
        println!("Removed logs");
    }

    // Remove the app from /Applications
    Command::new("sudo")
        .args([
            "rm",
            "-rf",
            "/Applications/AgentsSleepPreventer.app",
            "/Applications/ClaudeSleepPreventer.app",
        ])
        .output()?;
    println!("Removed app from /Applications");

    let _ = fs::remove_file("/usr/local/bin/asp");
    let _ = fs::remove_file("/usr/local/bin/agents-sleep-preventer");
    let _ = fs::remove_file("/usr/local/bin/claude-sleep-preventer");

    println!("Uninstalled successfully");

    Ok(())
}

fn cmd_settings() -> Result<()> {
    logging::init();
    logging::log("[settings] Opening settings window");

    // Initialize NSApplication for GUI
    unsafe {
        let _: objc_utils::Id = msg_send![class!(NSApplication), sharedApplication];
    }

    if let Some(new_settings) = settings::window::show_settings() {
        logging::log(&format!(
            "[settings] Settings saved: sleep_enabled={}, language={}, model={}",
            new_settings.sleep_prevention.enabled,
            new_settings.speech_to_text.language,
            new_settings.speech_to_text.model
        ));
        // Offer to download the selected model if it isn't present yet.
        dictation::ensure_selected_model_downloaded();
    } else {
        logging::log("[settings] Settings cancelled");
    }

    Ok(())
}

fn cmd_debug() -> Result<()> {
    println!("sysinfo processes:");
    let sys = System::new_all();
    for (pid, proc) in sys.processes() {
        let name = proc.name().to_string_lossy();
        let lower = name.to_lowercase();
        if lower.contains("claude") || lower.contains("codex") || lower.contains("hermes") {
            println!("  PID {}: name={:?}", pid.as_u32(), name);
        }
    }

    println!("\nps command:");
    let output = Command::new("ps")
        .args(["-eo", "pid=,ppid=,comm=,args="])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if lower.contains("claude") || lower.contains("codex") || lower.contains("hermes") {
            println!("  {}", line.trim());
        }
    }

    println!("\nDetected agent PIDs:");
    for process in get_all_agent_processes() {
        let kind = match classify_agent_process(&process) {
            Some(AgentKind::Claude) => "claude",
            Some(AgentKind::Codex) => "codex",
            Some(AgentKind::Hermes) => "hermes",
            None => "unknown",
        };
        println!(
            "  PID {}: kind={}, ppid={}, comm={}, args={}",
            process.pid, kind, process.ppid, process.comm, process.args
        );
    }

    Ok(())
}
