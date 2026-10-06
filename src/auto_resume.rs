//! Auto-resume: when an agent's turn is cut off by a network drop or an API
//! error, type a "continue" prompt into its terminal once the API answers
//! again, instead of leaving the work stalled until the user comes back.
//!
//! Claude Code reports the failure through its StopFailure hook
//! (`asp interrupted`). Codex records it in its session log as a
//! `task_complete` event with an `error`; the resume thread reads the tail of
//! the logs of the sessions its daemon hooks reported. One record per agent
//! pid lives in the runtime dir.

use super::*;
use serde::Deserialize;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::process::Stdio;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Automatic resumes of one failure streak before giving up.
const MAX_ATTEMPTS: u32 = 3;
/// A resumed turn that fails again within this window continues the streak.
const STREAK_SECS: u64 = 30 * 60;
/// How long an interrupted agent keeps the Mac awake while waiting for the
/// network; past it the Mac may sleep and the resume happens after wake.
const KEEP_AWAKE_SECS: u64 = 30 * 60;
/// The API must answer for this long before resuming: on a train the
/// connection comes and goes, and resuming into the next tunnel wastes a try.
const STABLE_NETWORK_SECS: u64 = 20;
/// Waits before retrying an API (server-side) error, by attempt.
const SERVER_BACKOFF_SECS: [u64; 3] = [30, 120, 300];
/// Our own "continue" fires UserPromptSubmit; within this window it is not
/// the user taking over.
const OWN_PROMPT_GRACE_SECS: u64 = 120;
/// Failures found in a Codex log are only acted on while this fresh.
const FRESH_FAILURE_SECS: u64 = 60 * 60;
/// Records are forgotten after this long, whatever their state.
const RECORD_TTL_SECS: u64 = 12 * 60 * 60;
/// Don't type into the terminal the user is looking at while they type.
const USER_TYPING_GRACE_SECS: f64 = 15.0;
const TICK: Duration = Duration::from_secs(5);

const CLAUDE_API_URL: &str = "https://api.anthropic.com/";
const CODEX_API_URL: &str = "https://chatgpt.com/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        }
    }

    fn api_url(self) -> &'static str {
        match self {
            Agent::Claude => CLAUDE_API_URL,
            Agent::Codex => CODEX_API_URL,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Cause {
    /// The connection dropped: resume as soon as the API answers again.
    Network,
    /// The API answered with an error (overloaded, 5xx): back off first.
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Waiting,
    Resumed,
    /// Gave up after MAX_ATTEMPTS; kept so the same failure isn't re-handled.
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Interruption {
    agent: Agent,
    pid: u32,
    cause: Cause,
    message: String,
    failed_at: u64,
    attempts: u32,
    stage: Stage,
    #[serde(default)]
    resumed_at: Option<u64>,
    /// Codex turn id of the failure, so a log scan doesn't record it twice.
    #[serde(default)]
    failed_turn: Option<String>,
    /// Told the user to type "continue" themselves (unsupported terminal).
    #[serde(default)]
    manual_hint_sent: bool,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn clock(unix: u64) -> String {
    let time = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&time, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn enabled() -> bool {
    settings::AppSettings::load().auto_resume.enabled
}

fn record_path(pid: u32) -> PathBuf {
    runtime_dir::interruptions_dir().join(pid.to_string())
}

fn load(pid: u32) -> Option<Interruption> {
    serde_json::from_str(&fs::read_to_string(record_path(pid)).ok()?).ok()
}

fn load_all() -> Vec<Interruption> {
    fs::read_dir(runtime_dir::interruptions_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| serde_json::from_str(&fs::read_to_string(entry.path()).ok()?).ok())
        .collect()
}

fn save(record: &Interruption) {
    if fs::create_dir_all(runtime_dir::interruptions_dir()).is_ok() {
        if let Ok(json) = serde_json::to_string(record) {
            let _ = fs::write(record_path(record.pid), json);
        }
    }
}

fn remove(pid: u32) {
    let _ = fs::remove_file(record_path(pid));
}

fn project_name(pid: u32) -> String {
    get_process_cwd(pid)
        .map(|cwd| project_info_from_cwd(&cwd).project)
        .filter(|project| !project.is_empty())
        .unwrap_or_else(|| "its project".to_string())
}

/// Network-level failures, as Claude Code and Codex word them.
const NETWORK_PATTERNS: [&str; 20] = [
    "connection refused",
    "connection reset",
    "connection error",
    "connection closed",
    "connection failed",
    "econnrefused",
    "econnreset",
    "etimedout",
    "enotfound",
    "eai_again",
    "enetunreach",
    "ehostunreach",
    "socket hang up",
    "fetch failed",
    "network",
    "stream disconnected",
    "error sending request",
    "stopped arriving",
    "stalled",
    "timed out",
];

/// Server-side failures worth retrying after a pause.
const SERVER_PATTERNS: [&str; 7] = [
    "overloaded",
    "internal server error",
    "server error",
    "bad gateway",
    "service unavailable",
    "502",
    "503",
];

/// Whether an error message is worth an automatic resume, and how.
/// Usage limits, authentication and billing errors are left alone: typing
/// "continue" cannot fix them (Claude Code resumes after a usage limit
/// reset by itself).
fn classify_message(message: &str) -> Option<Cause> {
    let lower = message.to_lowercase();
    if ["usage limit", "rate limit", "quota", "unauthorized", "authentication", "billing"]
        .iter()
        .any(|pattern| lower.contains(pattern))
    {
        return None;
    }
    if NETWORK_PATTERNS.iter().any(|pattern| lower.contains(pattern)) {
        return Some(Cause::Network);
    }
    if SERVER_PATTERNS.iter().any(|pattern| lower.contains(pattern)) {
        return Some(Cause::Server);
    }
    None
}

/// Claude Code's StopFailure `error` type plus the error text it shows.
fn classify_claude_failure(error: &str, message: &str) -> Option<Cause> {
    match error {
        "overloaded" => Some(Cause::Server),
        "server_error" => classify_message(message).or(Some(Cause::Server)),
        "unknown" => classify_message(message),
        _ => None,
    }
}

/// StopFailure hook: Claude Code's turn ended on an API error.
pub(super) fn record_claude_failure(pid: u32, hook: &serde_json::Value) {
    let error = hook.get("error").and_then(|value| value.as_str()).unwrap_or("");
    let message = ["last_assistant_message", "error_details"]
        .iter()
        .find_map(|field| hook.get(*field).and_then(|value| value.as_str()))
        .unwrap_or(error)
        .to_string();
    match classify_claude_failure(error, &message) {
        Some(cause) => record_failure(Agent::Claude, pid, cause, message, None),
        None => logging::log(&format!(
            "[auto-resume] Claude pid {} stopped on {} ({}): not resumable",
            pid, error, message
        )),
    }
}

fn record_failure(agent: Agent, pid: u32, cause: Cause, message: String, failed_turn: Option<String>) {
    if !enabled() {
        return;
    }
    let now = now_secs();
    let attempts = load(pid)
        .filter(|previous| {
            previous.stage == Stage::Resumed
                && now.saturating_sub(previous.resumed_at.unwrap_or(0)) < STREAK_SECS
        })
        .map(|previous| previous.attempts)
        .unwrap_or(0);
    let mut record = Interruption {
        agent,
        pid,
        cause,
        message,
        failed_at: now,
        attempts,
        stage: Stage::Waiting,
        resumed_at: None,
        failed_turn,
        manual_hint_sent: false,
    };
    if attempts >= MAX_ATTEMPTS {
        record.stage = Stage::Done;
        notifications::spool(
            &format!("{} could not be resumed", agent.name()),
            &format!(
                "{} still failed after {} automatic retries: {}",
                project_name(pid),
                attempts,
                record.message
            ),
            Some(pid),
        );
    }
    logging::log(&format!(
        "[auto-resume] {} pid {} cut off ({:?}, attempt {}): {}",
        agent.name(),
        pid,
        cause,
        attempts,
        record.message
    ));
    save(&record);
}

/// UserPromptSubmit: a prompt the user typed means they took over.
pub(super) fn turn_started(pid: u32) {
    let Some(record) = load(pid) else {
        return;
    };
    let own_prompt = record.stage == Stage::Resumed
        && now_secs().saturating_sub(record.resumed_at.unwrap_or(0)) < OWN_PROMPT_GRACE_SECS;
    if !own_prompt {
        remove(pid);
    }
}

/// Stop: Claude Code only fires it for turns that ended normally (failures
/// fire StopFailure), so the streak is over. Codex outcomes come from its log.
pub(super) fn turn_finished(pid: u32) {
    if load(pid).is_some_and(|record| record.agent == Agent::Claude) {
        remove(pid);
    }
}

fn keeps_awake(record: &Interruption, now: u64) -> bool {
    record.stage != Stage::Done && now.saturating_sub(record.failed_at) < KEEP_AWAKE_SECS
}

/// Interrupted agents keep the Mac awake for a while: the work isn't done,
/// and the resume needs the Mac up when the network comes back.
pub(super) fn keep_awake_count() -> usize {
    let now = now_secs();
    load_all()
        .iter()
        .filter(|record| {
            keeps_awake(record, now)
                && !get_pid_file(record.pid).exists()
                && is_process_alive(record.pid)
        })
        .count()
}

/// One line per interrupted agent for the menu.
pub(super) fn menu_notes() -> HashMap<u32, String> {
    let now = now_secs();
    load_all()
        .into_iter()
        .filter_map(|record| {
            let note = match (record.stage, record.cause) {
                (Stage::Waiting, Cause::Network) => format!(
                    "Cut off by a network drop at {} · resumes when the connection is back",
                    clock(record.failed_at)
                ),
                (Stage::Waiting, Cause::Server) => format!(
                    "API error at {} · retries at {}",
                    clock(record.failed_at),
                    clock(record.failed_at + server_backoff(record.attempts))
                ),
                (Stage::Resumed, _)
                    if now.saturating_sub(record.resumed_at.unwrap_or(0)) < OWN_PROMPT_GRACE_SECS =>
                {
                    format!(
                        "Resumed automatically after {}",
                        notifications::format_duration(
                            record.resumed_at.unwrap_or(now).saturating_sub(record.failed_at)
                        )
                    )
                }
                (Stage::Done, _) if now.saturating_sub(record.failed_at) < STREAK_SECS => format!(
                    "Not resumed: still failing after {} tries",
                    record.attempts
                ),
                _ => return None,
            };
            Some((record.pid, note))
        })
        .collect()
}

fn server_backoff(attempts: u32) -> u64 {
    SERVER_BACKOFF_SECS[(attempts as usize).min(SERVER_BACKOFF_SECS.len() - 1)]
}

/// What a Codex session's latest turn did, from the tail of its log.
#[derive(Debug, PartialEq)]
enum TurnOutcome {
    Running,
    Completed { at: u64 },
    Failed { turn: String, at: u64, message: String },
}

fn latest_codex_turn(log: &Path) -> Option<TurnOutcome> {
    const TAIL_BYTES: u64 = 256 * 1024;
    let mut file = fs::File::open(log).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES))).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    latest_codex_turn_in(&String::from_utf8_lossy(&bytes))
}

fn latest_codex_turn_in(log_tail: &str) -> Option<TurnOutcome> {
    for line in log_tail.lines().rev() {
        if !["\"task_started\"", "\"task_complete\"", "\"turn_aborted\""]
            .iter()
            .any(|kind| line.contains(kind))
        {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let payload = &event["payload"];
        let at = payload["completed_at"].as_u64().unwrap_or(0);
        match payload["type"].as_str() {
            Some("task_started") => return Some(TurnOutcome::Running),
            // Esc: the user stopped the turn on purpose.
            Some("turn_aborted") => return Some(TurnOutcome::Completed { at }),
            Some("task_complete") => {
                return Some(match payload["error"]["message"].as_str() {
                    Some(message) => TurnOutcome::Failed {
                        turn: payload["turn_id"].as_str().unwrap_or_default().to_string(),
                        at,
                        message: message.to_string(),
                    },
                    None => TurnOutcome::Completed { at },
                })
            }
            _ => continue,
        }
    }
    None
}

/// Codex sessions reported by daemon-run hooks: (pid, session log).
fn codex_sessions() -> Vec<(u32, PathBuf)> {
    fs::read_dir(runtime_dir::codex_sessions_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let content = fs::read_to_string(entry.path()).ok()?;
            let mut lines = content.lines();
            let pid = lines.next()?.trim().parse::<u32>().ok()?;
            let log = lines.next().map(str::trim).filter(|log| !log.is_empty())?;
            Some((pid, PathBuf::from(log)))
        })
        .collect()
}

fn scan_codex_sessions() {
    let now = now_secs();
    for (pid, log) in codex_sessions() {
        let previous = load(pid);
        match latest_codex_turn(&log) {
            Some(TurnOutcome::Failed { turn, at, message }) => {
                let known = previous
                    .as_ref()
                    .is_some_and(|record| record.failed_turn.as_deref() == Some(turn.as_str()));
                if known || now.saturating_sub(at) > FRESH_FAILURE_SECS {
                    continue;
                }
                if let Some(cause) = classify_message(&message) {
                    record_failure(Agent::Codex, pid, cause, message, Some(turn));
                }
            }
            Some(TurnOutcome::Completed { at }) => {
                // The resumed turn went through: the streak is over.
                if previous.is_some_and(|record| {
                    record.stage == Stage::Resumed && at >= record.resumed_at.unwrap_or(u64::MAX)
                }) {
                    remove(pid);
                }
            }
            _ => {}
        }
    }
}

/// Whether each API has answered continuously for a while.
#[derive(Default)]
struct NetworkWatch {
    reachable_since: HashMap<&'static str, u64>,
}

impl NetworkWatch {
    fn refresh(&mut self, urls: &HashSet<&'static str>, now: u64) {
        for &url in urls {
            if api_reachable(url) {
                self.reachable_since.entry(url).or_insert(now);
            } else {
                self.reachable_since.remove(url);
            }
        }
    }

    fn stable(&self, url: &str, now: u64) -> bool {
        self.reachable_since
            .get(url)
            .is_some_and(|&since| now.saturating_sub(since) >= STABLE_NETWORK_SECS)
    }
}

/// Any HTTP answer counts: a captive portal (train Wi-Fi) fails the TLS
/// handshake with the API host, so it does not.
fn api_reachable(url: &str) -> bool {
    Command::new("/usr/bin/curl")
        .args(["-sS", "-o", "/dev/null", "-m", "6", "-w", "%{http_code}", url])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|code| code.trim().parse::<u16>().ok())
        .is_some_and(|code| code > 0)
}

fn resume_prompt(record: &Interruption) -> String {
    let what = match record.cause {
        Cause::Network => "a network drop",
        Cause::Server => "an API error",
    };
    format!(
        "continue: your previous turn was cut off by {} at {} and Agents Sleep Preventer resumed you automatically. Check what was already done before redoing anything.",
        what,
        clock(record.failed_at)
    )
}

fn tick(network: &mut NetworkWatch) {
    if !enabled() {
        for record in load_all() {
            remove(record.pid);
        }
        return;
    }
    scan_codex_sessions();

    let now = now_secs();
    let mut waiting = Vec::new();
    for record in load_all() {
        let expired = match record.stage {
            Stage::Resumed => now.saturating_sub(record.resumed_at.unwrap_or(0)) > STREAK_SECS,
            _ => now.saturating_sub(record.failed_at) > RECORD_TTL_SECS,
        };
        if expired || !is_process_alive(record.pid) {
            remove(record.pid);
        } else if record.stage == Stage::Waiting {
            waiting.push(record);
        }
    }
    if waiting.is_empty() {
        return;
    }

    let urls = waiting.iter().map(|record| record.agent.api_url()).collect();
    network.refresh(&urls, now);

    for mut record in waiting {
        let pid = record.pid;
        // Working again (the user typed) or asking a question: hands off.
        if get_pid_file(pid).exists() || runtime_dir::attention_dir().join(pid.to_string()).exists() {
            continue;
        }
        if !network.stable(record.agent.api_url(), now) {
            continue;
        }
        let since_failure = now.saturating_sub(record.failed_at);
        let wait = match record.cause {
            Cause::Network => STABLE_NETWORK_SECS,
            Cause::Server => server_backoff(record.attempts),
        };
        if since_failure < wait {
            continue;
        }

        match deliver(pid, &resume_prompt(&record)) {
            Delivery::Sent(via) => {
                record.stage = Stage::Resumed;
                record.attempts += 1;
                record.resumed_at = Some(now);
                save(&record);
                logging::log(&format!(
                    "[auto-resume] Resumed {} pid {} via {} (attempt {})",
                    record.agent.name(),
                    pid,
                    via,
                    record.attempts
                ));
                notifications::spool(
                    &format!("{} resumed", record.agent.name()),
                    &format!(
                        "{}: continued after {} cut off.",
                        project_name(pid),
                        notifications::format_duration(since_failure)
                    ),
                    Some(pid),
                );
            }
            Delivery::Busy => {}
            Delivery::Unsupported => {
                if !record.manual_hint_sent {
                    record.manual_hint_sent = true;
                    save(&record);
                    notifications::spool(
                        &format!("{} can continue", record.agent.name()),
                        &format!(
                            "The connection is back. Type “continue” in {}: Agents Sleep Preventer can't type in this terminal.",
                            project_name(pid)
                        ),
                        Some(pid),
                    );
                }
            }
        }
    }
}

enum Delivery {
    Sent(&'static str),
    /// The user is typing in that very terminal: try again next tick.
    Busy,
    Unsupported,
}

fn deliver(pid: u32, text: &str) -> Delivery {
    let Some(tty) = get_process_tty(pid) else {
        return Delivery::Unsupported;
    };
    let tty = if tty.starts_with("/dev/") {
        tty
    } else {
        format!("/dev/{}", tty)
    };
    if tmux_send(&tty, text) {
        return Delivery::Sent("tmux");
    }
    let user_active = seconds_since_last_user_input() < USER_TYPING_GRACE_SECS;
    for (app, script) in [("iTerm2", ITERM_SCRIPT), ("Terminal", TERMINAL_SCRIPT)] {
        // `tell application` would launch an app that isn't running.
        if !app_running(app) {
            continue;
        }
        match run_osascript(script, &[&tty, text, if user_active { "1" } else { "0" }]).as_deref() {
            Some("sent") => return Delivery::Sent(app),
            Some("busy") => return Delivery::Busy,
            _ => {}
        }
    }
    Delivery::Unsupported
}

const ITERM_SCRIPT: &str = r#"on run argv
    set targetTty to item 1 of argv
    set message to item 2 of argv
    set userActive to (item 3 of argv) is "1"
    tell application "iTerm2"
        repeat with w in windows
            repeat with t in tabs of w
                repeat with s in sessions of t
                    if tty of s is targetTty then
                        if userActive and frontmost and (id of w is id of current window) and (unique id of s is unique id of (current session of w)) then return "busy"
                        tell s to write text message newline no
                        delay 0.3
                        tell s to write text ""
                        return "sent"
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    return "missing"
end run"#;

const TERMINAL_SCRIPT: &str = r#"on run argv
    set targetTty to item 1 of argv
    set message to item 2 of argv
    set userActive to (item 3 of argv) is "1"
    tell application "Terminal"
        repeat with w in windows
            repeat with t in tabs of w
                if tty of t is targetTty then
                    if userActive and frontmost and (selected of t) and (index of w is 1) then return "busy"
                    do script message in t
                    return "sent"
                end if
            end repeat
        end repeat
    end tell
    return "missing"
end run"#;

fn app_running(name: &str) -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-x", name])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// osascript can wait on the Automation consent prompt; never hang the
/// resume thread on it.
fn run_osascript(script: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                logging::log("[auto-resume] osascript timed out");
                return None;
            }
        }
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    if !stderr.trim().is_empty() {
        logging::log(&format!("[auto-resume] osascript: {}", stderr.trim()));
    }
    let mut stdout = String::new();
    child.stdout.take()?.read_to_string(&mut stdout).ok()?;
    Some(stdout.trim().to_string())
}

fn tmux_binary() -> Option<&'static str> {
    ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux", "/usr/bin/tmux"]
        .into_iter()
        .find(|path| Path::new(path).exists())
}

/// Agents inside tmux get the prompt through `send-keys` on their pane.
fn tmux_send(tty: &str, text: &str) -> bool {
    let Some(tmux) = tmux_binary() else {
        return false;
    };
    let Some(panes) = Command::new(tmux)
        .args(["list-panes", "-a", "-F", "#{pane_tty} #{pane_id}"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
    else {
        return false;
    };
    let Some(pane) = panes
        .lines()
        .find_map(|line| line.strip_prefix(tty)?.strip_prefix(' ').map(str::to_string))
    else {
        return false;
    };
    let sent = |keys: &[&str]| {
        Command::new(tmux)
            .args(["send-keys", "-t", &pane])
            .args(keys)
            .status()
            .is_ok_and(|status| status.success())
    };
    sent(&["-l", text]) && sent(&["Enter"])
}

pub(super) fn spawn() {
    let spawned = std::thread::Builder::new()
        .name("auto-resume".to_string())
        .spawn(|| {
            let mut network = NetworkWatch::default();
            loop {
                std::thread::sleep(TICK);
                tick(&mut network);
            }
        });
    if let Err(e) = spawned {
        logging::log(&format!("[auto-resume] Could not start: {}", e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_connection_errors_resume_at_once() {
        assert_eq!(
            classify_claude_failure(
                "server_error",
                "API Error: Connection refused — a firewall or proxy may be blocking it (ECONNREFUSED)"
            ),
            Some(Cause::Network)
        );
        assert_eq!(
            classify_claude_failure(
                "unknown",
                "API Error: The response stopped arriving. The response above may be incomplete."
            ),
            Some(Cause::Network)
        );
    }

    #[test]
    fn claude_server_errors_back_off_and_others_are_left_alone() {
        assert_eq!(classify_claude_failure("overloaded", "Overloaded"), Some(Cause::Server));
        assert_eq!(classify_claude_failure("server_error", "API Error: 500"), Some(Cause::Server));
        assert_eq!(classify_claude_failure("rate_limit", "Rate limit exceeded"), None);
        assert_eq!(classify_claude_failure("authentication_failed", "Invalid API key"), None);
        assert_eq!(classify_claude_failure("unknown", "Something odd"), None);
    }

    #[test]
    fn codex_messages() {
        assert_eq!(
            classify_message(
                "stream disconnected before completion: error sending request for url (https://chatgpt.com/backend-api/codex/responses)"
            ),
            Some(Cause::Network)
        );
        assert_eq!(classify_message("You've hit your usage limit."), None);
    }

    #[test]
    fn latest_codex_turn_reads_the_last_turn_event() {
        let failed = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"a"}}
{"type":"event_msg","payload":{"type":"task_complete","turn_id":"a","last_agent_message":null,"completed_at":100,"error":{"message":"stream disconnected before completion: error sending request"}}}"#;
        assert_eq!(
            latest_codex_turn_in(failed),
            Some(TurnOutcome::Failed {
                turn: "a".to_string(),
                at: 100,
                message: "stream disconnected before completion: error sending request".to_string(),
            })
        );

        let resumed = format!(
            "{}\n{}",
            failed, r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"b"}}"#
        );
        assert_eq!(latest_codex_turn_in(&resumed), Some(TurnOutcome::Running));

        let done = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"b","completed_at":200,"last_agent_message":"ok"}}"#;
        assert_eq!(latest_codex_turn_in(done), Some(TurnOutcome::Completed { at: 200 }));
        assert_eq!(latest_codex_turn_in("not json\n"), None);
    }

    #[test]
    fn server_backoff_grows_then_caps() {
        assert_eq!(server_backoff(0), 30);
        assert_eq!(server_backoff(1), 120);
        assert_eq!(server_backoff(7), 300);
    }
}
