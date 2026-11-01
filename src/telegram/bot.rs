//! Talking with the owner through their own Telegram bot, from every coding
//! agent session that listens: one session per project, each under a name.
//!
//! Telegram allows a single reader (`getUpdates`): the listener holding
//! `locks/poll.lock` reads for all and files each message in its session's
//! inbox (`inbox/<session>/`); the others wait on their inbox and take over
//! when it stops. Who a message is for, in order: the session's topic
//! (threaded mode), a leading "name:", the session whose message it
//! replies to, the session of the previous message if under 3 min, outside
//! topics the session that wrote last (under 30 min), the only active
//! session; otherwise the bot asks with buttons.
//!
//! Project groups: a Telegram group (the owner, the bot as administrator,
//! other registered members) can be bound to a session (`groups.json`). Its
//! members' messages go straight to that session's inbox, signed with who
//! speaks; everyone else's, and unknown groups', are only journaled.
//!
//! On-disk names (the `how` labels, files, locks) are the ones the Python
//! sync-telegram writes, so both can share one data folder; its journals
//! mark the owner's messages `from: "charles"`, read like `"owner"`.

use super::api::{ApiError, Params, Telegram};
use super::store::{now, Fields, Lock, Object, Store};
use super::text::{address, chunks, crc32, escape, normal, stamp, tag};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

/// French or English, picked once per bot from `bot.json` "language".
macro_rules! say {
    ($bot:expr, $fr:literal, $en:literal) => {
        if $bot.french() {
            format!($fr)
        } else {
            format!($en)
        }
    };
}
pub(crate) use say;

/// Short long polls: the reading passes quickly from one listener to another.
const POLL_WAIT: f64 = 25.0;
/// A message without an address, under 3 min after the previous one, follows it.
const FOLLOW_UP: f64 = 180.0;
/// `--max 0`: listen until the next message.
const FOREVER: f64 = 1e9;
/// Outside topics, a message without an address answers the session that
/// wrote under 30 min ago.
const ANSWER: f64 = 1800.0;
/// A session that listened under 10 min ago still counts as active.
const ACTIVE: f64 = 600.0;
/// A message waiting 3 min for a session that doesn't listen: the owner is told.
const STALE: f64 = 180.0;
/// /sessions and the buttons only show this week's sessions.
const SHOWN: f64 = 7.0 * 86400.0;
/// In a group, "Claude isn't available" at most once every 6 h.
const ABSENT_NOTICE: f64 = 6.0 * 3600.0;
pub(crate) const API_TIMEOUT: Duration = Duration::from_secs(70);
const UPDATES: &str = r#"["message", "edited_message", "callback_query", "my_chat_member"]"#;
const GROUPS: [&str; 2] = ["group", "supergroup"];
pub(crate) const MEDIA: [&str; 6] = ["voice", "audio", "video_note", "photo", "document", "video"];
/// The only colors Telegram accepts for a topic icon.
const TOPIC_COLORS: [i64; 6] = [7322096, 16766590, 13338331, 9367192, 16749490, 16478047];

/// A listener already holds this session's inbox.
#[derive(Debug)]
pub struct Busy(pub String);

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Busy {}

/// Where a bot message goes and what it answers.
#[derive(Default)]
pub struct Notice {
    pub thread: Option<i64>,
    pub reply_to: Option<i64>,
    pub markup: Option<Value>,
    pub session: Option<String>,
    /// A group (or a member's private chat) instead of the owner's chat.
    pub chat: Option<i64>,
}

pub struct Route {
    pub session: Option<String>,
    pub how: &'static str,
    pub text: String,
    pub candidates: Vec<String>,
}

pub type Listeners = Box<dyn Fn() -> Vec<(u32, String)>>;

pub struct Bot {
    pub store: Store,
    tg: Box<dyn Telegram>,
    /// Listeners still running a build older than the installed one.
    old_listeners: Listeners,
    identity: OnceCell<(bool, String)>,
}

/// `from` of the owner's messages in the journal and inboxes.
pub(crate) const FROM_OWNER: &str = "owner";

/// The owner's message (sync-telegram's journals say "charles").
pub(crate) fn from_owner(entry: &Object) -> bool {
    matches!(entry.text("from"), Some(FROM_OWNER) | Some("charles"))
}

fn api_error(error: &anyhow::Error) -> Option<&ApiError> {
    error.downcast_ref::<ApiError>()
}

fn obj(value: Value) -> Object {
    match value {
        Value::Object(object) => object,
        _ => Object::new(),
    }
}

fn nested_int(object: &Object, outer: &str, inner: &str) -> Option<i64> {
    object.object(outer).and_then(|o| o.int(inner))
}

fn nested_text<'a>(object: &'a Object, outer: &str, inner: &str) -> Option<&'a str> {
    object.object(outer).and_then(|o| o.text(inner))
}

fn opt(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::from)
}

fn message_text(message: &Object) -> String {
    message
        .text("text")
        .filter(|text| !text.is_empty())
        .or_else(|| message.text("caption"))
        .unwrap_or("")
        .to_string()
}

fn has_media(message: &Object) -> bool {
    MEDIA.iter().any(|kind| message.flag(kind))
}

/// The topic a message was written in, if any.
fn topic_of(message: &Object) -> Option<i64> {
    if message.flag("is_topic_message") {
        message.int("message_thread_id")
    } else {
        None
    }
}

fn first_word(text: &str) -> String {
    if !text.starts_with('/') {
        return String::new();
    }
    text.split_whitespace()
        .next()
        .and_then(|word| word.split('@').next())
        .unwrap_or("")
        .to_lowercase()
}

impl Bot {
    pub fn new(store: Store, tg: Box<dyn Telegram>, old_listeners: Listeners) -> Self {
        Self {
            store,
            tg,
            old_listeners,
            identity: OnceCell::new(),
        }
    }

    // Settings: bot.json.

    pub fn config(&self) -> Result<Object> {
        match self.store.read_object(&self.store.path(&["bot.json"])) {
            Some(settings) if !settings.is_empty() => Ok(settings),
            _ => bail!(
                "{} missing: run `asp telegram setup` first",
                self.store.path(&["bot.json"]).display()
            ),
        }
    }

    fn identity(&self) -> &(bool, String) {
        self.identity.get_or_init(|| {
            let settings = self.config().unwrap_or_default();
            let french = settings.text("language") == Some("fr");
            let owner = settings
                .text("name")
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| if french { "Propriétaire" } else { "Owner" }.to_string());
            (french, owner)
        })
    }

    pub fn french(&self) -> bool {
        self.identity().0
    }

    /// The owner's first name, as messages and listeners show it.
    pub fn owner(&self) -> String {
        self.identity().1.clone()
    }

    pub fn update_config(&self, changes: Object) -> Result<Object> {
        let lock = self.store.lock("registre", true);
        let mut settings = self.config()?;
        settings.extend(changes);
        self.store
            .write(&self.store.path(&["bot.json"]), &Value::Object(settings.clone()))?;
        drop(lock);
        Ok(settings)
    }

    fn chat_id(&self) -> Result<i64> {
        match self.config()?.int("chat_id") {
            Some(chat) => Ok(chat),
            None => {
                let owner = self.owner();
                Err(anyhow!(say!(
                    self,
                    "Conversation de {owner} inconnue : il doit envoyer /start au bot (puis asp telegram listen)",
                    "{owner}'s chat is unknown: send /start to the bot (then asp telegram listen)"
                )))
            }
        }
    }

    pub(crate) fn telegram(&self) -> &dyn Telegram {
        self.tg.as_ref()
    }

    fn api(&self, method: &str, params: Params) -> Result<Value, ApiError> {
        self.tg.call(method, &params, API_TIMEOUT, None)
    }

    // Sessions: one per project (or per given name), a sessions/<name>.json each.

    pub fn sessions(&self) -> BTreeMap<String, Object> {
        let mut found = BTreeMap::new();
        for file in self.store.json_files(&self.store.path(&["sessions"])) {
            if let Some(data) = self.store.read_object(&file) {
                if let Some(name) = data.text("name").filter(|name| !name.is_empty()) {
                    found.insert(name.to_string(), data.clone());
                }
            }
        }
        found
    }

    pub fn update_session(&self, name: &str, changes: Object) -> Result<Object> {
        let lock = self.store.lock("registre", true);
        let file = self.store.path(&["sessions", &format!("{name}.json")]);
        let mut data = self.store.read_object(&file).unwrap_or_else(|| {
            obj(json!({"name": name, "created": now(), "aliases": [], "thread_id": null}))
        });
        data.extend(changes);
        self.store.write(&file, &Value::Object(data.clone()))?;
        drop(lock);
        Ok(data)
    }

    pub fn listening(&self, name: &str) -> bool {
        self.store.held(&format!("session.{name}"))
    }

    fn active(&self, data: &Object, at: f64) -> bool {
        !data.flag("stopped")
            && (self.listening(data.text("name").unwrap_or(""))
                || at - data.float("last_listen") < ACTIVE)
    }

    fn recent(&self, data: &Object, at: f64) -> bool {
        self.listening(data.text("name").unwrap_or(""))
            || at - data.float("last_listen").max(data.float("created")) < SHOWN
    }

    fn thread_owner(thread: i64, known: &BTreeMap<String, Object>) -> Option<String> {
        known
            .iter()
            .find(|(_, data)| {
                data.int("thread_id") == Some(thread)
                    || data.list("threads").iter().any(|t| t.as_i64() == Some(thread))
            })
            .map(|(name, _)| name.clone())
    }

    pub fn waiting(&self, name: &str) -> usize {
        self.store
            .json_files(&self.store.path(&["inbox", name]))
            .len()
    }

    // Project groups: groups.json, group id → session, title, members.

    pub fn groups(&self) -> BTreeMap<i64, Object> {
        let data = self
            .store
            .read_object(&self.store.path(&["groups.json"]))
            .unwrap_or_default();
        let mut found = BTreeMap::new();
        if let Some(chats) = data.object("chats") {
            for (chat, group) in chats {
                if let (Ok(id), Some(group)) = (chat.parse::<i64>(), group.as_object()) {
                    let mut group = group.clone();
                    if !matches!(group.get("members"), Some(Value::Object(_))) {
                        group.insert("members".into(), json!({}));
                    }
                    found.insert(id, group);
                }
            }
        }
        found
    }

    /// Applies `change` to the group registry (`whole`: all of groups.json,
    /// with `migrations`; otherwise its `chats`), under the registry lock.
    fn update_groups<T>(&self, whole: bool, change: impl FnOnce(&mut Object) -> Result<T>) -> Result<T> {
        let lock = self.store.lock("registre", true);
        let file = self.store.path(&["groups.json"]);
        let mut data = self.store.read_object(&file).unwrap_or_default();
        if !matches!(data.get("chats"), Some(Value::Object(_))) {
            data.insert("chats".into(), json!({}));
        }
        let result = if whole {
            change(&mut data)?
        } else {
            let chats = data
                .get_mut("chats")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| anyhow!("groups.json: chats"))?;
            change(chats)?
        };
        self.store.write(&file, &Value::Object(data))?;
        drop(lock);
        Ok(result)
    }

    /// {old id: new} of groups turned supergroups, registered or not.
    pub fn migrations(&self) -> HashMap<i64, i64> {
        let data = self
            .store
            .read_object(&self.store.path(&["groups.json"]))
            .unwrap_or_default();
        let mut found = HashMap::new();
        if let Some(moved) = data.object("migrations") {
            for (old, new) in moved {
                let new = new.as_i64().or_else(|| new.as_str().and_then(|s| s.parse().ok()));
                if let (Ok(old), Some(new)) = (old.parse::<i64>(), new) {
                    found.insert(old, new);
                }
            }
        }
        found
    }

    /// A group's current id, following its migrations (basic → supergroup).
    pub fn latest(&self, chat: i64, moved: Option<&HashMap<i64, i64>>) -> i64 {
        let owned;
        let moved = match moved {
            Some(moved) => moved,
            None => {
                owned = self.migrations();
                &owned
            }
        };
        let mut chat = chat;
        let mut seen = HashSet::new();
        while let Some(next) = moved.get(&chat) {
            if !seen.insert(chat) {
                break;
            }
            chat = *next;
        }
        chat
    }

    /// Binds the group to the session; returns its data with `chat` (its
    /// current id). A group bound to another session is only rebound with
    /// `force`, and its members are then forgotten.
    pub fn group_add(
        &self,
        chat: i64,
        session: &str,
        members: &BTreeMap<i64, String>,
        title: Option<&str>,
        force: bool,
    ) -> Result<Object> {
        if chat >= 0 {
            bail!(say!(
                self,
                "{chat} : un groupe Telegram a un identifiant négatif (-100… pour un supergroupe)",
                "{chat}: a Telegram group has a negative id (-100… for a supergroup)"
            ));
        }
        let chat = self.latest(chat, None);
        let session = normal(session);
        if session.is_empty() {
            bail!("--session");
        }
        if let Some(other) = self
            .groups()
            .iter()
            .find(|(other, data)| data.text("session") == Some(&session) && **other != chat)
            .map(|(other, _)| *other)
        {
            bail!(say!(
                self,
                "La session « {session} » est déjà liée au groupe {other} (group remove d’abord)",
                "Session \"{session}\" is already bound to group {other} (group remove first)"
            ));
        }
        if let Some(uid) = members.keys().find(|uid| **uid <= 0) {
            bail!(say!(
                self,
                "--member {uid} : l’identifiant d’une personne est positif",
                "--member {uid}: a person's id is positive"
            ));
        }
        let fallback_title = title
            .map(str::to_string)
            .or_else(|| self.seen_title(chat))
            .unwrap_or_else(|| say!(self, "groupe {chat}", "group {chat}"));
        let french = self.french();
        self.update_groups(false, |chats| {
            let key = chat.to_string();
            let mut data = chats
                .get(&key)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_else(|| obj(json!({"added": now(), "members": {}})));
            if let Some(bound) = data.text("session").filter(|bound| *bound != session) {
                if !force {
                    let bound = bound.to_string();
                    bail!(if french {
                        format!("Le groupe {chat} est déjà lié à la session « {bound} » (--force pour le relier : ses membres sont alors oubliés)")
                    } else {
                        format!("Group {chat} is already bound to session \"{bound}\" (--force to rebind it: its members are then forgotten)")
                    });
                }
                data.insert("members".into(), json!({}));
            }
            let kept_title = title
                .map(str::to_string)
                .or_else(|| data.text("title").map(str::to_string))
                .unwrap_or(fallback_title);
            data.insert("session".into(), json!(session));
            data.insert("title".into(), json!(kept_title));
            let mut all = data.object("members").cloned().unwrap_or_default();
            for (uid, who) in members {
                all.insert(uid.to_string(), json!(who));
            }
            data.insert("members".into(), Value::Object(all));
            chats.insert(key, Value::Object(data.clone()));
            data.insert("chat".into(), json!(chat));
            Ok(data)
        })
    }

    pub fn group_remove(&self, chat: i64, member: Option<i64>) -> Result<Object> {
        let chat = self.latest(chat, None);
        let french = self.french();
        self.update_groups(false, |chats| {
            let key = chat.to_string();
            let Some(data) = chats.get_mut(&key).and_then(Value::as_object_mut) else {
                bail!(if french {
                    format!("Groupe {chat} non enregistré (group list)")
                } else {
                    format!("Group {chat} isn't registered (group list)")
                });
            };
            let Some(member) = member else {
                let removed = data.clone();
                chats.remove(&key);
                return Ok(removed);
            };
            let members = data
                .get_mut("members")
                .and_then(Value::as_object_mut)
                .filter(|members| members.contains_key(&member.to_string()));
            let Some(members) = members else {
                bail!(if french {
                    format!("{member} n’est pas membre enregistré du groupe {chat}")
                } else {
                    format!("{member} isn't a registered member of group {chat}")
                });
            };
            members.remove(&member.to_string());
            Ok(data.clone())
        })
    }

    /// A group turned supergroup changes id: the migration is kept (even for
    /// a group not registered yet, that `group add` will follow) and the
    /// registry follows. True when it is new.
    pub fn migrate_group(&self, old: i64, new: i64, title: Option<&str>) -> Result<bool> {
        let (fresh, registered) = self.update_groups(true, |data| {
            let mut moved = data.object("migrations").cloned().unwrap_or_default();
            let fresh = moved.get(&old.to_string()).and_then(Value::as_i64) != Some(new);
            moved.insert(old.to_string(), json!(new));
            data.insert("migrations".into(), Value::Object(moved));
            let chats = data
                .get_mut("chats")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| anyhow!("groups.json: chats"))?;
            let group = chats.remove(&old.to_string());
            let had = group.is_some();
            if let Some(Value::Object(mut group)) = group {
                group.insert("migrated_from".into(), json!(old));
                chats.insert(new.to_string(), Value::Object(group));
            }
            Ok((fresh, had || chats.contains_key(&new.to_string())))
        })?;
        if fresh {
            let group = title
                .map(str::to_string)
                .or_else(|| self.seen_title(old))
                .or_else(|| self.seen_title(new));
            let text = if registered {
                say!(self, "[groupe {old} devenu supergroupe {new} : registre mis à jour]", "[group {old} became supergroup {new}: registry updated]")
            } else {
                say!(self, "[groupe {old} devenu supergroupe {new}, pas encore enregistré]", "[group {old} became supergroup {new}, not registered yet]")
            };
            self.store.journal(&json!({
                "at": now(), "from": "bot", "event": "migration", "chat_id": new, "old_chat_id": old,
                "group": group, "registered": registered, "text": text,
            }));
        }
        Ok(fresh)
    }

    /// (id, data) of the group bound to the session.
    pub fn group_of(&self, name: &str) -> Option<(i64, Object)> {
        self.groups()
            .into_iter()
            .find(|(_, data)| data.text("session") == Some(name))
    }

    fn seen_title(&self, chat: i64) -> Option<String> {
        self.store
            .entries()
            .iter()
            .rev()
            .find(|entry| entry.int("chat_id") == Some(chat) && entry.flag("group"))
            .and_then(|entry| entry.text("group").map(str::to_string))
    }

    /// Groups seen in the journal (bot added, migration, ignored messages)
    /// that aren't registered, under their current id.
    pub fn unknown_groups(&self) -> BTreeMap<i64, String> {
        let (known, moved) = (self.groups(), self.migrations());
        let mut found = BTreeMap::new();
        for entry in self.store.entries() {
            let Some(chat) = entry.int("chat_id").filter(|chat| *chat != 0) else {
                continue;
            };
            let event = entry.text("event");
            if !(matches!(event, Some("membre") | Some("migration"))
                || entry.text("how") == Some("groupe inconnu"))
            {
                continue;
            }
            let current = self.latest(chat, Some(&moved));
            if event == Some("membre") && matches!(entry.text("status"), Some("left") | Some("kicked")) {
                found.remove(&current);
            } else if !known.contains_key(&current) {
                let title = entry
                    .text("group")
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .or_else(|| found.get(&current).cloned())
                    .unwrap_or_default();
                found.insert(current, title);
            }
        }
        found
    }

    /// A registered member's name (the owner always is one).
    fn member_name(&self, group: &Object, sender: &Object, settings: &Object) -> Option<String> {
        let uid = sender.int("id")?;
        if let Some(named) = group
            .object("members")
            .and_then(|members| members.text(&uid.to_string()))
            .filter(|named| !named.is_empty())
        {
            return Some(named.to_string());
        }
        (Some(uid) == settings.int("user_id")).then(|| self.owner())
    }

    /// A call into a group; if it became a supergroup meanwhile, follows its
    /// new id. Returns (result, id).
    fn group_call(
        &self,
        method: &str,
        chat: i64,
        params: Params,
        timeout: Duration,
        document: Option<&Path>,
    ) -> Result<(Value, i64)> {
        let chat = self.latest(chat, None);
        match self
            .tg
            .call(method, &params.clone().set("chat_id", chat), timeout, document)
        {
            Ok(result) => Ok((result, chat)),
            Err(ApiError::Refused {
                migrate_to: Some(new),
                ..
            }) => {
                self.migrate_group(chat, new, None)?;
                let result = self
                    .tg
                    .call(method, &params.set("chat_id", new), timeout, document)?;
                Ok((result, new))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The topic (group with topics) where a member wrote last: the session
    /// answers there.
    fn set_topic(&self, chat: i64, topic: Option<i64>) -> Result<()> {
        if self.groups().get(&chat).and_then(|g| g.int("topic")) == topic {
            return Ok(());
        }
        self.update_groups(false, |chats| {
            if let Some(data) = chats.get_mut(&chat.to_string()).and_then(Value::as_object_mut) {
                data.insert("topic".into(), opt(topic));
            }
            Ok(())
        })
    }

    /// A send into the group, in its last message's topic (deleted topic:
    /// the general one). Returns (result, id, topic).
    fn group_send(
        &self,
        method: &str,
        chat: i64,
        group: &Object,
        params: Params,
        timeout: Duration,
        document: Option<&Path>,
    ) -> Result<(Value, i64, Option<i64>)> {
        if let Some(topic) = group.int("topic") {
            match self.group_call(
                method,
                chat,
                params.clone().set("message_thread_id", topic),
                timeout,
                document,
            ) {
                Ok((result, chat)) => return Ok((result, chat, Some(topic))),
                Err(error) => {
                    if !api_error(&error).is_some_and(|e| e.mentions(&["thread", "topic"])) {
                        return Err(error);
                    }
                    self.set_topic(self.latest(chat, None), None)?;
                }
            }
        }
        let (result, chat) = self.group_call(method, chat, params, timeout, document)?;
        Ok((result, chat, None))
    }

    // Threaded mode: when the owner turns it on in @BotFather, each session
    // has its topic in the conversation.

    fn topics_enabled(&self) -> Result<bool> {
        let cached = self.config()?.object("topics").cloned().unwrap_or_default();
        if now() - cached.float("checked_at") < 300.0 {
            return Ok(cached.flag("enabled"));
        }
        let enabled = self
            .api("getMe", Params::new())?
            .get("has_topics_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.update_config(obj(json!({"topics": {"enabled": enabled, "checked_at": now()}})))?;
        Ok(enabled)
    }

    /// The session's topic, created when needed; None without threaded mode
    /// or when Telegram refuses.
    fn thread_of(&self, name: &str) -> Option<i64> {
        if !self.topics_enabled().unwrap_or(false) {
            return None;
        }
        let current = |bot: &Self| bot.sessions().get(name).and_then(|data| data.int("thread_id"));
        if let Some(thread) = current(self) {
            return Some(thread);
        }
        let lock = self.store.lock("fils", true); // two sends at once create one topic
        if let Some(thread) = current(self) {
            return Some(thread);
        }
        let color = TOPIC_COLORS[crc32(name.as_bytes()) as usize % TOPIC_COLORS.len()];
        let created = self.chat_id().map_err(|e| e.to_string()).and_then(|chat| {
            self.api(
                "createForumTopic",
                Params::new()
                    .set("chat_id", chat)
                    .set("name", name)
                    .set("icon_color", color),
            )
            .map_err(|e| e.to_string())
        });
        let thread = match created {
            Ok(created) => created.get("message_thread_id").and_then(Value::as_i64),
            Err(reason) => {
                eprintln!("asp telegram: topic for {name} not created ({reason}); messages carry its name");
                None
            }
        }?;
        self.update_session(name, obj(json!({"thread_id": thread}))).ok()?;
        drop(lock);
        Some(thread)
    }

    // Sending.

    /// A message to the owner (or into the group `chat`), journaled with
    /// `record` (who speaks, for which session).
    fn post(&self, params: Params, record: Object, chat: Option<i64>) -> Result<i64> {
        let params = Params::new()
            .json("link_preview_options", &json!({"is_disabled": true}))
            .merge(params);
        let mut record = record;
        let message = match chat {
            Some(chat) => {
                let (message, chat) = self.group_call("sendMessage", chat, params, API_TIMEOUT, None)?;
                record.insert("chat_id".into(), json!(chat));
                message
            }
            None => {
                let chat = self.chat_id()?;
                self.api("sendMessage", Params::new().set("chat_id", chat).merge(params))?
            }
        };
        let id = message
            .get("message_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("sendMessage: no message_id"))?;
        let mut entry = obj(json!({"at": now(), "message_id": id}));
        entry.extend(record);
        self.store.journal(&Value::Object(entry));
        Ok(id)
    }

    /// A session's text in its project group: no header, the group is its own.
    fn send_group(&self, text: &str, name: &str, chat: i64, group: &Object) -> Result<Vec<i64>> {
        let mut ids = Vec::new();
        let mut group = group.clone();
        let mut chat = chat;
        for part in chunks(text.trim()) {
            let (message, moved, topic) = self.group_send(
                "sendMessage",
                chat,
                &group,
                Params::new()
                    .set("text", &part)
                    .json("link_preview_options", &json!({"is_disabled": true})),
                API_TIMEOUT,
                None,
            )?;
            chat = moved;
            group.insert("topic".into(), opt(topic));
            let id = message.get("message_id").and_then(Value::as_i64).unwrap_or(0);
            self.store.journal(&json!({
                "at": now(), "message_id": id, "from": "claude", "session": name, "chat_id": chat,
                "group": group.get("title"), "thread_id": null, "topic_id": topic, "text": part,
            }));
            ids.push(id);
        }
        Ok(ids)
    }

    fn register_if_new(&self, name: &str) -> Result<()> {
        if !self.sessions().contains_key(name) {
            let cwd = std::env::current_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default();
            self.update_session(name, obj(json!({"cwd": cwd})))?;
        }
        Ok(())
    }

    /// A session's text: in its project group if it has one (unless
    /// `private`), otherwise to the owner, in its topic or headed by its name.
    pub fn send(&self, text: &str, name: &str, private: bool) -> Result<Vec<i64>> {
        self.register_if_new(name)?;
        if !private {
            if let Some((chat, group)) = self.group_of(name) {
                return self.send_group(text, name, chat, &group);
            }
        }
        let mut ids = Vec::new();
        for part in chunks(text.trim()) {
            for attempt in 1..=2 {
                let thread = self.thread_of(name);
                let params = match thread {
                    Some(thread) => Params::new().set("text", &part).set("message_thread_id", thread),
                    None => Params::new()
                        .set("text", format!("<b>{}</b>\n{}", escape(&tag(name)), escape(&part)))
                        .set("parse_mode", "HTML"),
                };
                let record = obj(json!({"from": "claude", "session": name, "thread_id": thread, "text": part}));
                match self.post(params, record, None) {
                    Ok(id) => {
                        ids.push(id);
                        break;
                    }
                    Err(error) => {
                        if thread.is_some()
                            && attempt == 1
                            && api_error(&error).is_some_and(|e| e.mentions(&["thread"]))
                        {
                            // Topic deleted by the owner: a new one is created.
                            self.update_session(name, obj(json!({"thread_id": null})))?;
                            continue;
                        }
                        return Err(error);
                    }
                }
            }
        }
        Ok(ids)
    }

    /// A file from a session (PDF, image…), where its messages go;
    /// `caption` goes with it (Telegram cuts it at 1024 characters).
    pub fn send_document(&self, file: &Path, name: &str, caption: &str, private: bool) -> Result<i64> {
        if !file.is_file() {
            let shown = file.display();
            bail!(say!(self, "Fichier introuvable : {shown}", "File not found: {shown}"));
        }
        self.register_if_new(name)?;
        let caption = caption.trim();
        let short = |text: &str| text.chars().take(1024).collect::<String>();
        let file_name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let journal_text = if caption.is_empty() {
            format!("[fichier] {file_name}")
        } else {
            format!("[fichier] {file_name}\n{caption}")
        };
        let upload = Duration::from_secs(120);
        if !private {
            if let Some((chat, group)) = self.group_of(name) {
                let params = if caption.is_empty() {
                    Params::new()
                } else {
                    Params::new().set("caption", short(caption))
                };
                let (message, chat, topic) =
                    self.group_send("sendDocument", chat, &group, params, upload, Some(file))?;
                let id = message.get("message_id").and_then(Value::as_i64).unwrap_or(0);
                self.store.journal(&json!({
                    "at": now(), "message_id": id, "from": "claude", "session": name, "chat_id": chat,
                    "group": group.get("title"), "thread_id": null, "topic_id": topic, "text": journal_text,
                }));
                return Ok(id);
            }
        }
        for attempt in 1..=2 {
            let thread = self.thread_of(name);
            let mut params = Params::new().set("chat_id", self.chat_id()?);
            let text = match thread {
                Some(thread) => {
                    params = params.set("message_thread_id", thread);
                    caption.to_string()
                }
                None => format!("{}\n{caption}", tag(name)).trim().to_string(),
            };
            if !text.is_empty() {
                params = params.set("caption", short(&text));
            }
            match self.tg.call("sendDocument", &params, upload, Some(file)) {
                Ok(message) => {
                    let id = message.get("message_id").and_then(Value::as_i64).unwrap_or(0);
                    self.store.journal(&json!({
                        "at": now(), "message_id": id, "from": "claude", "session": name,
                        "thread_id": thread, "text": journal_text,
                    }));
                    return Ok(id);
                }
                Err(error) => {
                    if thread.is_some() && attempt == 1 && error.mentions(&["thread"]) {
                        self.update_session(name, obj(json!({"thread_id": null})))?;
                        continue;
                    }
                    return Err(error.into());
                }
            }
        }
        bail!("sendDocument: no topic")
    }

    /// A message from the bot itself (question, state, alert), without a
    /// session header; to the owner, or into the group `notice.chat`.
    pub fn notify(&self, text: &str, notice: Notice) -> Result<i64> {
        let mut params = Params::new().set("text", text);
        if let Some(reply_to) = notice.reply_to {
            params = params.json(
                "reply_parameters",
                &json!({"message_id": reply_to, "allow_sending_without_reply": true}),
            );
        }
        if let Some(markup) = &notice.markup {
            params = params.json("reply_markup", markup);
        }
        let record = obj(json!({"from": "bot", "session": notice.session, "text": text}));
        if let Some(chat) = notice.chat {
            let params = match notice.thread {
                Some(thread) => params.set("message_thread_id", thread),
                None => params,
            };
            return self.post(params, record, Some(chat));
        }
        if let Some(thread) = notice.thread {
            let mut in_thread = record.clone();
            in_thread.insert("thread_id".into(), json!(thread));
            match self.post(params.clone().set("message_thread_id", thread), in_thread, None) {
                Ok(id) => return Ok(id),
                Err(error) => {
                    if !api_error(&error).is_some_and(|e| e.mentions(&["thread"])) {
                        return Err(error);
                    }
                }
            }
        }
        self.post(params, record, None)
    }

    /// 👀 on the received message (the owner's, or a member's in their
    /// group): the session got it.
    fn seen(&self, entry: &Object) {
        let Some(message_id) = entry.int("message_id") else {
            return;
        };
        let params = Params::new()
            .set("message_id", message_id)
            .json("reaction", &json!([{"type": "emoji", "emoji": "👀"}]));
        match entry.int("chat_id").filter(|chat| *chat != 0) {
            Some(chat) => {
                let _ = self.group_call("setMessageReaction", chat, params, API_TIMEOUT, None);
            }
            None => {
                if let Ok(chat) = self.chat_id() {
                    let _ = self.api("setMessageReaction", params.set("chat_id", chat));
                }
            }
        }
    }

    fn guide(&self) -> String {
        let topics = self
            .config()
            .ok()
            .and_then(|c| c.object("topics").map(|t| t.flag("enabled")))
            .unwrap_or(false);
        let mut lines = Vec::new();
        if topics {
            lines.push(say!(
                self,
                "• Chaque session a son fil, à son nom : écris dans le fil du projet. Dans un nouveau fil, je te demande une fois pour quel projet, puis le fil lui reste.",
                "• Each session has its own topic, under its name: write in the project's topic. In a new topic I ask once which project it is for, then it stays with it."
            ));
        }
        let example = tag("my-project");
        lines.push(say!(
            self,
            "• Hors fil, chaque message d’une session commence par son nom ({example}).",
            "• Outside topics, each message from a session starts with its name ({example})."
        ));
        lines.push(say!(
            self,
            "• Pour écrire à une session : commence par « nom: » (un bout du nom suffit, « project: … »), ou réponds à un de ses messages.",
            "• To write to a session: start with \"name:\" (part of the name is enough, \"project: …\"), or reply to one of its messages."
        ));
        lines.push(say!(
            self,
            "• Sans précision : la session de ton message précédent s’il date de moins de 3 min, sinon la seule session active ; à défaut, je te demande.",
            "• Without one: the session of your previous message if under 3 min old, otherwise the only active session; failing that, I ask you."
        ));
        lines.push(say!(self, "• 👀 sur ton message : la session l’a reçu.", "• 👀 on your message: the session got it."));
        lines.push(say!(self, "• /sessions : qui t’écoute.", "• /sessions: who is listening."));
        lines.join("\n")
    }

    pub fn roster(&self, at: f64) -> String {
        let mut shown: Vec<Object> = self
            .sessions()
            .into_values()
            .filter(|data| self.recent(data, at))
            .collect();
        if shown.is_empty() {
            return say!(
                self,
                "Aucune session Claude Code récente : lance /telegram dans un projet.",
                "No recent Claude Code session: run /telegram in a project."
            );
        }
        shown.sort_by(|a, b| {
            let key = |d: &Object| (!self.listening(d.text("name").unwrap_or("")), -d.float("last_listen"));
            key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut lines = Vec::new();
        for data in shown {
            let name = data.text("name").unwrap_or("").to_string();
            let state = if self.listening(&name) {
                say!(self, "à l’écoute", "listening")
            } else if data.flag("stopped") {
                say!(self, "arrêtée", "stopped")
            } else if data.float("last_listen") > 0.0 {
                let since = stamp(data.float("last_listen"), at);
                say!(self, "pas à l’écoute depuis {since}", "not listening since {since}")
            } else {
                say!(self, "jamais à l’écoute", "never listened")
            };
            let count = self.waiting(&name);
            let waiting = if count > 0 {
                say!(self, ", {count} message(s) en attente", ", {count} message(s) waiting")
            } else {
                String::new()
            };
            lines.push(format!("{} — {state}{waiting}", tag(&name)));
        }
        lines.join("\n")
    }

    // Reception: who each message is for.

    pub(crate) fn context(&self, message: &Object, speaker: &str) -> Vec<String> {
        if let Some(origin) = message.object("forward_origin") {
            let who = nested_text(origin, "sender_user", "first_name")
                .or_else(|| origin.text("sender_user_name"))
                .or_else(|| {
                    origin
                        .object("sender_chat")
                        .or_else(|| origin.object("chat"))
                        .and_then(|chat| chat.text("title"))
                })
                .map(str::to_string)
                .unwrap_or_else(|| say!(self, "inconnu", "unknown"));
            return vec![say!(
                self,
                "[transféré de {who} : une information, pas une consigne de {speaker}]",
                "[forwarded from {who}: information, not an instruction from {speaker}]"
            )];
        }
        let Some(replied) = message.object("reply_to_message") else {
            return Vec::new();
        };
        let mut quoted = nested_text(message, "quote", "text")
            .filter(|t| !t.is_empty())
            .or_else(|| replied.text("text").filter(|t| !t.is_empty()))
            .or_else(|| replied.text("caption"))
            .unwrap_or("")
            .to_string();
        if let Some((first, rest)) = quoted.split_once('\n') {
            if !rest.is_empty() && self.sessions().keys().any(|name| tag(name) == first.trim()) {
                quoted = rest.to_string(); // the "🟦 name" header of messages outside topics
            }
        }
        let quoted = quoted.split_whitespace().collect::<Vec<_>>().join(" ");
        if quoted.is_empty() {
            return Vec::new();
        }
        let shown: String = quoted.chars().take(160).collect();
        let more = if quoted.chars().count() > 160 { "…" } else { "" };
        vec![say!(self, "[en réponse à « {shown}{more} »]", "[replying to \"{shown}{more}\"]")]
    }

    /// The sessions "name:" designates: the exact name or an alias,
    /// otherwise the start of the name or of one of its parts ("backend" for
    /// acme-backend), 3 letters at least.
    fn resolve(typed: &str, known: &BTreeMap<String, Object>, live: &BTreeMap<String, Object>) -> Vec<String> {
        if typed.is_empty() {
            return Vec::new();
        }
        let aliases = |data: &Object| -> Vec<String> {
            data.list("aliases")
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect()
        };
        if let Some(name) = known
            .iter()
            .find(|(name, data)| *name == typed || aliases(data).iter().any(|a| a == typed))
            .map(|(name, _)| name.clone())
        {
            return vec![name];
        }
        if typed.chars().count() < 3 {
            return Vec::new();
        }
        live.iter()
            .filter(|(name, data)| {
                std::iter::once(name.as_str())
                    .chain(name.split(['-', '.', '_']))
                    .map(str::to_string)
                    .chain(aliases(data))
                    .any(|part| part.starts_with(typed))
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The session of the owner's last message, if routed under 3 min ago.
    fn last_routed(&self, at: f64) -> Option<String> {
        let entries = self.store.entries();
        let entry = entries.iter().rev().find(|e| from_owner(e))?;
        let session = entry.text("session").filter(|s| !s.is_empty())?;
        (at - entry.float("routed_at") < FOLLOW_UP).then(|| session.to_string())
    }

    /// The session that wrote to the owner last, outside topics, under 30 min ago.
    fn last_speaker(&self, at: f64) -> Option<String> {
        for entry in self.store.entries().iter().rev() {
            if entry.int("chat_id").is_some_and(|chat| chat != 0) {
                continue; // written in a project group, not to the owner
            }
            if entry.text("from") == Some("claude") {
                if let Some(session) = entry.text("session").filter(|s| !s.is_empty()) {
                    if !entry.flag("thread_id") && at - entry.float("at") < ANSWER {
                        return Some(session.to_string());
                    }
                    return None;
                }
            }
        }
        None
    }

    /// The journal entry of this message. Message numbers are per chat:
    /// `chat` None is the owner's private chat (entries without chat_id).
    fn find(&self, message_id: Option<i64>, chat: Option<i64>) -> Option<Object> {
        self.store
            .entries()
            .into_iter()
            .rev()
            .find(|entry| entry.int("message_id") == message_id && entry.int("chat_id") == chat)
    }

    pub fn route(&self, message: &Object, text: &str, at: f64) -> Route {
        let known = self.sessions();
        let live: BTreeMap<String, Object> = known
            .iter()
            .filter(|(_, data)| !data.flag("stopped") && self.recent(data, at))
            .map(|(name, data)| (name.clone(), data.clone()))
            .collect();
        let found = |session: String, how: &'static str, text: &str| Route {
            session: Some(session),
            how,
            text: text.to_string(),
            candidates: Vec::new(),
        };
        let thread = topic_of(message);
        if let Some(owner) = thread.and_then(|thread| Self::thread_owner(thread, &known)) {
            return found(owner, "fil", text);
        }
        if let Some(addressed) = address(text) {
            let named = Self::resolve(&normal(&addressed.typed), &known, &live);
            let rest = addressed.rest.trim().to_string();
            if named.len() == 1 {
                return found(named[0].clone(), "nom", &rest);
            }
            if !named.is_empty() {
                return Route {
                    session: None,
                    how: "",
                    text: rest,
                    candidates: named,
                };
            }
        }
        if let Some(replied) = message.object("reply_to_message") {
            if let Some(session) = self
                .find(replied.int("message_id"), None)
                .and_then(|entry| entry.text("session").map(str::to_string))
                .filter(|session| known.contains_key(session))
            {
                return found(session, "réponse", text);
            }
        }
        if thread.is_none() {
            if let Some(last) = self.last_routed(at).filter(|s| live.contains_key(s)) {
                return found(last, "suite", text);
            }
            if let Some(speaker) = self.last_speaker(at).filter(|s| live.contains_key(s)) {
                return found(speaker, "dernière à t’écrire", text);
            }
        }
        // A session bound to a project group listens for that group: a
        // private message only reaches it when addressed to it (topic, name,
        // reply, follow-up, button), otherwise its answer would go to the group.
        let bound: HashSet<String> = self
            .groups()
            .values()
            .filter_map(|data| data.text("session").map(str::to_string))
            .collect();
        let actives: Vec<&String> = live
            .iter()
            .filter(|(name, data)| self.active(data, at) && !bound.contains(*name))
            .map(|(name, _)| name)
            .collect();
        if actives.len() == 1 {
            return found(actives[0].clone(), "seule active", text);
        }
        Route {
            session: None,
            how: "",
            text: text.to_string(),
            candidates: Vec::new(),
        }
    }

    /// The session will answer in the topic the owner just wrote to it in.
    /// A topic nobody owns yet becomes its own and takes its name; another
    /// session's topic is never taken.
    fn follow(&self, name: &str, thread: i64) -> Result<()> {
        let known = self.sessions();
        let owner = Self::thread_owner(thread, &known);
        let data = known.get(name).cloned().unwrap_or_default();
        if owner.as_deref().is_some_and(|owner| owner != name) || data.int("thread_id") == Some(thread) {
            return Ok(());
        }
        let mut others: Vec<i64> = data
            .list("threads")
            .iter()
            .filter_map(Value::as_i64)
            .chain(data.int("thread_id"))
            .filter(|t| *t != thread)
            .collect();
        others.sort_unstable();
        others.dedup();
        self.update_session(name, obj(json!({"thread_id": thread, "threads": others})))?;
        if owner.is_none() {
            if let Ok(chat) = self.chat_id() {
                let _ = self.api(
                    "editForumTopic",
                    Params::new()
                        .set("chat_id", chat)
                        .set("message_thread_id", thread)
                        .set("name", name),
                );
            }
        }
        Ok(())
    }

    fn deliver(&self, name: &str, entry: &Object, how: &str) -> Result<()> {
        let mut entry = entry.clone();
        entry.insert("session".into(), json!(name));
        entry.insert("how".into(), json!(how));
        entry.insert("routed_at".into(), json!(now()));
        if let Some(thread) = entry.int("thread_id") {
            self.follow(name, thread)?;
        }
        let update_id = entry.int("update_id").unwrap_or(0);
        self.store.write(
            &self.store.path(&["inbox", name, &format!("{update_id}.json")]),
            &Value::Object(entry.clone()),
        )?;
        self.store.journal(&Value::Object(entry));
        Ok(())
    }

    /// "Which session?" buttons; the message waits in pending/ for the answer.
    fn ask(&self, entry: &Object, candidates: Vec<String>) -> Result<()> {
        let at = now();
        let names = if candidates.is_empty() {
            let mut recent: Vec<(String, Object)> = self
                .sessions()
                .into_iter()
                .filter(|(_, data)| !data.flag("stopped") && data.float("last_listen") > 0.0 && self.recent(data, at))
                .collect();
            recent.sort_by(|(a, da), (b, db)| {
                let key = |n: &str, d: &Object| (!self.listening(n), -d.float("last_listen"));
                key(a, da).partial_cmp(&key(b, db)).unwrap_or(std::cmp::Ordering::Equal)
            });
            recent.into_iter().take(8).map(|(name, _)| name).collect()
        } else {
            candidates
        };
        let thread = entry.int("thread_id");
        let reply_to = entry.int("message_id");
        if names.is_empty() {
            let mut lost = entry.clone();
            lost.insert("how".into(), json!("sans session"));
            lost.insert("routed_at".into(), json!(at));
            self.store.journal(&Value::Object(lost));
            self.notify(
                &say!(
                    self,
                    "Aucune session Claude Code ne t’écoute : ton message n’a été remis à personne. Lance /telegram dans un projet.",
                    "No Claude Code session is listening: your message wasn't delivered. Run /telegram in a project."
                ),
                Notice { thread, reply_to, ..Notice::default() },
            )?;
            return Ok(());
        }
        let update_id = entry.int("update_id").unwrap_or(0);
        self.store.write(
            &self.store.path(&["pending", &format!("{update_id}.json")]),
            &Value::Object(entry.clone()),
        )?;
        let mut rows: Vec<Value> = names
            .iter()
            .map(|name| json!([{"text": tag(name), "callback_data": format!("to:{update_id}:{name}")}]))
            .collect();
        rows.push(json!([{
            "text": say!(self, "Laisser de côté", "Leave it"),
            "callback_data": format!("no:{update_id}"),
        }]));
        self.notify(
            &say!(
                self,
                "Plusieurs sessions Claude Code t’écoutent, une par projet : pour laquelle est ce message ?",
                "Several Claude Code sessions are listening, one per project: which one is this message for?"
            ),
            Notice {
                thread,
                reply_to,
                markup: Some(json!({"inline_keyboard": rows})),
                ..Notice::default()
            },
        )?;
        Ok(())
    }

    fn choose(&self, callback: &Object) -> Result<()> {
        let data = callback.text("data").unwrap_or("");
        let (kind, rest) = data.split_once(':').unwrap_or((data, ""));
        let (update_id, name) = rest.split_once(':').unwrap_or((rest, ""));
        let file = (!update_id.is_empty() && update_id.chars().all(|c| c.is_ascii_digit()))
            .then(|| self.store.path(&["pending", &format!("{update_id}.json")]));
        let entry = file.as_ref().and_then(|file| self.store.read_object(file));
        let answer = match (entry, &file) {
            (Some(entry), Some(file)) => {
                if kind == "to" && self.sessions().contains_key(name) {
                    self.deliver(name, &entry, "bouton")?;
                    let _ = std::fs::remove_file(file);
                    format!("→ {}", tag(name))
                } else {
                    let mut left = entry.clone();
                    left.insert("how".into(), json!("laissé"));
                    left.insert("routed_at".into(), json!(now()));
                    self.store.journal(&Value::Object(left));
                    let _ = std::fs::remove_file(file);
                    say!(self, "Laissé de côté.", "Left aside.")
                }
            }
            _ => say!(self, "Déjà traité.", "Already handled."),
        };
        if let (Ok(chat), Some(prompt)) = (
            self.chat_id(),
            callback.object("message").and_then(|m| m.int("message_id")),
        ) {
            let _ = self.api(
                "editMessageText",
                Params::new()
                    .set("chat_id", chat)
                    .set("message_id", prompt)
                    .set("text", &answer),
            );
        }
        if let Some(id) = callback.text("id").filter(|id| !id.is_empty()) {
            let _ = self.api(
                "answerCallbackQuery",
                Params::new().set("callback_query_id", id).set("text", &answer),
            );
        }
        Ok(())
    }

    /// "/start@AnotherBot": a command meant for another bot of the group.
    fn for_another_bot(&self, text: &str) -> bool {
        if !text.starts_with('/') {
            return false;
        }
        let first = text.split_whitespace().next().unwrap_or("");
        let Some((_, target)) = first.split_once('@') else {
            return false;
        };
        target.to_lowercase() != self.bot_username().to_lowercase()
    }

    /// The bot's @username, from bot.json, else asked once to Telegram.
    fn bot_username(&self) -> String {
        if let Some(name) = self
            .config()
            .ok()
            .and_then(|c| c.text("username").map(str::to_string))
        {
            return name;
        }
        let Ok(me) = self.api("getMe", Params::new()) else {
            return String::new();
        };
        let name = me.get("username").and_then(Value::as_str).unwrap_or("").to_string();
        if !name.is_empty() {
            let _ = self.update_config(obj(json!({"username": name})));
        }
        name
    }

    /// /start, /sessions, /aide: the bot answers by itself. True when it was one.
    fn command(&self, text: &str, message: &Object) -> Result<bool> {
        let thread = topic_of(message);
        let notice = || Notice { thread, ..Notice::default() };
        match first_word(text).as_str() {
            "/start" => {
                let hello = say!(
                    self,
                    "Bien reçu. Je relaie tes messages aux sessions Claude Code de ton Mac.",
                    "Got it. I pass your messages on to the Claude Code sessions on your Mac."
                );
                self.notify(&format!("{hello}\n\n{}", self.guide()), notice())?;
            }
            "/sessions" | "/projets" | "/projects" => {
                self.notify(&self.roster(now()), notice())?;
            }
            "/aide" | "/help" => {
                self.notify(&self.guide(), notice())?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn group_guide(&self, group: &Object) -> String {
        let title = group.text("title").unwrap_or("");
        say!(
            self,
            "Je transmets vos messages à Claude, qui s’occupe de « {title} ». Écrivez simplement ici : un message, une photo ou un vocal. 👀 sur votre message : il est bien reçu.",
            "I pass your messages on to Claude, who takes care of \"{title}\". Just write here: a message, a photo or a voice note. 👀 on your message: it was received."
        )
    }

    /// /aide, /start, /sessions in a project group: a short answer, in the group.
    fn group_command(&self, text: &str, message: &Object, group: &Object, chat: i64) -> Result<bool> {
        let answer = match first_word(text).as_str() {
            "/start" | "/aide" | "/help" => self.group_guide(group),
            "/sessions" | "/projets" | "/projects" => {
                let name = group.text("session").unwrap_or("");
                if self.listening(name) {
                    say!(self, "Claude ({name}) est à l’écoute.", "Claude ({name}) is listening.")
                } else {
                    say!(
                        self,
                        "Claude ({name}) n’écoute pas en ce moment : vos messages l’attendent, il les lira à son retour.",
                        "Claude ({name}) isn't listening right now: your messages wait for it, it will read them when it's back."
                    )
                }
            }
            _ => return Ok(false),
        };
        self.notify(
            &answer,
            Notice {
                thread: topic_of(message),
                reply_to: message.int("message_id"),
                session: group.text("session").map(str::to_string),
                chat: Some(chat),
                ..Notice::default()
            },
        )?;
        Ok(true)
    }

    /// The bot added to a group, removed, made administrator: journaled; the
    /// owner is told privately when it concerns them.
    fn membership(&self, update: &Object, settings: &Object) -> Result<i32> {
        let Some(change) = update.object("my_chat_member") else {
            return Ok(0);
        };
        let chat = change.object("chat").cloned().unwrap_or_default();
        if !GROUPS.contains(&chat.text("type").unwrap_or("")) {
            return Ok(0);
        }
        let old = nested_text(change, "old_chat_member", "status").unwrap_or("").to_string();
        let new = nested_text(change, "new_chat_member", "status").unwrap_or("").to_string();
        let by = change.object("from").cloned().unwrap_or_default();
        let id = chat.int("id").unwrap_or(0);
        let title = chat.text("title").map(str::to_string).unwrap_or_else(|| id.to_string());
        let inside = ["member", "administrator", "creator", "restricted"];
        let verb = match new.as_str() {
            "administrator" => say!(self, "administrateur", "administrator"),
            "member" => say!(self, "membre (sans droits d’administrateur)", "member (without administrator rights)"),
            "left" => say!(self, "retiré", "removed"),
            "kicked" => say!(self, "exclu", "banned"),
            "restricted" => say!(self, "restreint", "restricted"),
            other => other.to_string(),
        };
        let who = by
            .text("first_name")
            .map(str::to_string)
            .unwrap_or_else(|| by.int("id").unwrap_or(0).to_string());
        self.store.journal(&json!({
            "at": change.int("date").map_or(now(), |d| d as f64), "from": "bot", "event": "membre",
            "chat_id": id, "group": title, "status": new, "old_status": old, "by": by.int("id"),
            "update_id": update.int("update_id"),
            "text": say!(self, "[bot {verb} dans le groupe « {title} » ({id}) par {who}]", "[bot {verb} in group \"{title}\" ({id}) by {who}]"),
        }));
        let registered = self.groups().contains_key(&id);
        let joined = inside.contains(&new.as_str()) && !inside.contains(&old.as_str());
        let notice = if joined && !registered && by.int("id") == settings.int("user_id") {
            let mut notice = say!(
                self,
                "Ajouté au groupe « {title} » (identifiant {id}). Pour le lier à une session : asp telegram group add --chat {id} --session NOM --member ID=Nom.",
                "Added to group \"{title}\" (id {id}). To bind it to a session: asp telegram group add --chat {id} --session NAME --member ID=Name."
            );
            if new != "administrator" {
                notice.push(' ');
                notice.push_str(&say!(
                    self,
                    "Nomme-moi administrateur du groupe pour que je lise tous les messages.",
                    "Make me an administrator of the group so I read every message."
                ));
            }
            if chat.text("type") == Some("group") {
                notice.push(' ');
                notice.push_str(&say!(
                    self,
                    "Me nommer administrateur peut en faire un supergroupe, sous un nouvel identifiant (-100…) : je te le dirai ; asp telegram group list donne toujours le bon.",
                    "Making me administrator can turn it into a supergroup, under a new id (-100…): I'll tell you; asp telegram group list always gives the right one."
                ));
            }
            Some(notice)
        } else if registered && new == "member" && old == "administrator" {
            Some(say!(
                self,
                "Je ne suis plus administrateur de « {title} » : je n’y lis plus que les commandes et les réponses.",
                "I'm no longer an administrator of \"{title}\": I only read commands and replies there now."
            ))
        } else if registered && matches!(new.as_str(), "left" | "kicked") {
            Some(say!(
                self,
                "Retiré du groupe « {title} ». Il reste enregistré : asp telegram group remove --chat {id} pour l’oublier.",
                "Removed from group \"{title}\". It stays registered: asp telegram group remove --chat {id} to forget it."
            ))
        } else {
            None
        };
        if let Some(notice) = notice {
            let _ = self.notify(&notice, Notice::default());
        }
        Ok(0)
    }

    /// A group message: delivered to the group's session when it comes from
    /// a registered member; otherwise ignored (1) and only journaled.
    fn dispatch_group(&self, update: &Object, message: &Object, edited: bool, settings: &Object) -> Result<i32> {
        let chat_data = message.object("chat").cloned().unwrap_or_default();
        let chat = chat_data.int("id").ok_or_else(|| anyhow!("message without chat id"))?;
        let moved = match (message.int("migrate_to_chat_id"), message.int("migrate_from_chat_id")) {
            (Some(new), _) => Some((chat, new)),
            (None, Some(old)) => Some((old, chat)),
            _ => None,
        };
        if let Some((old, new)) = moved {
            let title = chat_data.text("title");
            if self.migrate_group(old, new, title)?
                && !self.groups().contains_key(&new)
                && self.added_by_owner(old, settings)
            {
                let shown = title.map(str::to_string).unwrap_or_else(|| old.to_string());
                let _ = self.notify(
                    &say!(
                        self,
                        "Le groupe « {shown} » est devenu un supergroupe : son identifiant est maintenant {new} (et non plus {old}). Pour le lier : asp telegram group add --chat {new} --session NOM --member ID=Nom.",
                        "Group \"{shown}\" became a supergroup: its id is now {new} (no longer {old}). To bind it: asp telegram group add --chat {new} --session NAME --member ID=Name."
                    ),
                    Notice::default(),
                );
            }
            return Ok(0);
        }
        let sender = message.object("from").cloned().unwrap_or_default();
        let group = self.groups().get(&chat).cloned();
        let text = message_text(message);
        if text.is_empty() && !has_media(message) {
            return Ok(0); // service message (member added, title changed…)
        }
        let topic = topic_of(message);
        let at = message.int("edit_date").or_else(|| message.int("date")).unwrap_or(0);
        let shown_title = group
            .as_ref()
            .and_then(|g| g.text("title"))
            .or_else(|| chat_data.text("title"))
            .map(str::to_string)
            .unwrap_or_else(|| chat.to_string());
        let mut base = obj(json!({
            "at": at, "from": "groupe", "chat_id": chat, "group": shown_title,
            "user_id": sender.int("id"), "message_id": message.int("message_id"),
            "update_id": update.int("update_id"),
        }));
        let short: String = text.chars().take(300).collect();
        let short = if short.is_empty() { say!(self, "(pièce jointe)", "(attachment)") } else { short };
        let ignored = |base: &Object, speaker: &str, how: &str| {
            let mut entry = base.clone();
            entry.extend(obj(json!({
                "speaker": speaker, "how": how, "session": null, "text": short, "routed_at": now(),
            })));
            self.store.journal(&Value::Object(entry));
        };
        if sender.flag("is_bot") {
            let Some(sender_chat) = message.object("sender_chat") else {
                return Ok(0); // the bot itself (or another bot of the group)
            };
            // An anonymous administrator, or a message on behalf of a
            // channel: nobody knows who speaks, so nothing is delivered; the
            // owner is told once per group.
            let first = group.is_some()
                && !self
                    .store
                    .entries()
                    .iter()
                    .any(|e| e.text("how") == Some("anonyme") && e.int("chat_id") == Some(chat));
            ignored(&base, sender_chat.text("title").unwrap_or("anonyme"), "anonyme");
            if first {
                let title = base.text("group").unwrap_or("").to_string();
                let _ = self.notify(
                    &say!(
                        self,
                        "Un message anonyme (administrateur « anonyme » ou au nom d’une chaîne) dans « {title} » n’a pas été transmis : je ne sais pas qui l’écrit. Désactive « Rester anonyme » dans les droits d’administrateur du groupe pour que tes messages y arrivent.",
                        "An anonymous message (\"anonymous\" administrator or on behalf of a channel) in \"{title}\" wasn't passed on: I can't tell who wrote it. Turn off \"Remain anonymous\" in the group's administrator rights so your messages get through."
                    ),
                    Notice::default(),
                );
            }
            return Ok(1);
        }
        let Some(group) = group else {
            ignored(&base, &Self::full_name(&sender), "groupe inconnu");
            return Ok(1);
        };
        let Some(speaker) = self.member_name(&group, &sender, settings) else {
            ignored(&base, &Self::full_name(&sender), "non membre");
            return Ok(1);
        };
        if self.for_another_bot(text.trim()) {
            let mut entry = base.clone();
            entry.extend(obj(json!({
                "speaker": speaker, "how": "autre bot", "session": null,
                "text": text.chars().take(300).collect::<String>(), "routed_at": now(),
            })));
            self.store.journal(&Value::Object(entry));
            return Ok(0); // "/start@AnotherBot": neither answered nor delivered
        }
        if !edited && self.group_command(text.trim(), message, &group, chat)? {
            return Ok(0);
        }
        let body = self.describe(message, update.int("update_id").unwrap_or(0), &text, &speaker);
        let body = if edited { say!(self, "(message modifié) {body}", "(edited message) {body}") } else { body };
        base.extend(obj(json!({"speaker": speaker, "thread_id": null, "topic_id": topic, "text": body})));
        if let Some(album) = message.get("media_group_id").filter(|v| !v.is_null()) {
            base.insert("album".into(), album.clone());
        }
        let session = group.text("session").unwrap_or("").to_string();
        self.deliver(&session, &base, "groupe")?;
        self.set_topic(chat, topic)?;
        Ok(0)
    }

    fn full_name(sender: &Object) -> String {
        let name = [sender.text("first_name"), sender.text("last_name")]
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if name.is_empty() {
            sender.int("id").unwrap_or(0).to_string()
        } else {
            name
        }
    }

    /// The owner added the bot to this group (journal): they want its id.
    fn added_by_owner(&self, chat: i64, settings: &Object) -> bool {
        let owner = settings.int("user_id");
        self.store.entries().iter().any(|e| {
            e.text("event") == Some("membre") && e.int("chat_id") == Some(chat) && e.int("by") == owner
        })
    }

    /// A project group member (not the owner) writes to the bot privately:
    /// nothing is delivered; they are told once to write in their group.
    fn member_writes_privately(&self, update: &Object, message: &Object, settings: &Object) -> Result<i32> {
        let uid = nested_int(message, "from", "id");
        let titles: Vec<String> = self
            .groups()
            .values()
            .filter(|data| {
                uid.is_some_and(|uid| data.object("members").is_some_and(|m| m.contains_key(&uid.to_string())))
            })
            .map(|data| data.text("title").unwrap_or("").to_string())
            .collect();
        if titles.is_empty() || uid == settings.int("user_id") {
            return Ok(1);
        }
        let first = !self
            .store
            .entries()
            .iter()
            .any(|e| e.text("how") == Some("membre en privé") && e.int("user_id") == uid);
        let text = message_text(message);
        let short: String = text.chars().take(300).collect();
        let title = &titles[0];
        let chat = nested_int(message, "chat", "id");
        self.store.journal(&json!({
            "at": message.int("date"), "from": "groupe", "chat_id": chat, "user_id": uid,
            "group": say!(self, "{title}, en privé", "{title}, privately"),
            "speaker": nested_text(message, "from", "first_name").map(str::to_string)
                .unwrap_or_else(|| uid.unwrap_or(0).to_string()),
            "how": "membre en privé", "session": null, "message_id": message.int("message_id"),
            "update_id": update.int("update_id"),
            "text": if short.is_empty() { say!(self, "(pièce jointe)", "(attachment)") } else { short },
            "routed_at": now(),
        }));
        if first {
            let _ = self.notify(
                &say!(
                    self,
                    "Bonjour ! Pour me parler, écrivez-moi dans le groupe « {title} » : c’est là que je lis vos messages.",
                    "Hello! To talk to me, write in the group \"{title}\": that's where I read your messages."
                ),
                Notice { chat, ..Notice::default() },
            );
        }
        Ok(1)
    }

    /// Files a Telegram update; 1 when it comes from another account or an
    /// unregistered group (ignored), 0 otherwise.
    pub fn dispatch(&self, update: &Object, settings: &Object, at: f64) -> Result<i32> {
        if let Some(callback) = update.object("callback_query") {
            if nested_int(callback, "from", "id") != settings.int("user_id") {
                return Ok(1);
            }
            self.choose(callback)?;
            return Ok(0);
        }
        if update.contains_key("my_chat_member") {
            return self.membership(update, settings);
        }
        let edited = update.contains_key("edited_message");
        let Some(message) = update.object("message").or_else(|| update.object("edited_message")) else {
            return Ok(0);
        };
        if GROUPS.contains(&nested_text(message, "chat", "type").unwrap_or("")) {
            return self.dispatch_group(update, message, edited, settings);
        }
        if message.object("from").is_some_and(|f| f.flag("is_bot")) {
            return Ok(0); // the bot's own messages (topic created, renamed…)
        }
        if nested_text(message, "chat", "type") != Some("private") {
            return Ok(1); // a channel
        }
        if nested_int(message, "from", "id") != settings.int("user_id") {
            return self.member_writes_privately(update, message, settings);
        }
        let chat = nested_int(message, "chat", "id");
        if chat.is_some() && chat != settings.int("chat_id") {
            self.update_config(obj(json!({"chat_id": chat})))?;
        }
        let mut text = message_text(message);
        if !edited && self.command(text.trim(), message)? {
            return Ok(0);
        }
        if text.is_empty() && !has_media(message) {
            return Ok(0); // service message (topic created, renamed…)
        }
        let original = if edited {
            self.find(message.int("message_id"), None)
        } else {
            None
        };
        let (name, how, candidates) = match original
            .as_ref()
            .and_then(|o| o.text("session").filter(|s| !s.is_empty()))
        {
            Some(session) => {
                let known = self.sessions();
                if let Some(addressed) = address(&text) {
                    if Self::resolve(&normal(&addressed.typed), &known, &known) == [session.to_string()] {
                        text = addressed.rest.trim().to_string();
                    }
                }
                (Some(session.to_string()), "modifié", Vec::new())
            }
            None => {
                let routed = self.route(message, &text, at);
                text = routed.text;
                (routed.session, routed.how, routed.candidates)
            }
        };
        let update_id = update.int("update_id").unwrap_or(0);
        let body = self.describe(message, update_id, &text, &self.owner());
        let body = if edited { say!(self, "(message modifié) {body}", "(edited message) {body}") } else { body };
        let entry = obj(json!({
            "at": message.int("edit_date").or_else(|| message.int("date")),
            "from": FROM_OWNER, "message_id": message.int("message_id"), "update_id": update_id,
            "text": body, "thread_id": topic_of(message),
        }));
        match name {
            Some(name) => self.deliver(&name, &entry, how)?,
            None => self.ask(&entry, candidates)?,
        }
        Ok(0)
    }

    /// Updates already filed (recent journal, inboxes, questions): a reading
    /// replayed after a cut doesn't deliver them twice.
    fn handled_ids(&self) -> HashSet<i64> {
        let entries = self.store.entries();
        let mut ids: HashSet<i64> = entries
            .iter()
            .rev()
            .take(1000)
            .filter_map(|e| e.int("update_id").filter(|id| *id != 0))
            .collect();
        let mut folders = vec![self.store.path(&["pending"])];
        if let Ok(read) = std::fs::read_dir(self.store.path(&["inbox"])) {
            folders.extend(read.filter_map(|e| e.ok().map(|e| e.path())));
        }
        for folder in folders {
            for file in self.store.json_files(&folder) {
                if let Some(id) = file.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()) {
                    ids.insert(id);
                }
            }
        }
        ids
    }

    /// One long poll of Telegram, for every session; returns how many
    /// updates were ignored.
    pub fn poll_once(&self, wait: u64) -> Result<i32> {
        let offset_file = self.store.path(&["offset"]);
        let offset: i64 = std::fs::read_to_string(&offset_file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        let updates = self.tg.call(
            "getUpdates",
            &Params::new()
                .set("offset", offset)
                .set("timeout", wait)
                .set("allowed_updates", UPDATES),
            Duration::from_secs(wait + 20),
            None,
        )?;
        let updates = updates.as_array().cloned().unwrap_or_default();
        let done = if updates.is_empty() { HashSet::new() } else { self.handled_ids() };
        let mut ignored = 0;
        for update in updates {
            let update = obj(update);
            let Some(update_id) = update.int("update_id") else {
                continue;
            };
            if !done.contains(&update_id) {
                let filed = self.config().and_then(|settings| self.dispatch(&update, &settings, now()));
                match filed {
                    Ok(count) => ignored += count,
                    Err(error) => self.warn_unfiled(&update, &error),
                }
            }
            std::fs::create_dir_all(self.store.root())?;
            std::fs::write(&offset_file, (update_id + 1).to_string())?;
        }
        Ok(ignored)
    }

    /// An unreadable update must not block the next ones: the owner is told.
    fn warn_unfiled(&self, update: &Object, error: &anyhow::Error) {
        let update_id = update.int("update_id").unwrap_or(0);
        eprintln!("asp telegram: update {update_id} not filed ({error})");
        let chat = update
            .object("message")
            .or_else(|| update.object("edited_message"))
            .or_else(|| update.object("my_chat_member"))
            .and_then(|m| m.object("chat"))
            .cloned()
            .unwrap_or_default();
        let place = chat
            .text("title")
            .map(str::to_string)
            .or_else(|| chat.int("id").map(|id| id.to_string()))
            .unwrap_or_default();
        let warning = if update.contains_key("my_chat_member") {
            say!(self, "⚠️ Un changement de mes droits dans « {place} » n’a pas pu être noté ({error}).", "⚠️ A change of my rights in \"{place}\" couldn't be recorded ({error}).")
        } else if update.contains_key("callback_query") {
            say!(self, "⚠️ Ton choix de session n’a pas pu être pris en compte ({error}).", "⚠️ Your choice of session couldn't be applied ({error}).")
        } else if GROUPS.contains(&chat.text("type").unwrap_or("")) {
            say!(self, "⚠️ Un message du groupe « {place} » n’a pas pu être relayé ({error}).", "⚠️ A message from group \"{place}\" couldn't be passed on ({error}).")
        } else {
            say!(self, "⚠️ Un de tes messages n’a pas pu être relayé ({error}). Renvoie-le autrement.", "⚠️ One of your messages couldn't be passed on ({error}). Send it another way.")
        };
        let _ = self.notify(&warning, Notice::default());
    }

    /// Once per message: tell the owner when its session hasn't taken it
    /// after 3 min.
    pub(crate) fn warn_stale(&self, at: f64) -> Result<()> {
        let marks = self.store.path(&["prevenus"]);
        if let Ok(read) = std::fs::read_dir(&marks) {
            for mark in read.filter_map(|e| e.ok()) {
                let old = mark
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .is_some_and(|m| at - m.as_secs_f64() > 86400.0);
                if old {
                    let _ = std::fs::remove_file(mark.path());
                }
            }
        }
        let Ok(read) = std::fs::read_dir(self.store.path(&["inbox"])) else {
            return Ok(());
        };
        let known = self.sessions();
        for folder in read.filter_map(|e| e.ok().map(|e| e.path())) {
            let Some(name) = folder.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
                continue;
            };
            if !folder.is_dir() || self.listening(&name) {
                continue;
            }
            for file in self.store.json_files(&folder) {
                let Some(file_name) = file.file_name() else {
                    continue;
                };
                let mark = marks.join(file_name);
                if mark.exists() {
                    continue;
                }
                let Some(entry) = self.store.read_object(&file) else {
                    continue;
                };
                let routed = entry.get("routed_at").and_then(Value::as_f64).unwrap_or(at);
                if at - routed < STALE {
                    continue;
                }
                std::fs::create_dir_all(&marks)?;
                std::fs::write(&mark, "")?;
                let who = tag(&name);
                let when = stamp(entry.float("at"), at);
                if entry.int("chat_id").is_some_and(|chat| chat != 0) {
                    // A group message: the owner is told privately, without quoting it.
                    let speaker = entry.text("speaker").unwrap_or("").to_string();
                    let group = entry.text("group").unwrap_or("").to_string();
                    let _ = self.notify(
                        &say!(
                            self,
                            "⏳ {who} n’écoute pas en ce moment : le message de {speaker} dans le groupe « {group} » ({when}) l’attend, elle le lira dès qu’elle écoutera.",
                            "⏳ {who} isn't listening right now: {speaker}'s message in group \"{group}\" ({when}) waits for it, it will read it as soon as it listens."
                        ),
                        Notice { session: Some(name.clone()), ..Notice::default() },
                    );
                    self.tell_group_absent(&entry, &name, at);
                    continue;
                }
                let _ = self.notify(
                    &say!(
                        self,
                        "⏳ {who} n’écoute pas en ce moment : ton message de {when} l’attend, elle le lira dès qu’elle écoutera.",
                        "⏳ {who} isn't listening right now: your message from {when} waits for it, it will read it as soon as it listens."
                    ),
                    Notice {
                        thread: known.get(&name).and_then(|d| d.int("thread_id")),
                        reply_to: entry.int("message_id"),
                        session: Some(name.clone()),
                        ..Notice::default()
                    },
                );
            }
        }
        Ok(())
    }

    /// In the group, once every 6 h at most: the session isn't there, the
    /// owner is told, the message waits. Nothing more (no project, no Mac).
    fn tell_group_absent(&self, entry: &Object, name: &str, at: f64) {
        let moved = self.migrations();
        let chat = self.latest(entry.int("chat_id").unwrap_or(0), Some(&moved));
        let Some(group) = self.groups().get(&chat).cloned() else {
            return;
        };
        if group.text("session") != Some(name) {
            return;
        }
        let recently = self.store.entries().iter().any(|e| {
            e.text("event") == Some("absente")
                && at - e.float("at") < ABSENT_NOTICE
                && self.latest(e.int("chat_id").unwrap_or(0), Some(&moved)) == chat
        });
        if recently {
            return;
        }
        let owner = self.owner();
        let text = say!(
            self,
            "Claude n’est pas disponible pour le moment. Votre message est bien gardé : il le lira dès son retour, et {owner} est prévenu.",
            "Claude isn't available right now. Your message is kept safe: it will read it as soon as it's back, and {owner} has been told."
        );
        let Ok((message, chat, _)) = self.group_send(
            "sendMessage",
            chat,
            &group,
            Params::new().set("text", text),
            API_TIMEOUT,
            None,
        ) else {
            return;
        };
        self.store.journal(&json!({
            "at": at, "message_id": message.get("message_id"), "from": "bot", "event": "absente",
            "session": name, "chat_id": chat, "group": group.get("title"),
            "text": say!(self, "[groupe prévenu : session absente]", "[group told: session away]"),
        }));
    }

    /// A message from this machine (not from Telegram) into a session's
    /// inbox, shown by its listener under `speaker`.
    pub fn drop_in_inbox(&self, name: &str, speaker: &str, text: &str) -> Result<()> {
        let at = now();
        let entry = json!({
            "at": at, "from": "asp", "speaker": speaker, "message_id": null, "update_id": null,
            "thread_id": null, "text": text, "session": name, "how": "moniteur", "routed_at": at,
        });
        let file = format!("asp-{}.json", (at * 1000.0) as u64);
        self.store.write(&self.store.path(&["inbox", name, &file]), &entry)?;
        self.store.journal(&entry);
        Ok(())
    }

    pub fn take(&self, name: &str) -> Vec<Object> {
        let mut files = self.store.json_files(&self.store.path(&["inbox", name]));
        files.sort_by_key(|file| {
            file.file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0)
        });
        let mut received = Vec::new();
        for file in files {
            let entry = self.store.read_object(&file);
            let _ = std::fs::remove_file(&file);
            received.extend(entry);
        }
        received
    }

    /// An album (several photos at once) arrives as several messages: a few
    /// more seconds to return the whole album in one wake-up.
    fn album_tail(&self, name: &str, leader: bool) -> Vec<Object> {
        let mut extra = Vec::new();
        for _ in 0..3 {
            if !leader || self.poll_once(1).is_err() {
                std::thread::sleep(Duration::from_secs(1));
            }
            extra.extend(self.take(name));
        }
        extra
    }

    /// Waits for this session's messages; reads Telegram for all of them when
    /// no other listener does. Returns (messages, ignored).
    pub fn listen(&self, name: &str, max_seconds: u64) -> Result<(Vec<Object>, i32)> {
        let Some(mine) = self.store.lock(&format!("session.{name}"), false) else {
            return Err(self.busy(name).into());
        };
        let result = self.listen_holding(name, max_seconds);
        let _ = self.update_session(name, obj(json!({"last_listen": now()})));
        drop(mine);
        result
    }

    fn busy(&self, name: &str) -> Busy {
        Busy(say!(
            self,
            "« {name} » écoute déjà (une autre session sous ce nom, ou ta propre écoute en tâche de fond)",
            "\"{name}\" is already listening (another session under this name, or your own background listener)"
        ))
    }

    fn listen_holding(&self, name: &str, max_seconds: u64) -> Result<(Vec<Object>, i32)> {
        self.update_session(name, obj(json!({"last_listen": now(), "stopped": false})))?;
        let span = if max_seconds == 0 { FOREVER } else { max_seconds as f64 };
        let deadline = now() + span;
        let (mut beat, mut pause, mut ignored) = (now(), 5.0f64, 0);
        let mut leader: Option<Lock> = None;
        let left = |deadline: f64| (deadline - now()).max(1.0);
        loop {
            let mut received = self.take(name);
            if !received.is_empty() {
                if received.iter().any(|entry| entry.flag("album")) {
                    received.extend(self.album_tail(name, leader.is_some()));
                }
                for entry in &received {
                    self.seen(entry);
                }
                return Ok((received, ignored));
            }
            let at = now();
            if at >= deadline {
                return Ok((Vec::new(), ignored));
            }
            if at - beat >= 60.0 {
                self.update_session(name, obj(json!({"last_listen": at})))?;
                beat = at;
            }
            if leader.is_none() {
                leader = self.store.lock("poll", false);
                if leader.is_none() {
                    // Another listener reads Telegram and fills our inbox.
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }
            match self.poll_once(POLL_WAIT.min(left(deadline)) as u64) {
                Ok(count) => {
                    ignored += count;
                    self.warn_stale(now())?;
                    pause = 5.0;
                }
                Err(error) => {
                    let Some(api) = api_error(&error) else {
                        return Err(error);
                    };
                    match api {
                        ApiError::Refused { code: 409, .. } => {
                            // Another reader still holds Telegram: let it finish.
                            leader = None;
                            std::thread::sleep(Duration::from_secs(5));
                        }
                        ApiError::Refused { code, retry_after, .. } if *code == 429 || *code >= 500 => {
                            let wait = retry_after.map_or(pause, |r| r as f64).min(left(deadline));
                            std::thread::sleep(Duration::from_secs_f64(wait));
                            pause = (pause * 2.0).min(60.0);
                        }
                        ApiError::Refused { .. } => return Err(error),
                        ApiError::Network(_) => {
                            // Passing network cut (timeout, DNS, sleep): the offset didn't move.
                            std::thread::sleep(Duration::from_secs_f64(pause.min(left(deadline))));
                            pause = (pause * 2.0).min(60.0);
                        }
                    }
                }
            }
        }
    }

    pub fn start(&self, name: &str, aliases: &[String], announce_group: bool) -> Result<Object> {
        if self.listening(name) {
            return Err(self.busy(name).into());
        }
        let before: Vec<String> = self
            .sessions()
            .get(name)
            .map(|d| d.list("aliases").iter().filter_map(|a| a.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let mut all: Vec<String> = before
            .into_iter()
            .chain(aliases.iter().map(|a| normal(a)))
            .filter(|a| !a.is_empty() && a != name)
            .collect();
        all.sort();
        all.dedup();
        let cwd = std::env::current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let data = self.update_session(
            name,
            obj(json!({"cwd": cwd, "stopped": false, "started": now(), "aliases": all})),
        )?;
        // The owner's menu in private; in project groups, only /aide.
        let menus = [
            (
                "all_private_chats",
                json!([
                    {"command": "sessions", "description": say!(self, "Les sessions Claude Code et leur état", "Claude Code sessions and their state")},
                    {"command": "aide", "description": say!(self, "Comment écrire à une session", "How to write to a session")},
                ]),
            ),
            (
                "all_group_chats",
                json!([{"command": "aide", "description": say!(self, "Comment me parler ici", "How to talk to me here")}]),
            ),
        ];
        for (scope, commands) in menus {
            let _ = self.api(
                "setMyCommands",
                Params::new()
                    .json("commands", &commands)
                    .json("scope", &json!({"type": scope})),
            );
        }
        let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
        let place = data.text("cwd").unwrap_or("");
        let place = match place.strip_prefix(&home) {
            Some(rest) if !home.is_empty() => format!("~{rest}"),
            _ => place.to_string(),
        };
        let bound = self.group_of(name);
        if let (Some((chat, group)), true) = (&bound, announce_group) {
            let title = group.text("title").unwrap_or("");
            self.send_group(
                &say!(
                    self,
                    "Bonjour ! Je suis là et je vous écoute. Écrivez-moi ici ce que vous souhaitez pour « {title} » : un message, une photo ou un vocal.",
                    "Hello! I'm here and listening. Write here what you'd like for \"{title}\": a message, a photo or a voice note."
                ),
                name,
                *chat,
                group,
            )?;
            return Ok(data);
        }
        let mut hello = if self.thread_of(name).is_some() {
            say!(self, "Je t’écoute depuis {place}. Écris-moi dans ce fil.", "Listening from {place}. Write to me in this topic.")
        } else {
            let short = std::iter::once(name.to_string())
                .chain(data.list("aliases").iter().filter_map(|a| a.as_str().map(str::to_string)))
                .min_by_key(|candidate| candidate.chars().count())
                .unwrap_or_else(|| name.to_string());
            say!(
                self,
                "Je t’écoute depuis {place}. Pour m’écrire : commence ton message par « {short}: », ou réponds à un de mes messages.",
                "Listening from {place}. To write to me: start your message with \"{short}:\", or reply to one of my messages."
            )
        };
        if let Some((_, group)) = &bound {
            let title = group.text("title").unwrap_or("");
            hello.push('\n');
            hello.push_str(&say!(self, "Je réponds aussi dans le groupe « {title} ».", "I also answer in the group \"{title}\"."));
        }
        self.send(&hello, name, true)?;
        Ok(data)
    }

    pub fn stop(&self, name: &str, announce_group: bool) -> Result<()> {
        self.update_session(name, obj(json!({"stopped": true})))?;
        match (self.group_of(name), announce_group) {
            (Some((chat, group)), true) => {
                self.send_group(
                    &say!(
                        self,
                        "Je fais une pause pour le moment. Vos messages ici seront lus à mon retour.",
                        "I'm taking a break for now. Your messages here will be read when I'm back."
                    ),
                    name,
                    chat,
                    &group,
                )?;
            }
            _ => {
                self.send(
                    &say!(
                        self,
                        "Je ne t’écoute plus. Relance /telegram dans ce projet pour reprendre.",
                        "I've stopped listening. Run /telegram in this project again to resume."
                    ),
                    name,
                    true,
                )?;
            }
        }
        Ok(())
    }

    pub fn old_listeners(&self) -> Vec<(u32, String)> {
        (self.old_listeners)()
    }

    /// What `doctor` prints.
    pub fn doctor(&self) -> Result<Vec<String>> {
        let me = self.api("getMe", Params::new())?;
        let me = obj(me);
        let settings = self.config()?;
        let mut lines = Vec::new();
        let username = me.text("username").unwrap_or("?");
        let user = settings.int("user_id").map_or("?".to_string(), |u| u.to_string());
        let chat = if settings.int("chat_id").is_some() {
            say!(self, "connue", "known")
        } else {
            say!(self, "inconnue (attendre /start)", "unknown (wait for /start)")
        };
        let topics = if me.flag("has_topics_enabled") {
            say!(self, "actif", "on")
        } else {
            say!(self, "inactif (BotFather)", "off (BotFather)")
        };
        lines.push(say!(
            self,
            "Bot @{username} joignable ; compte écouté {user} ; conversation {chat} ; mode fils {topics}",
            "Bot @{username} reachable; listening to account {user}; chat {chat}; threaded mode {topics}"
        ));
        let listeners: Vec<String> = self
            .sessions()
            .into_keys()
            .filter(|name| self.listening(name))
            .collect();
        let listeners = if listeners.is_empty() {
            say!(self, "aucune session", "no session")
        } else {
            listeners.join(", ")
        };
        let reading = if self.store.held("poll") {
            say!(self, "en cours", "in progress")
        } else {
            say!(self, "libre", "free")
        };
        let pending = self.store.json_files(&self.store.path(&["pending"])).len();
        lines.push(say!(
            self,
            "À l’écoute : {listeners} ; lecture de Telegram {reading} ; {pending} question(s) en attente",
            "Listening: {listeners}; Telegram reading {reading}; {pending} question(s) pending"
        ));
        for (chat, data) in self.groups() {
            let status = match self.api(
                "getChatMember",
                Params::new()
                    .set("chat_id", chat)
                    .set("user_id", me.int("id").unwrap_or(0)),
            ) {
                Ok(member) => member.get("status").and_then(Value::as_str).unwrap_or("?").to_string(),
                Err(error) => say!(self, "injoignable ({error})", "unreachable ({error})"),
            };
            let title = data.text("title").unwrap_or("");
            let session = data.text("session").unwrap_or("");
            let warning = if matches!(status.as_str(), "administrator" | "creator") {
                String::new()
            } else {
                say!(self, " (doit être administrateur pour tout lire)", " (must be administrator to read everything)")
            };
            lines.push(say!(
                self,
                "Groupe « {title} » ({chat}) → {session} : bot {status}{warning}",
                "Group \"{title}\" ({chat}) → {session}: bot {status}{warning}"
            ));
        }
        let old = self.old_listeners();
        if !old.is_empty() {
            let shown = old
                .iter()
                .map(|(pid, session)| format!("{session} (pid {pid})"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(say!(
                self,
                "Écoutes encore sur l’ancien code (à relancer) : {shown}",
                "Listeners still on old code (restart them): {shown}"
            ));
        }
        Ok(lines)
    }
}

trait Merge {
    fn merge(self, other: Params) -> Params;
}

impl Merge for Params {
    /// `other`'s values win.
    fn merge(self, other: Params) -> Params {
        other
            .pairs()
            .iter()
            .fold(self, |merged, (key, value)| merged.set(key, value))
    }
}
