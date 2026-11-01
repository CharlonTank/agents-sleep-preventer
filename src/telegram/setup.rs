//! `asp telegram setup`: connects the user's own bot in three steps — the
//! token from @BotFather, a first message from their Telegram account, a
//! confirmation — without any choice to make.

use super::api::{HttpTelegram, Params, Telegram};
use super::store::{now, Fields, Object, Store};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// How long to wait for the first message.
const FIRST_MESSAGE_WAIT: f64 = 600.0;

fn french() -> bool {
    let languages = Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let first = languages
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .find(|word| !word.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("LANG").ok())
        .unwrap_or_default();
    first.to_lowercase().starts_with("fr")
}

macro_rules! tr {
    ($french:expr, $fr:literal, $en:literal) => {
        if $french {
            format!($fr)
        } else {
            format!($en)
        }
    };
}

/// A line from the terminal, typed without echo (the token).
fn read_hidden() -> Result<String> {
    let fd = libc::STDIN_FILENO;
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let terminal = unsafe { libc::tcgetattr(fd, &mut saved) } == 0;
    if terminal {
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    }
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);
    if terminal {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
        println!();
    }
    read.context("stdin")?;
    Ok(line.trim().to_string())
}

fn read_line() -> Result<String> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).context("stdin")?;
    Ok(line.trim().to_string())
}

fn plausible_token(token: &str) -> bool {
    let Some((id, secret)) = token.split_once(':') else {
        return false;
    };
    !id.is_empty()
        && id.chars().all(|c| c.is_ascii_digit())
        && secret.len() >= 30
        && secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Saves the token in the Keychain through `security -i`, so it never
/// shows in the process list.
fn save_token(service: &str, token: &str) -> Result<()> {
    let mut child = Command::new("/usr/bin/security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("security")?;
    if let Some(stdin) = child.stdin.as_mut() {
        writeln!(stdin, "add-generic-password -U -s \"{service}\" -a asp -w \"{token}\"")?;
    }
    let output = child.wait_with_output()?;
    let refused = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() || refused.contains("rror") {
        bail!("security: {}", refused.trim());
    }
    Ok(())
}

/// The first private message from a person, after `offset`: (update, message).
fn first_message(tg: &HttpTelegram, mut offset: i64) -> Result<Option<(i64, Object)>> {
    let deadline = now() + FIRST_MESSAGE_WAIT;
    while now() < deadline {
        let updates = tg.call(
            "getUpdates",
            &Params::new()
                .set("offset", offset)
                .set("timeout", 25)
                .set("allowed_updates", r#"["message"]"#),
            Duration::from_secs(45),
            None,
        )?;
        for update in updates.as_array().cloned().unwrap_or_default() {
            let Value::Object(update) = update else {
                continue;
            };
            let id = update.int("update_id").unwrap_or(offset);
            offset = offset.max(id + 1);
            let Some(message) = update.object("message") else {
                continue;
            };
            let private = message.object("chat").and_then(|c| c.text("type")) == Some("private");
            let person = message.object("from").is_some_and(|f| !f.flag("is_bot"));
            if private && person {
                return Ok(Some((id, message.clone())));
            }
        }
    }
    Ok(None)
}

pub fn run(data_dir: &Path, service: &str) -> Result<()> {
    let fr = french();
    println!("{}", tr!(
        fr,
        "Connecter ton bot Telegram, en trois étapes.\n\n1. Dans Telegram, ouvre @BotFather (https://t.me/BotFather), envoie /newbot et suis ses deux questions (un nom, puis un identifiant qui finit par « bot »).\n2. Il te donne un jeton (123456789:AA…). Colle-le ici, il ne s’affiche pas :",
        "Connect your Telegram bot, in three steps.\n\n1. In Telegram, open @BotFather (https://t.me/BotFather), send /newbot and answer its two questions (a name, then a username ending in \"bot\").\n2. It gives you a token (123456789:AA…). Paste it here, it won't be shown:"
    ));
    let token = read_hidden()?;
    if !plausible_token(&token) {
        bail!(tr!(fr, "Ce n’est pas un jeton de bot (attendu : chiffres:lettres, 40 caractères environ).", "That isn't a bot token (expected: digits:letters, about 40 characters)."));
    }
    let tg = HttpTelegram::new(token.clone());
    let me = tg
        .call("getMe", &Params::new(), Duration::from_secs(30), None)
        .context("getMe")?;
    let username = me.get("username").and_then(Value::as_str).unwrap_or("").to_string();
    save_token(service, &token)?;
    // Updates sent before now are skipped: the first message must be new.
    let pending = tg.call("getUpdates", &Params::new().set("offset", -1).set("timeout", 0), Duration::from_secs(30), None)?;
    let offset = pending
        .as_array()
        .and_then(|updates| updates.last())
        .and_then(|update| update.get("update_id"))
        .and_then(Value::as_i64)
        .map_or(0, |id| id + 1);
    println!("{}", tr!(
        fr,
        "3. Jeton enregistré dans le Trousseau. Écris maintenant n’importe quel message à @{username} dans Telegram (https://t.me/{username})…",
        "3. Token saved in the Keychain. Now send any message to @{username} in Telegram (https://t.me/{username})…"
    ));
    let Some((update_id, message)) = first_message(&tg, offset)? else {
        bail!(tr!(fr, "Aucun message reçu en 10 minutes : relance asp telegram setup.", "No message within 10 minutes: run asp telegram setup again."));
    };
    let sender = message.object("from").cloned().unwrap_or_default();
    let first_name = sender.text("first_name").unwrap_or("").to_string();
    let said = message.text("text").unwrap_or("").to_string();
    print!("{}", tr!(
        fr,
        "Message de {first_name} : « {said} ». C’est bien toi ? [O/n] ",
        "Message from {first_name}: \"{said}\". Is that you? [Y/n] "
    ));
    io::stdout().flush()?;
    let answer = read_line()?.to_lowercase();
    if answer.starts_with('n') {
        bail!(tr!(fr, "Annulé : relance asp telegram setup et écris au bot depuis ton compte.", "Cancelled: run asp telegram setup again and write to the bot from your account."));
    }
    let store = Store::new(data_dir.to_path_buf());
    let file = store.path(&["bot.json"]);
    let mut settings = store.read_object(&file).unwrap_or_default();
    settings.extend(match json!({
        "user_id": sender.int("id"),
        "chat_id": message.object("chat").and_then(|c| c.int("id")),
        "name": first_name,
        "username": username,
        "language": if fr { "fr" } else { "en" },
    }) {
        Value::Object(object) => object,
        _ => Object::new(),
    });
    store.write(&file, &Value::Object(settings))?;
    std::fs::write(store.path(&["offset"]), (update_id + 1).to_string())?;
    let welcome = tr!(
        fr,
        "C’est branché. Dans une session Claude Code, tape /telegram : elle t’écoutera ici et te répondra.",
        "All set. In a Claude Code session, type /telegram: it will listen to you here and answer."
    );
    if let Some(chat) = message.object("chat").and_then(|c| c.int("id")) {
        let _ = tg.call(
            "sendMessage",
            &Params::new().set("chat_id", chat).set("text", &welcome),
            Duration::from_secs(30),
            None,
        );
    }
    println!("{welcome}");
    for (file, written) in super::install_skill(&first_name, &username) {
        if written {
            let shown = file.display();
            println!("{}", tr!(fr, "Skill /telegram installé : {shown}", "/telegram skill installed: {shown}"));
        }
    }
    Ok(())
}
