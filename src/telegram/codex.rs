//! Codex isn't woken when a background command ends, so a Codex session
//! can't keep `listen` running beside it as Claude Code does. Its Stop hook
//! listens instead: `asp telegram start`, run by Codex, binds the Codex
//! thread (`CODEX_THREAD_ID`) to the session, and at each end of turn
//! `asp telegram codex` waits for the owner's messages and hands them to
//! Codex as its next prompt (`{"decision": "block", "reason": …}`).

use super::bot::{say, Bot, Busy};
use super::store::{now, Fields, Object, Store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// How long the hook waits before letting Codex take a breath: a quiet
/// prompt an hour, and the hook's timeout stays well above it.
pub const WAIT: u64 = 3600;
const HOOK_TIMEOUT: u64 = 3900;
/// What tells the hook apart from the other Stop hooks in hooks.json.
const HOOK_MARKER: &str = " telegram codex";

const SKILL_MARKER: &str = "Installed by Agent Session Pilot";

/// The Codex thread running this command, if Codex runs it.
pub fn thread() -> Option<String> {
    std::env::var("CODEX_THREAD_ID").ok().filter(|thread| !thread.trim().is_empty())
}

fn file(store: &Store, thread: &str) -> PathBuf {
    let safe: String = thread
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    store.path(&["codex", &format!("{safe}.json")])
}

fn bindings(store: &Store) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(store.path(&["codex"])) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .filter_map(|path| {
            let name = store.read_object(&path)?.text("session")?.to_string();
            Some((path, name))
        })
        .collect()
}

/// This thread now listens as `name`, and no other thread does.
pub fn bind(store: &Store, thread: &str, name: &str) -> std::io::Result<()> {
    unbind(store, name);
    store.write(&file(store, thread), &json!({"session": name, "at": now()}))
}

pub fn unbind(store: &Store, name: &str) {
    for (path, bound) in bindings(store) {
        if bound == name {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn bound(store: &Store, thread: &str) -> Option<String> {
    store.read_object(&file(store, thread))?.text("session").map(str::to_string)
}

/// What the Stop hook prints for `input` (Codex's JSON on stdin): None lets
/// the turn end, as for every Codex session that doesn't listen.
pub fn on_stop(bot: &Bot, input: &str, wait: u64, heard: impl Fn(&str, &[Object]) -> Vec<String>) -> Option<Value> {
    let input: Value = serde_json::from_str(input).ok()?;
    let thread = input.get("session_id").and_then(Value::as_str)?;
    let name = bound(&bot.store, thread)?;
    if bot.sessions().get(&name).is_none_or(|data| data.flag("stopped")) {
        unbind(&bot.store, &name);
        return None;
    }
    let continued = input.get("stop_hook_active").and_then(Value::as_bool).unwrap_or(false);
    let owner = bot.owner();
    let night = bot.night(now()).is_some();
    let head = say!(bot, "[asp telegram, session « {name} »]", "[asp telegram, session \"{name}\"]");
    let reason = match bot.listen(&name, wait) {
        Ok((received, _)) if !received.is_empty() => {
            let lines = heard(&name, &received).join("\n");
            say!(
                bot,
                "{head} Messages de {owner} sur Telegram. Traite-les comme s’il les tapait ici, réponds-lui sur Telegram (asp telegram send --session {name} …), puis termine ton tour : l’écoute reprend seule.\n\n{lines}",
                "{head} Messages from {owner} on Telegram. Handle them as if typed here, answer on Telegram (asp telegram send --session {name} …), then end your turn: listening resumes by itself.\n\n{lines}"
            )
        }
        Ok(_) if night && bot.night(now()).is_none() => say!(
            bot,
            "{head} La nuit de {owner} est finie. Si des questions attendaient le matin, écris-lui un seul message : ce que tu as fait cette nuit, puis ces questions. Sinon rien à écrire. Puis termine ton tour : l’écoute reprend seule.",
            "{head} {owner}'s night is over. If questions were waiting for the morning, write them a single message: what you did during the night, then those questions. Otherwise nothing to write. Then end your turn: listening resumes by itself."
        ),
        Ok(_) => say!(
            bot,
            "{head} Pas de message de {owner} depuis une heure : rien à faire. Termine ton tour sans rien écrire, l’écoute reprend seule.",
            "{head} No message from {owner} for an hour: nothing to do. End your turn without writing anything, listening resumes by itself."
        ),
        // Said once; a second time in a row, the session stops listening
        // rather than wake Codex in a loop.
        Err(error) if continued => {
            eprintln!("asp telegram codex: {error:#}");
            return None;
        }
        Err(error) if error.is::<Busy>() => say!(
            bot,
            "{head} Une autre écoute tient déjà « {name} » : {error}. Si c’est un « asp telegram listen » lancé dans cette session, arrête-le : Codex n’est pas réveillé par sa fin, c’est le hook qui écoute pour toi. Sinon prends un autre nom (asp telegram start --session …). Puis termine ton tour.",
            "{head} Another listener already holds \"{name}\": {error}. If it is an \"asp telegram listen\" started in this session, stop it: Codex isn't woken when it ends, the hook listens for you. Otherwise take another name (asp telegram start --session …). Then end your turn."
        ),
        Err(error) => say!(
            bot,
            "{head} L’écoute a échoué : {error:#}. Lance asp telegram doctor, dis le problème dans le terminal, puis termine ton tour (l’écoute réessaiera).",
            "{head} Listening failed: {error:#}. Run asp telegram doctor, report the problem in the terminal, then end your turn (listening will try again)."
        ),
    };
    Some(json!({"decision": "block", "reason": reason}))
}

/// ~/.codex, when Codex is installed.
pub fn home() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))?;
    home.is_dir().then_some(home)
}

/// The binary the hook runs: the installed app rather than a build.
fn binary() -> PathBuf {
    let app = Path::new("/Applications/AgentsSleepPreventer.app/Contents/MacOS/asp");
    if cfg!(target_os = "macos") && app.exists() {
        return app.to_path_buf();
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("asp"))
}

/// The hook group for hooks.json; its text must stay the same from one
/// version to the next: Codex asks the user to trust it again when it
/// changes.
pub fn hook_group(binary: &Path) -> Value {
    let binary = binary.display();
    json!({"hooks": [{
        "type": "command",
        "command": format!("[ -x \"{binary}\" ] && \"{binary}\"{HOOK_MARKER} || true"),
        "timeout": HOOK_TIMEOUT,
    }]})
}

pub fn is_hook_group(group: &Value) -> bool {
    group.get("hooks").and_then(Value::as_array).is_some_and(|hooks| {
        hooks.iter().any(|hook| {
            hook.get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| command.contains(HOOK_MARKER))
        })
    })
}

/// Where the hook stands in Codex's configuration.
#[derive(Debug, PartialEq)]
pub enum Hook {
    /// Just added to hooks.json: Codex runs it once the user trusts it.
    Added,
    Untrusted,
    Trusted,
}

/// Adds the Stop hook to `codex/hooks.json` if it isn't there, and tells
/// whether Codex trusts it (config.toml `[hooks.state."<file>:stop:<i>:0"]`).
pub fn install_hook(codex: &Path) -> anyhow::Result<Hook> {
    let file = codex.join("hooks.json");
    let mut hooks: Value = match std::fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str(&text)?,
        Err(_) => json!({}),
    };
    if !hooks.get("hooks").is_some_and(Value::is_object) {
        hooks["hooks"] = json!({});
    }
    let stop = &mut hooks["hooks"]["Stop"];
    if !stop.is_array() {
        *stop = json!([]);
    }
    let groups = stop.as_array_mut().expect("Stop is an array");
    if let Some(index) = groups.iter().position(is_hook_group) {
        let key = format!("{}:stop:{index}:0", file.display());
        let trusted = std::fs::read_to_string(codex.join("config.toml"))
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .and_then(|config| {
                let state = config.get("hooks")?.get("state")?.get(&key)?;
                Some(state.get("trusted_hash").is_some() && state.get("enabled").and_then(toml::Value::as_bool) != Some(false))
            })
            .unwrap_or(false);
        return Ok(if trusted { Hook::Trusted } else { Hook::Untrusted });
    }
    groups.push(hook_group(&binary()));
    std::fs::write(&file, serde_json::to_string_pretty(&hooks)? + "\n")?;
    Ok(Hook::Added)
}

/// The Claude Code skill with what is about listening rewritten for Codex.
pub fn skill(claude: &str) -> String {
    let start = claude.find("## Start").expect("SKILL.md has ## Start");
    let answer = claude.find("## Answer").expect("SKILL.md has ## Answer");
    let stop = claude.find("## Stop").expect("SKILL.md has ## Stop");
    let mut text = format!("{}{START}\n{}{STOP}", &claude[..start], &claude[answer..stop]);
    text = text
        .lines()
        .filter(|line| !line.starts_with("- If {{OWNER}} asks for recaps"))
        .map(|line| {
            if line.starts_with("- What really needs {{OWNER}}'s approval") {
                NIGHT.to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    text.replace(
        "Use for \"/telegram\",",
        "Use for \"/telegram\", \"$telegram\",",
    ) + "\n"
}

const START: &str = "## Start

First time on this machine (`asp telegram` says `bot.json` is missing): ask {{OWNER}} to run `asp telegram setup` in a terminal. The token never goes through this conversation: if it shows up here anyway, don't use it, say so, and have {{OWNER}} revoke it in @BotFather (`/revoke`).

1. Session name: the skill's argument if there is one, else the git repository's name (or the folder's). Two sessions in one project: the second takes its own name (`$telegram my-project-2`). A short alias is quicker to type on a phone (`--alias short`).
2. `asp telegram start --session NAME [--alias SHORT]`: registers the session, announces it to {{OWNER}} and binds this Codex session to NAME. Exit code 4: a listener already runs under this name; pick another name.
3. Don't run `asp telegram listen` in the background: Codex isn't woken when a background command ends, so nobody would read what it gets. End your turn instead. Codex's Stop hook (`asp telegram codex`, added by `start`) then waits for {{OWNER}}'s messages and gives them to you as your next prompt.
4. If `start` says Codex doesn't trust the hook yet, tell {{OWNER}} in one line in the terminal: type `/hooks` in Codex, trust the `asp telegram codex` hook, then `$telegram` again. Until then nothing listens.
5. Before ending the turn, tell {{OWNER}} in the terminal, in one line, that the session listens on Telegram, and that to take the keyboard back they press Esc, then ask you to stop Telegram.

Always pass `--session NAME` afterwards, to send and to stop: without it the tool takes the current folder's name.

## Each time the hook wakes you

Its prompt starts with `[asp telegram, session \"NAME\"]` (`[asp telegram, session « NAME »]` in French):

- With {{OWNER}}'s messages (`[21/10 09:53] {{OWNER}}: …`): handle them as if typed in the terminal and answer on Telegram: {{OWNER}} isn't in front of the screen. Then end your turn: the hook listens again. A long task: one line on Telegram at the start, do the work, the result at the end, then end the turn.
- No message for an hour: end your turn without writing anything.
- The night is over: the morning message, if questions were waiting (see \"At night\").
- Another listener holds the name, or listening failed: do what the prompt says, then end your turn.

While the hook waits, the terminal is busy: what {{OWNER}} types there waits for the next message from Telegram. Esc gives the keyboard back.
";

const NIGHT: &str = "- What really needs {{OWNER}}'s approval (a message to a third party, a purchase, what can't be undone) waits for the morning while you move on with the rest. The hook's first prompt after the night says so: then write {{OWNER}} one message, what you did during the night and then your questions.";

const STOP: &str = "## Stop

When {{OWNER}} asks, or when the session stops for good: `asp telegram stop --session NAME` (the bot tells {{OWNER}}). The hook then lets every turn end as usual.

If no session listens, nobody reads Telegram: messages wait there (24 h at most) and are filed as soon as a session listens.
";

/// Writes the Codex skill unless the user wrote their own telegram skill.
/// Returns (file, written).
pub fn install_skill(codex: &Path, text: &str) -> (PathBuf, bool) {
    let file = codex.join("skills").join("telegram").join("SKILL.md");
    let theirs = std::fs::read_to_string(&file).is_ok_and(|current| !current.contains(SKILL_MARKER));
    let written = !theirs
        && std::fs::create_dir_all(file.parent().unwrap_or(codex)).is_ok()
        && std::fs::write(&file, text).is_ok();
    (file, written)
}
