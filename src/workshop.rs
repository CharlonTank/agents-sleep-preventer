//! `asp workshop`: opens your named Claude Code sessions in iTerm in one go,
//! from `[workshop]` in config.toml: tabs of panes (folder, session name,
//! texts to type). A named session that exists is resumed by its id (a
//! duplicate name would open a picker), a new one is created under its
//! name. A detached pilot then answers the startup questions and types
//! each pane's texts once Claude is ready.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// How long the pilot waits for every Claude to be ready.
const PILOT_TIMEOUT: Duration = Duration::from_secs(240);
/// Only the bottom of the screen counts: the history above doesn't.
const SCREEN_TAIL: usize = 25;

#[derive(Deserialize, Clone)]
pub struct Pane {
    /// A folder, absolute or relative to `projects_dir`.
    pub dir: String,
    /// The Claude Code session name.
    pub session: String,
    /// Texts typed once Claude is ready, instead of `type`/`type_new`.
    #[serde(default, rename = "type")]
    pub texts: Option<Vec<String>>,
}

#[derive(Deserialize, Clone)]
pub struct Tab {
    pub panes: Vec<Pane>,
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub projects_dir: String,
    /// Typed into a resumed session.
    #[serde(rename = "type")]
    pub texts: Vec<String>,
    /// Typed into a new session.
    pub type_new: Vec<String>,
    /// Extra arguments for `claude` (e.g. "--dangerously-skip-permissions").
    pub claude_args: Vec<String>,
    /// For a heavy old session Claude asks how to resume: false takes its
    /// recommended "Resume from summary", true "Resume full session as-is".
    pub resume_full: bool,
    /// Claude Code profiles (CLAUDE_CONFIG_DIR) by short name; empty: the
    /// default profile only.
    pub profiles: BTreeMap<String, String>,
    pub tab: Vec<Tab>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            projects_dir: "~".to_string(),
            texts: vec!["/telegram".to_string(), "continue".to_string()],
            type_new: vec!["/telegram".to_string()],
            claude_args: Vec::new(),
            resume_full: false,
            profiles: BTreeMap::new(),
            tab: Vec::new(),
        }
    }
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    workshop: Settings,
}

fn expand(path: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    match path {
        "~" => home,
        _ => match path.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(path),
        },
    }
}

fn settings() -> Result<Settings> {
    let file = crate::telegram::config_file();
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let settings = toml::from_str::<ConfigFile>(&text)
        .with_context(|| file.display().to_string())?
        .workshop;
    if settings.tab.is_empty() {
        bail!(
            "No [[workshop.tab]] in {}. Example:\n\n[workshop]\nprojects_dir = \"~/projects\"\n\n[[workshop.tab]]\npanes = [{{ dir = \"my-app\", session = \"dev\" }}]",
            file.display()
        );
    }
    Ok(settings)
}

/// The profile folder: the one asked for, else the most recently used of
/// the configured ones, else CLAUDE_CONFIG_DIR or ~/.claude.
fn profile_dir(settings: &Settings, asked: Option<&str>) -> Result<(String, PathBuf)> {
    if let Some(name) = asked {
        let Some(dir) = settings.profiles.get(name) else {
            let known = settings.profiles.keys().cloned().collect::<Vec<_>>().join(", ");
            bail!("Unknown profile \"{name}\" (known: {known})");
        };
        return Ok((name.to_string(), expand(dir)));
    }
    let used = |dir: &Path| {
        std::fs::metadata(dir.join("history.jsonl"))
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    };
    if let Some((name, dir)) = settings
        .profiles
        .iter()
        .map(|(name, dir)| (name.clone(), expand(dir)))
        .max_by_key(|(_, dir)| used(dir))
    {
        return Ok((name, dir));
    }
    let default = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| expand("~/.claude"));
    Ok(("default".to_string(), default))
}

/// A session's current name: custom-title.json (/rename), else the last
/// custom-title line of its transcript.
fn current_title(transcript: &Path) -> Option<String> {
    let folder = transcript.with_extension("");
    if let Some(title) = std::fs::read_to_string(folder.join("custom-title.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| v.get("customTitle").and_then(|t| t.as_str()).map(str::to_string))
    {
        return Some(title);
    }
    let output = Command::new("/usr/bin/grep")
        .args(["-F", "\"type\":\"custom-title\""])
        .arg(transcript)
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()?
                .get("customTitle")?
                .as_str()
                .map(str::to_string)
        })
}

/// The id of the most recent session under this name in this folder.
fn find_session(profile: &Path, cwd: &Path, name: &str) -> Option<String> {
    let mangled: String = cwd
        .display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let project = profile.join("projects").join(mangled);
    if !project.is_dir() {
        return None;
    }
    let pattern = serde_json::json!({ "customTitle": name }).to_string();
    let pattern = &pattern[1..pattern.len() - 1];
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg("/usr/bin/grep -l -F -- \"$1\" *.jsonl */custom-title.json 2>/dev/null")
        .arg("sh")
        .arg(pattern)
        .current_dir(&project)
        .output()
        .ok()?;
    let mut candidates: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|file| {
            let id = match file.strip_suffix(".jsonl") {
                Some(id) => id,
                None => file.split('/').next()?,
            };
            let transcript = project.join(format!("{id}.jsonl"));
            transcript.exists().then_some(transcript)
        })
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates
        .into_iter()
        .filter(|transcript| current_title(transcript).as_deref() == Some(name))
        .max_by_key(|transcript| {
            std::fs::metadata(transcript)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        })
        .and_then(|transcript| transcript.file_stem().map(|s| s.to_string_lossy().into_owned()))
}

/// Workshop folders where a Claude already runs.
fn claude_running_in(folders: &[PathBuf]) -> Vec<String> {
    let mut found: Vec<String> = crate::load_process_table()
        .into_iter()
        .filter(|p| Path::new(&p.arg0).file_name().is_some_and(|n| n == "claude"))
        .filter_map(|p| crate::get_process_cwd(p.pid))
        .filter(|cwd| folders.iter().any(|f| Path::new(cwd) == f))
        .map(|cwd| Path::new(&cwd).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(cwd))
        .collect();
    found.sort();
    found.dedup();
    found
}

fn quote(word: &str) -> String {
    if word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@".contains(c)) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn claude_argv(settings: &Settings, name: &str, id: Option<&str>) -> Vec<String> {
    let mut argv = vec!["claude".to_string()];
    argv.extend(settings.claude_args.iter().cloned());
    match id {
        Some(id) => argv.extend(["--resume".to_string(), id.to_string()]),
        None => argv.extend(["-n".to_string(), name.to_string()]),
    }
    argv
}

fn osascript(script: &str, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .args(args)
        .output()
        .context("osascript")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end_matches('\n').to_string())
}

const FIND: &str = r#"
on findSession(uid)
  tell application "iTerm2"
    repeat with w in windows
      repeat with t in tabs of w
        repeat with s in sessions of t
          if unique id of s is uid then return s
        end repeat
      end repeat
    end repeat
  end tell
  return missing value
end findSession
"#;

const CONTENTS: &str = r#"
on run argv
  set s to findSession(item 1 of argv)
  if s is missing value then return "__GONE__"
  tell application "iTerm2" to return contents of s
end run
"#;

/// Text without a newline, then Enter (character 13) on its own: written
/// together, Claude Code takes the newline as part of the text.
const TYPE: &str = r#"
on run argv
  set s to findSession(item 1 of argv)
  if s is missing value then return "gone"
  tell application "iTerm2"
    if (item 2 of argv) is not "" then
      tell s to write text (item 2 of argv) without newline
      delay 0.5
    end if
    if (item 3 of argv) is "down" then
      tell s to write text ((character id 27) & "[B") without newline
    else
      tell s to write text (character id 13) without newline
    end if
  end tell
  return "ok"
end run
"#;

/// argv: mode (current|new), the current session's id, then one argument
/// per tab: its panes' commands separated by "|||" (empty for the current pane).
const LAYOUT: &str = r#"
on splitText(txt)
  set AppleScript's text item delimiters to "|||"
  set parts to text items of txt
  set AppleScript's text item delimiters to ""
  return parts
end splitText

on run argv
  set mode to item 1 of argv
  set s0 to findSession(item 2 of argv)
  set ids to {}
  tell application "iTerm2"
    if s0 is missing value then
      set w to current window
    else
      repeat with ww in windows
        repeat with t in tabs of ww
          repeat with s in sessions of t
            if unique id of s is (item 2 of argv) then
              set w to ww
              set t0 to t
            end if
          end repeat
        end repeat
      end repeat
    end if
    repeat with i from 3 to count of argv
      set cmds to my splitText(item i of argv)
      if i is 3 and mode is "current" and s0 is not missing value then
        set first_session to s0
      else
        tell w to set nt to (create tab with default profile)
        set first_session to current session of nt
      end if
      set prev to first_session
      repeat with j from 1 to count of cmds
        if j is 1 then
          set s to first_session
        else
          tell prev to set s to (split vertically with default profile)
          set prev to s
        end if
        if (item j of cmds) is not "" then tell s to write text (item j of cmds)
        set end of ids to (unique id of s)
      end repeat
    end repeat
    if s0 is not missing value then tell t0 to select
  end tell
  set AppleScript's text item delimiters to linefeed
  set r to ids as text
  set AppleScript's text item delimiters to ""
  return r
end run
"#;

#[derive(Serialize, Deserialize, Clone)]
pub struct Piloted {
    pub uid: String,
    pub name: String,
    pub texts: Vec<String>,
    pub resume_full: bool,
}

fn log(message: &str) {
    crate::logging::log(&format!("[workshop] {message}"));
}

/// Footers of a Claude Code waiting for input, whatever the permission mode.
const READY: [&str; 4] = ["? for shortcuts", "bypass permissions on", "accept edits on", "plan mode on"];

/// Waits for each Claude to be ready, answers its startup questions, then
/// types its texts. Runs detached.
pub fn pilot(plan: Vec<Piloted>) {
    crate::logging::init_quiet();
    log(&format!("pilot: {} sessions", plan.len()));
    let started = Instant::now();
    let mut waiting: Vec<Piloted> = plan.clone();
    let mut ready_since: BTreeMap<String, Instant> = BTreeMap::new();
    let mut downs: BTreeMap<String, u32> = BTreeMap::new();
    let mut report = Vec::new();
    let find_and = |script: &str| format!("{FIND}{script}");
    while !waiting.is_empty() && started.elapsed() < PILOT_TIMEOUT {
        let mut done = Vec::new();
        for pane in &waiting {
            let Ok(screen) = osascript(&find_and(CONTENTS), &[&pane.uid]) else {
                continue;
            };
            if screen == "__GONE__" {
                report.push(format!("{}: tab closed", pane.name));
                done.push(pane.uid.clone());
                continue;
            }
            let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
            let bottom = lines[lines.len().saturating_sub(SCREEN_TAIL)..].join("\n");
            if bottom.contains("Enter to confirm") {
                // The highlighted choice isn't always the right one (the
                // trust question starts on "No, exit"): go down to it.
                let questions = [
                    ("Yes, I trust this folder", "Yes, I trust this folder"),
                    ("Resume from summary", if pane.resume_full { "Resume full session as-is" } else { "Resume from summary" }),
                ];
                if let Some((sign, choice)) = questions.iter().find(|(sign, _)| bottom.contains(sign)) {
                    let highlighted = bottom
                        .lines()
                        .any(|line| line.trim_start().strip_prefix('❯').is_some_and(|rest| rest.trim_start().starts_with(choice)));
                    if highlighted {
                        let _ = osascript(&find_and(TYPE), &[&pane.uid, "", "enter"]);
                        log(&format!("{}: question \"{sign}\" → {choice}", pane.name));
                        if choice.starts_with("Resume") {
                            report.push(format!("{}: {choice}", pane.name));
                        }
                    } else if downs.get(&pane.uid).copied().unwrap_or(0) < 6 {
                        let _ = osascript(&find_and(TYPE), &[&pane.uid, "", "down"]);
                        *downs.entry(pane.uid.clone()).or_default() += 1;
                    }
                }
                continue;
            }
            let footer = lines[lines.len().saturating_sub(4)..].join("\n");
            if !READY.iter().any(|ready| footer.contains(ready)) {
                continue;
            }
            let since = *ready_since.entry(pane.uid.clone()).or_insert_with(Instant::now);
            if since.elapsed() < Duration::from_secs(2) {
                continue; // let the resumed session finish drawing
            }
            for text in &pane.texts {
                let _ = osascript(&find_and(TYPE), &[&pane.uid, text, "enter"]);
                std::thread::sleep(Duration::from_secs(2));
            }
            log(&format!("{}: typed {:?}", pane.name, pane.texts));
            done.push(pane.uid.clone());
        }
        waiting.retain(|pane| !done.contains(&pane.uid));
        std::thread::sleep(Duration::from_secs(1));
    }
    for pane in &waiting {
        report.push(format!("{}: not ready, type it by hand", pane.name));
    }
    let summary = if report.is_empty() {
        format!("{} session(s) started, texts sent", plan.len())
    } else {
        report.join("; ")
    };
    let _ = osascript(
        "on run argv\ndisplay notification (item 1 of argv) with title \"Workshop\"\nend run",
        &[&summary],
    );
    log(&format!("pilot: done — {summary}"));
}

/// `asp workshop [PROFILE] [--force] [--only SESSION]`.
pub fn open(profile: Option<&str>, force: bool, only: Option<&str>) -> Result<()> {
    let settings = settings()?;
    let (profile_name, profile) = profile_dir(&settings, profile)?;
    let projects = expand(&settings.projects_dir);
    let tabs: Vec<Tab> = settings
        .tab
        .iter()
        .filter(|tab| only.is_none_or(|name| tab.panes.iter().any(|p| p.session == name)))
        .cloned()
        .collect();
    if tabs.is_empty() {
        bail!("No tab holds session {:?}", only.unwrap_or(""));
    }
    let folder_of = |pane: &Pane| {
        let dir = expand(&pane.dir);
        if dir.is_absolute() { dir } else { projects.join(&pane.dir) }
    };
    if !force {
        let folders: Vec<PathBuf> = tabs.iter().flat_map(|t| t.panes.iter().map(folder_of)).collect();
        let running = claude_running_in(&folders);
        if !running.is_empty() {
            bail!(
                "Claude already runs in: {}\nThe workshop is probably open already (otherwise two Claude would share a session).\nasp workshop --force opens it anyway.",
                running.join(", ")
            );
        }
    }
    let current = std::env::var("ITERM_SESSION_ID")
        .ok()
        .and_then(|id| id.split_once(':').map(|(_, uid)| uid.to_string()))
        .unwrap_or_default();
    // --only opens its tab next to this one: the current tab is never turned into it.
    let mode = if only.is_some() || current.is_empty() { "new" } else { "current" };
    println!("Workshop with profile {profile_name} ({})", profile.display());
    let mut layout = Vec::new();
    let mut plan = Vec::new();
    let mut first: Option<(PathBuf, Vec<String>)> = None;
    for (i, tab) in tabs.iter().enumerate() {
        let mut commands = Vec::new();
        for (j, pane) in tab.panes.iter().enumerate() {
            let cwd = folder_of(pane);
            let id = find_session(&profile, &cwd, &pane.session);
            let texts = pane.texts.clone().unwrap_or_else(|| {
                if id.is_some() { settings.texts.clone() } else { settings.type_new.clone() }
            });
            println!("  {:16} {}{}", pane.session, pane.dir, if id.is_some() { "" } else { "   (new session)" });
            plan.push(Piloted { uid: String::new(), name: pane.session.clone(), texts, resume_full: settings.resume_full });
            let argv = claude_argv(&settings, &pane.session, id.as_deref());
            if i == 0 && j == 0 && mode == "current" {
                first = Some((cwd, argv));
                commands.push(String::new());
            } else {
                let command = argv.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ");
                commands.push(format!(
                    "cd {} && env CLAUDE_CONFIG_DIR={} {command}",
                    quote(&cwd.display().to_string()),
                    quote(&profile.display().to_string())
                ));
            }
        }
        layout.push(commands.join("|||"));
    }
    let mut args: Vec<&str> = vec![mode, &current];
    args.extend(layout.iter().map(String::as_str));
    let ids = osascript(&format!("{FIND}{LAYOUT}"), &args)?;
    for (pane, uid) in plan.iter_mut().zip(ids.lines()) {
        pane.uid = uid.to_string();
    }
    let exe = std::env::current_exe().context("current executable")?;
    {
        use std::os::unix::process::CommandExt;
        Command::new(exe)
            .args(["workshop-pilot", &serde_json::to_string(&plan)?])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .context("pilot")?;
    }
    if let Some((cwd, argv)) = first {
        // This tab becomes the workshop's first Claude.
        use std::os::unix::process::CommandExt;
        let error = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(cwd)
            .env("CLAUDE_CONFIG_DIR", &profile)
            .exec();
        bail!("claude: {error}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_reads_tabs_and_defaults() {
        let config: ConfigFile = toml::from_str(
            r#"
[workshop]
projects_dir = "~/projects"
claude_args = ["--dangerously-skip-permissions"]

[workshop.profiles]
cc = "~/.claude-cc"

[[workshop.tab]]
panes = [{ dir = "app", session = "dev" }, { dir = "app", session = "seo" }]

[[workshop.tab]]
panes = [{ dir = "/abs/tools", session = "moniteur", type = ["/telegram moniteur"] }]
"#,
        )
        .unwrap();
        let workshop = config.workshop;
        assert_eq!(workshop.tab.len(), 2);
        assert_eq!(workshop.tab[0].panes[1].session, "seo");
        assert_eq!(workshop.tab[1].panes[0].texts.as_deref(), Some(&["/telegram moniteur".to_string()][..]));
        assert_eq!(workshop.texts, ["/telegram", "continue"]);
        assert_eq!(claude_argv(&workshop, "dev", Some("abc")), ["claude", "--dangerously-skip-permissions", "--resume", "abc"]);
        assert_eq!(claude_argv(&workshop, "dev", None), ["claude", "--dangerously-skip-permissions", "-n", "dev"]);
    }

    #[test]
    fn a_session_is_found_by_its_current_name() {
        let profile = tempfile::tempdir().unwrap();
        let cwd = Path::new("/Users/x/projects/my-app");
        let project = profile.path().join("projects").join("-Users-x-projects-my-app");
        std::fs::create_dir_all(project.join("renamed")).unwrap();
        // Named "dev", then renamed: no longer "dev".
        std::fs::write(project.join("renamed.jsonl"), "{\"type\":\"custom-title\",\"customTitle\":\"dev\"}\n").unwrap();
        std::fs::write(project.join("renamed/custom-title.json"), "{\"customTitle\":\"autre\"}").unwrap();
        std::fs::write(project.join("good.jsonl"), "{\"type\":\"user\"}\n{\"type\":\"custom-title\",\"customTitle\":\"dev\"}\n").unwrap();
        assert_eq!(find_session(profile.path(), cwd, "dev").as_deref(), Some("good"));
        assert_eq!(find_session(profile.path(), cwd, "autre").as_deref(), Some("renamed"));
        assert_eq!(find_session(profile.path(), cwd, "none"), None);
    }

    #[test]
    fn shell_words_are_quoted() {
        assert_eq!(quote("/Users/x/my app"), "'/Users/x/my app'");
        assert_eq!(quote("--resume"), "--resume");
    }
}
