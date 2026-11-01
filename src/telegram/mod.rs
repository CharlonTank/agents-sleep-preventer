//! `asp telegram`: the owner talks with their coding agent sessions through
//! their own Telegram bot. See `bot.rs` for how messages find their session.

mod api;
mod bot;
mod media;
mod setup;
mod store;
mod text;
#[cfg(test)]
mod tests;

use anyhow::{bail, Context, Result};
pub(crate) use bot::Bot;
use bot::{say, Busy};
pub(crate) use setup::system_prefers_french;
use clap::Subcommand;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use store::{Fields, Store};

#[derive(Subcommand)]
pub enum TelegramCommand {
    /// Connect your own bot: token, first message, confirmation
    Setup,
    /// Register this session and tell the owner
    Start {
        /// Session name (default: the git repository or folder)
        #[arg(long)]
        session: Option<String>,
        /// Short name accepted in "name:" (repeatable)
        #[arg(long)]
        alias: Vec<String>,
        /// Announce it in the bound project group
        #[arg(long)]
        announce_group: bool,
    },
    /// Wait for the owner's messages (exit 0: messages, 3: none within
    /// --max, 4: already listening)
    Listen {
        #[arg(long)]
        session: Option<String>,
        /// Seconds to listen at most (0: until the next message)
        #[arg(long, default_value_t = 1800)]
        max: u64,
    },
    /// Send a message or a file
    Send {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        text: Option<String>,
        /// Read the message from this file
        #[arg(long)]
        file: Option<PathBuf>,
        /// A file to send (PDF, image…); --text or --file is its caption
        #[arg(long)]
        document: Option<PathBuf>,
        /// To the owner privately, even if the session has a group
        #[arg(long)]
        private: bool,
    },
    /// Stop listening for this session
    Stop {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        announce_group: bool,
    },
    /// Project groups: a Telegram group bound to a session
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },
    /// Known sessions and their state
    Sessions,
    /// The last exchanges
    Read {
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Check the bot, listeners and groups
    Doctor,
    /// Install the /telegram skill in every Claude Code profile
    Skill,
}

#[derive(Subcommand)]
pub enum GroupCommand {
    /// Bind a group to a session (run again to add members)
    Add {
        /// Group id (-100…)
        #[arg(long, allow_hyphen_values = true)]
        chat: i64,
        #[arg(long)]
        session: String,
        /// ID=Name of a member who is listened to (repeatable)
        #[arg(long)]
        member: Vec<String>,
        #[arg(long)]
        title: Option<String>,
        /// Rebind a group bound to another session (its members are forgotten)
        #[arg(long)]
        force: bool,
    },
    List,
    /// Forget a group, or only one of its members
    Remove {
        #[arg(long, allow_hyphen_values = true)]
        chat: i64,
        #[arg(long)]
        member: Option<i64>,
    },
}

/// `[telegram]` in config.toml: where the bot keeps its state and its token.
#[derive(Deserialize, Default)]
struct Settings {
    #[serde(default)]
    telegram: TelegramSettings,
}

#[derive(Deserialize, Default)]
struct TelegramSettings {
    data_dir: Option<String>,
    keychain_service: Option<String>,
}

const DEFAULT_KEYCHAIN_SERVICE: &str = "agents-sleep-preventer-telegram";
const SKILL: &str = include_str!("SKILL.md");
const SKILL_MARKER: &str = "Installed by Agents Sleep Preventer";

/// Installs the /telegram skill in every Claude Code profile. A telegram
/// skill the user wrote themselves is left alone. Returns (file, written).
pub fn install_skill(owner: &str, bot: &str) -> Vec<(PathBuf, bool)> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let text = SKILL.replace("{{OWNER}}", owner).replace("{{BOT}}", bot);
    let mut done = Vec::new();
    for profile in crate::claude_config_dirs(&home) {
        let file = profile.join("skills").join("telegram").join("SKILL.md");
        let theirs = std::fs::read_to_string(&file).is_ok_and(|current| !current.contains(SKILL_MARKER));
        let written = !theirs
            && std::fs::create_dir_all(file.parent().unwrap_or(&profile)).is_ok()
            && std::fs::write(&file, &text).is_ok();
        done.push((file, written));
    }
    done
}

fn app_support() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("AgentsSleepPreventer")
}

/// config.toml, or the file `ASP_CONFIG` names.
pub fn config_file() -> PathBuf {
    std::env::var_os("ASP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| app_support().join("config.toml"))
}

fn expand(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

fn settings() -> Result<TelegramSettings> {
    let file = config_file();
    match std::fs::read_to_string(&file) {
        Ok(text) => Ok(toml::from_str::<Settings>(&text)
            .with_context(|| format!("{}", file.display()))?
            .telegram),
        Err(_) => Ok(TelegramSettings::default()),
    }
}

fn data_dir(settings: &TelegramSettings) -> PathBuf {
    settings
        .data_dir
        .as_deref()
        .map(expand)
        .unwrap_or_else(|| app_support().join("telegram"))
}

fn keychain_service(settings: &TelegramSettings) -> String {
    settings
        .keychain_service
        .clone()
        .unwrap_or_else(|| DEFAULT_KEYCHAIN_SERVICE.to_string())
}

fn token(service: &str) -> Result<String> {
    let output = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-w"])
        .output()
        .context("security")?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || token.is_empty() {
        bail!("No bot token in the Keychain (service {service}): run `asp telegram setup`");
    }
    Ok(token)
}

/// A running listener: its pid and session.
type Listener = (u32, String);

/// `asp telegram listen` processes started before the installed binary,
/// and listeners of the Python sync-telegram still running.
fn running_listeners(code: &Path) -> (Vec<Listener>, Vec<Listener>) {
    let built = std::fs::metadata(code)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |m| m.as_secs_f64());
    let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,etime=,command="])
        .output()
    else {
        return (Vec::new(), Vec::new());
    };
    let session_of = |command: &str| {
        let mut words = command.split_whitespace();
        while let Some(word) = words.next() {
            if word == "--session" {
                return words.next().unwrap_or("?").to_string();
            }
            if let Some(name) = word.strip_prefix("--session=") {
                return name.to_string();
            }
        }
        "?".to_string()
    };
    let at = store::now();
    let (mut old, mut python) = (Vec::new(), Vec::new());
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(elapsed)) = (parts.next(), parts.next()) else {
            continue;
        };
        let command = parts.collect::<Vec<_>>().join(" ");
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if command.contains("sync_telegram.cli bot listen") {
            python.push((pid, session_of(&command)));
        } else if command.contains("telegram listen") && command.contains("asp") {
            if let Some(seconds) = elapsed_seconds(elapsed) {
                if at - seconds < built {
                    old.push((pid, session_of(&command)));
                }
            }
        }
    }
    (old, python)
}

/// "[[dd-]hh:]mm:ss" (ps etime) in seconds.
fn elapsed_seconds(text: &str) -> Option<f64> {
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.parse::<f64>().ok()?, rest),
        None => (0.0, text),
    };
    let mut seconds = 0.0;
    for part in rest.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(seconds + days * 86400.0)
}

fn open_bot() -> Result<Bot> {
    let settings = settings()?;
    let store = Store::new(data_dir(&settings));
    let tg = api::HttpTelegram::new(token(&keychain_service(&settings))?);
    let code = std::env::current_exe().unwrap_or_default();
    Ok(Bot::new(
        store,
        Box::new(tg),
        Box::new(move || running_listeners(&code).0),
    ))
}

/// The given name, else the git repository's, else the folder's.
fn session_name(given: Option<&str>) -> Result<String> {
    let name = match given {
        Some(name) => name.to_string(),
        None => {
            let folder = std::env::current_dir().unwrap_or_default();
            Command::new("git")
                .arg("-C")
                .arg(&folder)
                .args(["rev-parse", "--show-toplevel"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| {
                    let top = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    Path::new(&top).file_name().map(|n| n.to_string_lossy().into_owned())
                })
                .or_else(|| folder.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default()
        }
    };
    let normal = text::normal(&name);
    if normal.is_empty() {
        bail!("Unreadable session name: {name:?} (give it with --session)");
    }
    Ok(normal)
}

/// The configured bot, once `asp telegram setup` is done.
pub(crate) fn configured_bot() -> Option<Bot> {
    open_bot().ok().filter(|bot| bot.config().is_ok())
}

/// The language the owner reads: the bot's when it is set up, else the Mac's.
pub(crate) fn owner_prefers_french() -> bool {
    settings()
        .ok()
        .map(|settings| Store::new(data_dir(&settings)))
        .and_then(|store| store.read_object(&store.path(&["bot.json"])))
        .and_then(|bot| bot.text("language").map(|language| language == "fr"))
        .unwrap_or_else(system_prefers_french)
}

/// Runs a telegram command; returns the process exit code.
pub fn run(command: TelegramCommand) -> i32 {
    match execute(command) {
        Ok(code) => code,
        Err(error) => {
            if let Some(busy) = error.downcast_ref::<Busy>() {
                eprintln!("asp telegram: {busy}");
                return 4;
            }
            eprintln!("asp telegram: {error:#}");
            2
        }
    }
}

fn execute(command: TelegramCommand) -> Result<i32> {
    if let TelegramCommand::Setup = command {
        let settings = settings()?;
        return setup::run(&data_dir(&settings), &keychain_service(&settings)).map(|()| 0);
    }
    let bot = open_bot()?;
    let owner = bot.owner();
    match command {
        TelegramCommand::Setup => unreachable!(),
        TelegramCommand::Start { session, alias, announce_group } => {
            let name = session_name(session.as_deref())?;
            bot.start(&name, &alias, announce_group)?;
            let told = match bot.group_of(&name) {
                Some((_, group)) if announce_group => {
                    let title = group.text("title").unwrap_or("").to_string();
                    say!(bot, "dans le groupe « {title} »", "in the group \"{title}\"")
                }
                _ => say!(bot, "à {owner}", "to {owner}"),
            };
            println!("{}", say!(
                bot,
                "Session « {name} » annoncée {told}.\nÉcoute (en tâche de fond) : asp telegram listen --session {name} --max 0\nRéponse : asp telegram send --session {name} --text \"…\"",
                "Session \"{name}\" announced {told}.\nListen (in the background): asp telegram listen --session {name} --max 0\nAnswer: asp telegram send --session {name} --text \"…\""
            ));
            Ok(0)
        }
        TelegramCommand::Listen { session, max } => {
            let name = session_name(session.as_deref())?;
            let (received, ignored) = bot.listen(&name, max)?;
            if ignored > 0 {
                println!("{}", say!(bot, "({ignored} message(s) d’un autre compte ignoré(s))", "({ignored} message(s) from another account ignored)"));
            }
            let bound = bot.group_of(&name).is_some();
            for entry in &received {
                // A session bound to a group: what doesn't come from it is
                // marked [privé] (answer with --private).
                let origin = match entry.text("group") {
                    Some(group) => say!(bot, "[groupe {group}] ", "[group {group}] "),
                    None if bound => say!(bot, "[privé] ", "[private] "),
                    None => String::new(),
                };
                let speaker = entry.text("speaker").map_or(owner.clone(), str::to_string);
                let when = text::day_and_time(entry.float("at"));
                let said = entry.text("text").unwrap_or("");
                println!("{}", say!(bot, "[{when}] {origin}{speaker} : {said}", "[{when}] {origin}{speaker}: {said}"));
            }
            Ok(if received.is_empty() { 3 } else { 0 })
        }
        TelegramCommand::Send { session, text: message, file, document, private } => {
            let name = session_name(session.as_deref())?;
            let message = match file {
                Some(file) => Some(std::fs::read_to_string(&file).with_context(|| file.display().to_string())?),
                None => message,
            };
            let place = match bot.group_of(&name) {
                Some((_, group)) if !private => {
                    let title = group.text("title").unwrap_or("").to_string();
                    say!(bot, "dans le groupe « {title} »", "in the group \"{title}\"")
                }
                _ => say!(bot, "à {owner}", "to {owner}"),
            };
            if let Some(document) = document {
                bot.send_document(&document, &name, message.as_deref().unwrap_or(""), private)?;
                let shown = document.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                println!("{}", say!(bot, "Fichier envoyé {place} ({name}) : {shown}", "File sent {place} ({name}): {shown}"));
                return Ok(0);
            }
            let Some(message) = message.filter(|m| !m.trim().is_empty()) else {
                bail!(say!(bot, "Rien à envoyer : --text ou --file", "Nothing to send: --text or --file"));
            };
            let count = bot.send(&message, &name, private)?.len();
            println!("{}", say!(bot, "Envoyé {place} ({name}) : {count} message(s).", "Sent {place} ({name}): {count} message(s)."));
            Ok(0)
        }
        TelegramCommand::Stop { session, announce_group } => {
            let name = session_name(session.as_deref())?;
            bot.stop(&name, announce_group)?;
            println!("{}", say!(bot, "Session « {name} » arrêtée (arrêter aussi sa tâche d’écoute).", "Session \"{name}\" stopped (stop its listening task too)."));
            Ok(0)
        }
        TelegramCommand::Skill => {
            let settings = bot.config()?;
            let username = settings.text("username").unwrap_or("").to_string();
            for (file, written) in install_skill(&owner, &username) {
                let shown = file.display();
                if written {
                    println!("{}", say!(bot, "Skill installé : {shown}", "Skill installed: {shown}"));
                } else {
                    println!("{}", say!(bot, "Laissé tel quel (skill à toi) : {shown}", "Left as is (your own skill): {shown}"));
                }
            }
            Ok(0)
        }
        TelegramCommand::Group { command } => group(&bot, command),
        TelegramCommand::Sessions => {
            sessions(&bot);
            Ok(0)
        }
        TelegramCommand::Read { session, limit } => {
            let name = session.as_deref().map(|s| session_name(Some(s))).transpose()?;
            read(&bot, name.as_deref(), limit);
            Ok(0)
        }
        TelegramCommand::Doctor => {
            for line in bot.doctor()? {
                println!("{line}");
            }
            let (_, python) = running_listeners(Path::new(""));
            if !python.is_empty() {
                let shown = python
                    .iter()
                    .map(|(pid, session)| format!("{session} (pid {pid})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("{}", say!(bot, "Écoutes sync-telegram (Python) qui partagent ce dossier : {shown}", "sync-telegram (Python) listeners sharing this folder: {shown}"));
            }
            Ok(0)
        }
    }
}

fn group(bot: &Bot, command: GroupCommand) -> Result<i32> {
    match command {
        GroupCommand::Add { chat, session, member, title, force } => {
            let mut members = BTreeMap::new();
            for item in &member {
                let parsed = item
                    .split_once('=')
                    .and_then(|(uid, who)| Some((uid.trim().parse::<i64>().ok()?, who.trim())))
                    .filter(|(uid, who)| *uid > 0 && !who.is_empty());
                let Some((uid, who)) = parsed else {
                    bail!(say!(bot, "--member {item:?} : attendu ID=Nom, ID positif (par exemple 123456789=Alice)", "--member {item:?}: expected ID=Name, positive ID (for example 123456789=Alice)"));
                };
                members.insert(uid, who.to_string());
            }
            let data = bot.group_add(chat, &session, &members, title.as_deref(), force)?;
            let id = data.int("chat").unwrap_or(chat);
            let shown_members = data
                .object("members")
                .map(|m| {
                    m.iter()
                        .map(|(uid, who)| format!("{} ({uid})", who.as_str().unwrap_or("")))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| say!(bot, "aucun", "none"));
            let title = data.text("title").unwrap_or("");
            let bound = data.text("session").unwrap_or("");
            let moved = if id != chat {
                say!(bot, " (le groupe {chat} est devenu le supergroupe {id})", " (group {chat} became supergroup {id})")
            } else {
                String::new()
            };
            println!("{}", say!(bot, "Groupe {id} « {title} » → session {bound} ; membres : {shown_members}{moved}", "Group {id} \"{title}\" → session {bound}; members: {shown_members}{moved}"));
            warn_old_listeners(bot);
            Ok(0)
        }
        GroupCommand::Remove { chat, member } => {
            bot.group_remove(chat, member)?;
            match member {
                Some(member) => println!("{}", say!(bot, "Groupe {chat} : membre {member} retiré.", "Group {chat}: member {member} removed.")),
                None => println!("{}", say!(bot, "Groupe {chat} : oublié.", "Group {chat}: forgotten.")),
            }
            Ok(0)
        }
        GroupCommand::List => {
            let known = bot.groups();
            if known.is_empty() {
                println!("{}", say!(bot, "Aucun groupe enregistré.", "No registered group."));
            }
            for (chat, data) in &known {
                let members = data
                    .object("members")
                    .map(|m| {
                        m.iter()
                            .map(|(uid, who)| format!("{} ({uid})", who.as_str().unwrap_or("")))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| say!(bot, "aucun", "none"));
                let title = data.text("title").unwrap_or("");
                let session = data.text("session").unwrap_or("");
                println!("{}", say!(bot, "{chat}  « {title} » → {session}  membres : {members}", "{chat}  \"{title}\" → {session}  members: {members}"));
            }
            let unknown = bot.unknown_groups();
            if !unknown.is_empty() {
                let shown = unknown
                    .iter()
                    .map(|(chat, title)| format!("{chat} « {title} »"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("{}", say!(bot, "Vus, non enregistrés : {shown}", "Seen, not registered: {shown}"));
            }
            Ok(0)
        }
    }
}

fn warn_old_listeners(bot: &Bot) {
    let old = bot.old_listeners();
    if old.is_empty() {
        return;
    }
    let shown = old
        .iter()
        .map(|(pid, session)| format!("{session} (pid {pid})"))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("{}", say!(
        bot,
        "Attention : {shown} écoute(nt) encore avec l’ancien code. Les relancer (un message à chacune suffit) avant d’écrire dans le groupe.",
        "Warning: {shown} still listen(s) with old code. Restart them (one message to each is enough) before writing in the group."
    ));
}

fn sessions(bot: &Bot) {
    let known = bot.sessions();
    if known.is_empty() {
        println!("{}", say!(bot, "Aucune session.", "No session."));
    }
    let at = store::now();
    let mut all: Vec<_> = known.values().collect();
    all.sort_by(|a, b| b.float("last_listen").partial_cmp(&a.float("last_listen")).unwrap_or(std::cmp::Ordering::Equal));
    for data in all {
        let name = data.text("name").unwrap_or("");
        let state = if bot.listening(name) {
            say!(bot, "à l’écoute", "listening")
        } else if data.flag("stopped") {
            say!(bot, "arrêtée", "stopped")
        } else if data.float("last_listen") > 0.0 {
            let when = text::stamp(data.float("last_listen"), at);
            say!(bot, "dernière écoute {when}", "last listened {when}")
        } else {
            say!(bot, "jamais", "never")
        };
        let mut extra = Vec::new();
        let aliases: Vec<&str> = data.list("aliases").iter().filter_map(|a| a.as_str()).collect();
        if !aliases.is_empty() {
            extra.push(format!("alias {}", aliases.join(", ")));
        }
        if let Some(thread) = data.int("thread_id") {
            extra.push(say!(bot, "fil {thread}", "topic {thread}"));
        }
        if let Some((_, group)) = bot.group_of(name) {
            let title = group.text("title").unwrap_or("");
            extra.push(say!(bot, "groupe « {title} »", "group \"{title}\""));
        }
        let waiting = bot.waiting(name);
        if waiting > 0 {
            extra.push(say!(bot, "{waiting} en attente", "{waiting} waiting"));
        }
        let extra = if extra.is_empty() { String::new() } else { format!("  ({})", extra.join("; ")) };
        println!("{name:28} {state:28} {}{extra}", data.text("cwd").unwrap_or(""));
    }
}

fn read(bot: &Bot, name: Option<&str>, limit: usize) {
    let entries: Vec<_> = bot
        .store
        .entries()
        .into_iter()
        .filter(|e| name.is_none() || e.text("session") == name)
        .collect();
    let shown = &entries[entries.len().saturating_sub(limit)..];
    if shown.is_empty() {
        println!("{}", say!(bot, "Aucun échange.", "No exchange."));
    }
    let owner = bot.owner();
    for entry in shown {
        let from = entry.text("from").unwrap_or("");
        let session = entry.text("session").filter(|s| !s.is_empty());
        let mut who = match from {
            _ if bot::from_owner(entry) => owner.clone(),
            "bot" => "Bot".to_string(),
            _ => session.unwrap_or("Claude").to_string(),
        };
        let mut target = match session {
            Some(session) if bot::from_owner(entry) => format!(" → {session}"),
            _ => String::new(),
        };
        if from == "groupe" {
            let speaker = entry.text("speaker").unwrap_or("");
            let group = entry.text("group").unwrap_or("");
            who = say!(bot, "{speaker} [groupe {group}]", "{speaker} [group {group}]");
            target = match session {
                Some(session) => format!(" → {session}"),
                None => {
                    let how = entry.text("how").unwrap_or("");
                    say!(bot, " (ignoré : {how})", " (ignored: {how})")
                }
            };
        } else if entry.int("chat_id").is_some_and(|c| c != 0) && matches!(from, "claude" | "bot") {
            let group = entry
                .text("group")
                .map(str::to_string)
                .unwrap_or_else(|| entry.int("chat_id").unwrap_or(0).to_string());
            target = say!(bot, " → groupe {group}", " → group {group}");
        }
        let when = text::day_and_time(entry.float("at"));
        let said = entry.text("text").unwrap_or("");
        println!("[{when}] {who}{target} : {said}");
    }
}
