//! The bot against a fake Telegram: routing, topics, groups, listeners.
//! Scenarios ported from sync-telegram's test suite, so both behave alike.

use super::api::{ApiError, Params, Relay, Telegram};
use super::bot::{within, Bot, Extras, Notice};
use super::store::{now, Fields, Lock, Object, Store};
use super::text::tag;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

const OWNER: i64 = 42;
const ALICE: i64 = 7000000001;
const GROUP: i64 = -1001234;
const QUESTION: &str =
    "Plusieurs sessions Claude Code t’écoutent, une par projet : pour laquelle est ce message ?";

#[derive(Default)]
struct FakeState {
    calls: Vec<(String, Params)>,
    updates: Vec<Value>,
    next_id: i64,
    next_thread: i64,
    topics: bool,
    conflicts: u32,
    lost_thread: bool,
    wrong_home: bool,
    refuse_download: Option<String>,
    migrated: HashMap<i64, i64>,
}

struct Fake(Rc<RefCell<FakeState>>);

fn refused(method: &str, code: i64, description: &str, migrate_to: Option<i64>) -> ApiError {
    ApiError::Refused {
        method: method.to_string(),
        code,
        description: description.to_string(),
        retry_after: None,
        migrate_to,
    }
}

impl Telegram for Fake {
    fn call(&self, method: &str, params: &Params, _: Duration, document: Option<&Path>) -> Result<Value, ApiError> {
        let mut state = self.0.borrow_mut();
        let mut recorded = params.clone();
        if let Some(file) = document {
            recorded = recorded.set("document", file.file_name().unwrap().to_string_lossy());
        }
        state.calls.push((method.to_string(), recorded));
        let chat = params.get("chat_id").and_then(|c| c.parse::<i64>().ok());
        if let Some(new) = chat.and_then(|chat| state.migrated.get(&chat).copied()) {
            return Err(refused(method, 400, "Bad Request: group chat was upgraded to a supergroup chat", Some(new)));
        }
        match method {
            "sendMessage" | "sendDocument" => {
                if method == "sendMessage" && state.lost_thread && params.get("message_thread_id").is_some() {
                    state.lost_thread = false;
                    return Err(refused(method, 400, "Bad Request: message thread not found", None));
                }
                state.next_id += 1;
                Ok(json!({"message_id": state.next_id}))
            }
            "getUpdates" => {
                if state.conflicts > 0 {
                    state.conflicts -= 1;
                    return Err(refused(method, 409, "Conflict: terminated by other getUpdates request", None));
                }
                let offset: i64 = params.get("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
                let fresh: Vec<Value> = state
                    .updates
                    .iter()
                    .filter(|u| u["update_id"].as_i64().unwrap_or(0) >= offset)
                    .cloned()
                    .collect();
                Ok(Value::Array(fresh))
            }
            "getMe" => Ok(json!({"id": 1000000001i64, "username": "TestBot", "has_topics_enabled": state.topics})),
            "getChatMember" => Ok(json!({"status": "administrator"})),
            "createForumTopic" => {
                if state.wrong_home {
                    return Err(refused(method, 400, "Bad Request: BOT_TOPIC_WRONG_HOME", None));
                }
                state.next_thread += 1;
                Ok(json!({"message_thread_id": state.next_thread - 1, "name": params.get("name")}))
            }
            "getFile" => match &state.refuse_download {
                Some(reason) => Err(refused(method, 400, reason, None)),
                None => Ok(json!({"file_path": format!("files/{}", params.get("file_id").unwrap_or(""))})),
            },
            _ => Ok(Value::Bool(true)),
        }
    }

    fn download(&self, _: &str, target: &Path) -> Result<(), ApiError> {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, b"data").unwrap();
        Ok(())
    }
}

/// The relay: updates waiting for this machine, and every call it got.
#[derive(Default)]
struct RelayState {
    calls: Vec<(String, Option<Value>)>,
    updates: Vec<Value>,
}

struct FakeRelay(Rc<RefCell<RelayState>>);

impl Relay for FakeRelay {
    fn call(&self, url: &str, route: &str, body: Option<&Value>) -> anyhow::Result<Value> {
        assert_eq!(url, RELAY_URL);
        let mut state = self.0.borrow_mut();
        state.calls.push((route.to_string(), body.cloned()));
        match route {
            "/releve" => {
                let ack = body.and_then(|b| b["ack"].as_i64()).unwrap_or(0);
                state.updates.retain(|u| u["update_id"].as_i64().unwrap_or(0) > ack);
                Ok(json!({"updates": state.updates.clone()}))
            }
            "/etat" => Ok(json!({"maintenant": 1000.0, "principale": "mac",
                                 "machines": [{"name": "mac", "vue": 990.0}, {"name": "pc", "vue": 400.0}],
                                 "en_attente": [{"machine": "pc", "n": 2}]})),
            _ => Ok(json!({"ok": true})),
        }
    }
}

const RELAY_URL: &str = "https://relay.example";

struct Harness {
    dir: tempfile::TempDir,
    bot: Bot,
    tg: Rc<RefCell<FakeState>>,
    relay: Rc<RefCell<RelayState>>,
    locks: Vec<Lock>,
}

fn obj(value: Value) -> Object {
    value.as_object().cloned().unwrap()
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    store
        .write(
            &store.path(&["bot.json"]),
            &json!({"user_id": OWNER, "chat_id": OWNER, "name": "Sam", "language": "fr", "night": [],
                    "username": "TestBot", "topics": {"enabled": false, "checked_at": now()}}),
        )
        .unwrap();
    let tg = Rc::new(RefCell::new(FakeState {
        next_id: 500,
        next_thread: 77,
        ..FakeState::default()
    }));
    let relay = Rc::new(RefCell::new(RelayState::default()));
    let bot = Bot::new(store, Box::new(Fake(tg.clone())), Box::new(FakeRelay(relay.clone())), Box::new(Vec::new));
    Harness { dir, bot, tg, relay, locks: Vec::new() }
}

fn message(update_id: i64, message_id: i64, text: &str, extra: Value) -> Value {
    let mut message = json!({"message_id": message_id, "date": now() as i64, "from": {"id": OWNER},
                             "chat": {"id": OWNER, "type": "private"}, "text": text});
    message.as_object_mut().unwrap().extend(obj(extra));
    json!({"update_id": update_id, "message": message})
}

fn group_message(update_id: i64, message_id: i64, text: &str, sender: i64, name: &str, chat: i64, extra: Value) -> Value {
    let mut message = json!({"message_id": message_id, "date": now() as i64,
                             "from": {"id": sender, "first_name": name},
                             "chat": {"id": chat, "type": "supergroup", "title": "Site d’Alice"}, "text": text});
    message.as_object_mut().unwrap().extend(obj(extra));
    json!({"update_id": update_id, "message": message})
}

fn alice(update_id: i64, message_id: i64, text: &str) -> Value {
    group_message(update_id, message_id, text, ALICE, "Alice", GROUP, json!({}))
}

fn buttons(params: &Params) -> Vec<String> {
    let markup: Value = serde_json::from_str(params.get("reply_markup").unwrap()).unwrap();
    markup["inline_keyboard"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row.as_array().unwrap().iter())
        .map(|b| b["callback_data"].as_str().unwrap().to_string())
        .collect()
}

impl Harness {
    fn session(&mut self, name: &str, listening: bool, extra: Value) {
        let mut changes = obj(json!({"cwd": format!("/projets/{name}"), "last_listen": now()}));
        changes.extend(obj(extra));
        self.bot.update_session(name, changes).unwrap();
        if listening {
            self.locks.push(self.bot.store.lock(&format!("session.{name}"), false).unwrap());
        }
    }

    fn live(&mut self, name: &str) {
        self.session(name, true, json!({}));
    }

    fn dispatch_at(&self, update: Value, at: f64) -> i32 {
        let settings = self.bot.config().unwrap();
        self.bot.dispatch(&obj(update), &settings, at).unwrap()
    }

    fn dispatch(&self, update: Value) -> i32 {
        self.dispatch_at(update, now())
    }

    fn inbox(&self, name: &str) -> Vec<String> {
        self.bot
            .take(name)
            .iter()
            .map(|e| e.text("text").unwrap_or("").to_string())
            .collect()
    }

    fn choose(&self, update_id: i64, data: &str) {
        self.dispatch(json!({"update_id": update_id, "callback_query": {
            "id": format!("q{update_id}"), "from": {"id": OWNER}, "data": data, "message": {"message_id": 900}}}));
    }

    fn calls(&self, method: &str) -> Vec<Params> {
        self.tg
            .borrow()
            .calls
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    fn sent(&self) -> Vec<Params> {
        self.calls("sendMessage")
    }

    fn last_sent(&self) -> Params {
        self.sent().pop().expect("a sent message")
    }

    fn topics(&self) {
        self.bot
            .update_config(obj(json!({"topics": {"enabled": true, "checked_at": now()}})))
            .unwrap();
    }

    fn text_of(params: &Params) -> String {
        params.get("text").unwrap_or("").to_string()
    }

    fn group(&self) {
        let members = BTreeMap::from([(ALICE, "Alice".to_string()), (OWNER, "Sam".to_string())]);
        self.bot.group_add(GROUP, "alice-site", &members, Some("Site d’Alice"), false).unwrap();
    }

    fn offset(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("offset")).unwrap()
    }

    /// The bot reads through a relay, this machine under `prefix`.
    fn relayed(&self, prefix: &str) {
        let relay = json!({"url": RELAY_URL, "machine": "mac", "prefixe": prefix, "principale": prefix.is_empty()});
        self.bot.update_config(obj(json!({"relais": relay}))).unwrap();
    }

    fn relay_calls(&self, route: &str) -> Vec<Value> {
        self.relay
            .borrow()
            .calls
            .iter()
            .filter(|(r, _)| r == route)
            .map(|(_, body)| body.clone().unwrap_or(Value::Null))
            .collect()
    }
}

// Routing.

#[test]
fn a_leading_name_picks_the_session_and_is_dropped() {
    let mut h = harness();
    h.live("acme-backend");
    h.live("nova-studio");
    h.dispatch(message(1, 10, "backend : déploie la v336", json!({})));
    h.dispatch(message(2, 11, "@nova relance la campagne", json!({})));
    assert_eq!(h.inbox("acme-backend"), ["déploie la v336"]);
    assert_eq!(h.inbox("nova-studio"), ["relance la campagne"]);
}

#[test]
fn an_exact_alias_beats_an_ambiguous_start() {
    let mut h = harness();
    h.session("acme-backend", true, json!({"aliases": ["acme"]}));
    h.live("acme-landing");
    h.dispatch(message(1, 12, "acme: le point client", json!({})));
    assert_eq!(h.inbox("acme-backend"), ["le point client"]);
}

#[test]
fn an_ambiguous_start_offers_only_the_sessions_it_matches() {
    let mut h = harness();
    h.live("acme-backend");
    h.live("acme-landing");
    h.live("nova-studio");
    h.dispatch(message(1, 13, "acme: on déploie ?", json!({})));
    assert_eq!(buttons(&h.last_sent()), ["to:1:acme-backend", "to:1:acme-landing", "no:1"]);
    h.choose(2, "to:1:acme-landing");
    assert_eq!(h.inbox("acme-landing"), ["on déploie ?"]);
}

#[test]
fn an_ordinary_colon_is_not_an_address() {
    let mut h = harness();
    h.live("acme-backend");
    let text = "Question : on livre ce soir ? https://meet.example.com";
    h.dispatch(message(1, 14, text, json!({})));
    assert_eq!(h.inbox("acme-backend"), [text]);
}

#[test]
fn a_reply_goes_to_its_session_with_the_quote() {
    let mut h = harness();
    h.live("acme-backend");
    h.live("nova-studio");
    let sent = h.bot.send("Fiche client prête, je l’envoie ?", "nova-studio", false).unwrap()[0];
    let quoted = json!({"message_id": sent, "text": format!("{}\nFiche client prête, je l’envoie ?", tag("nova-studio", ""))});
    h.dispatch(message(1, 20, "oui", json!({"reply_to_message": quoted})));
    assert_eq!(h.inbox("nova-studio"), ["[en réponse à « Fiche client prête, je l’envoie ? »]\noui"]);
    assert!(h.inbox("acme-backend").is_empty());
}

#[test]
fn without_an_address_and_two_sessions_the_bot_asks_once() {
    let mut h = harness();
    h.live("acme-backend");
    h.live("nova-studio");
    h.dispatch(message(1, 30, "tu en es où ?", json!({})));
    let prompt = h.last_sent();
    assert_eq!(Harness::text_of(&prompt), QUESTION);
    let reply: Value = serde_json::from_str(prompt.get("reply_parameters").unwrap()).unwrap();
    assert_eq!(reply["message_id"], 30);
    assert!(buttons(&prompt).contains(&"to:1:nova-studio".to_string()));
    assert!(h.inbox("nova-studio").is_empty());
    h.choose(2, "to:1:nova-studio");
    assert_eq!(h.inbox("nova-studio"), ["tu en es où ?"]);
    let edits = h.calls("editMessageText");
    assert_eq!(edits.last().unwrap().get("text").unwrap(), format!("→ {}", tag("nova-studio", "")));
    h.choose(3, "to:1:nova-studio"); // a second tap: nothing more
    assert!(h.inbox("nova-studio").is_empty());
}

#[test]
fn leaving_it_aside_delivers_nothing() {
    let mut h = harness();
    h.live("acme-backend");
    h.live("nova-studio");
    h.dispatch(message(1, 31, "brouillon", json!({})));
    h.choose(2, "no:1");
    assert!(h.inbox("nova-studio").is_empty() && h.inbox("acme-backend").is_empty());
    assert!(h.bot.store.json_files(&h.dir.path().join("pending")).is_empty());
}

#[test]
fn a_conversation_follows_for_three_minutes() {
    let mut h = harness();
    h.live("acme-backend");
    h.live("nova-studio");
    h.dispatch(message(1, 40, "nova: première consigne", json!({})));
    h.dispatch(message(2, 41, "et la deuxième", json!({})));
    assert_eq!(h.inbox("nova-studio"), ["première consigne", "et la deuxième"]);
    h.dispatch_at(message(3, 42, "autre chose", json!({})), now() + 600.0);
    assert_eq!(Harness::text_of(&h.last_sent()), QUESTION);
}

#[test]
fn the_only_active_session_gets_unaddressed_messages() {
    let mut h = harness();
    h.live("acme-backend");
    h.session("nova-studio", false, json!({"last_listen": now() - 3600.0}));
    h.dispatch(message(1, 43, "tu peux regarder ?", json!({})));
    assert_eq!(h.inbox("acme-backend"), ["tu peux regarder ?"]);
}

#[test]
fn outside_topics_a_message_carries_the_session_name() {
    let h = harness();
    h.bot.send("a < b & c", "nova-studio", false).unwrap();
    let last = h.last_sent();
    assert_eq!(last.get("parse_mode"), Some("HTML"));
    assert_eq!(Harness::text_of(&last), format!("<b>{}</b>\na &lt; b &amp; c", tag("nova-studio", "")));
    assert!(last.get("message_thread_id").is_none());
}

#[test]
fn threaded_mode_one_topic_per_session() {
    let mut h = harness();
    h.topics();
    h.live("acme-backend");
    h.live("nova-studio");
    h.bot.send("Bonjour", "nova-studio", false).unwrap();
    h.bot.send("Encore", "nova-studio", false).unwrap();
    let created = h.calls("createForumTopic");
    assert_eq!(created.iter().map(|p| p.get("name").unwrap()).collect::<Vec<_>>(), ["nova-studio"]);
    let last = h.last_sent();
    assert_eq!((last.get("message_thread_id"), last.get("text")), (Some("77"), Some("Encore")));
    assert!(last.get("parse_mode").is_none());
    h.dispatch(message(1, 50, "vas-y", json!({"message_thread_id": 77, "is_topic_message": true})));
    assert_eq!(h.inbox("nova-studio"), ["vas-y"]);
}

#[test]
fn a_file_goes_to_the_session_topic_with_its_caption() {
    let h = harness();
    h.topics();
    let pdf = h.dir.path().join("INV-000155-octobre-2026.pdf");
    std::fs::write(&pdf, b"%PDF-1.4").unwrap();
    h.bot.send_document(&pdf, "invoices", "INV-000155 (brouillon)", false, Extras::default()).unwrap();
    let sent = h.calls("sendDocument").pop().unwrap();
    assert_eq!(
        (sent.get("message_thread_id"), sent.get("caption"), sent.get("document")),
        (Some("77"), Some("INV-000155 (brouillon)"), Some("INV-000155-octobre-2026.pdf"))
    );
    assert_eq!(
        h.bot.store.entries().last().unwrap().text("text"),
        Some("[fichier] INV-000155-octobre-2026.pdf\nINV-000155 (brouillon)")
    );
}

#[test]
fn a_deleted_topic_is_created_again() {
    let h = harness();
    h.topics();
    h.bot.send("Bonjour", "nova-studio", false).unwrap();
    h.tg.borrow_mut().lost_thread = true;
    h.bot.send("Toujours là", "nova-studio", false).unwrap();
    assert_eq!(h.bot.sessions()["nova-studio"].int("thread_id"), Some(78));
    assert_eq!(h.last_sent().get("message_thread_id"), Some("78"));
}

#[test]
fn when_telegram_refuses_new_topics_a_session_takes_over_a_stopped_ones() {
    let mut h = harness();
    h.topics();
    h.tg.borrow_mut().wrong_home = true;
    h.session("old-site", false, json!({"thread_id": 50, "stopped": true, "last_listen": now() - 3600.0}));
    h.bot.send("Bonjour", "fable", false).unwrap();
    assert_eq!(h.last_sent().get("message_thread_id"), Some("50"));
    let sessions = h.bot.sessions();
    assert_eq!(sessions["fable"].int("thread_id"), Some(50));
    assert_eq!(sessions["old-site"].int("thread_id"), None);
    let renamed = h.calls("editForumTopic").pop().unwrap();
    assert_eq!((renamed.get("message_thread_id"), renamed.get("name")), (Some("50"), Some("fable")));
    h.dispatch(message(1, 60, "salut", json!({"message_thread_id": 50, "is_topic_message": true})));
    assert_eq!(h.inbox("fable"), ["salut"]);
}

#[test]
fn never_the_topic_of_a_live_bound_or_waiting_session() {
    let mut h = harness();
    h.topics();
    h.tg.borrow_mut().wrong_home = true;
    h.session("live", true, json!({"thread_id": 51}));
    h.session("recent", false, json!({"thread_id": 52, "last_listen": now() - 3600.0}));
    h.session("waiting", false, json!({"thread_id": 53, "stopped": true}));
    h.bot.store
        .write(&h.bot.store.path(&["inbox", "waiting", "1.json"]), &json!({"update_id": 1, "text": "x"}))
        .unwrap();
    h.group();
    h.session("alice-site", false, json!({"thread_id": 54, "stopped": true}));
    h.bot.start("fable", &[], false).unwrap();
    let hello = h.last_sent();
    assert!(hello.get("message_thread_id").is_none());
    assert!(Harness::text_of(&hello).contains("Telegram a refusé de me créer un fil"));
    assert_eq!(h.bot.sessions()["fable"].int("thread_id"), None);
    assert!(h.calls("editForumTopic").is_empty());
}

#[test]
fn the_longest_gone_session_gives_up_its_topic_first() {
    let mut h = harness();
    h.topics();
    h.tg.borrow_mut().wrong_home = true;
    h.session("a", false, json!({"thread_id": 60, "stopped": true, "last_listen": now() - 100.0}));
    h.session("b", false, json!({"thread_id": 61, "last_listen": now() - 30.0 * 86400.0}));
    h.bot.send("Bonjour", "fable", false).unwrap();
    assert_eq!(h.bot.sessions()["fable"].int("thread_id"), Some(61));
    assert_eq!(h.bot.sessions()["a"].int("thread_id"), Some(60));
}

#[test]
fn an_unknown_topic_joins_the_chosen_session_and_is_renamed() {
    let mut h = harness();
    h.topics();
    h.live("acme-backend");
    h.live("nova-hub");
    h.dispatch(message(1, 60, "Bah je sais pas", json!({"message_thread_id": 35051, "is_topic_message": true})));
    let prompt = h.last_sent();
    assert_eq!((prompt.get("text"), prompt.get("message_thread_id")), (Some(QUESTION), Some("35051")));
    h.choose(2, "to:1:acme-backend");
    assert_eq!(h.inbox("acme-backend"), ["Bah je sais pas"]);
    let renamed = h.calls("editForumTopic").pop().unwrap();
    assert_eq!((renamed.get("message_thread_id"), renamed.get("name")), (Some("35051"), Some("acme-backend")));
    h.dispatch(message(3, 61, "et maintenant ?", json!({"message_thread_id": 35051, "is_topic_message": true})));
    assert_eq!(h.inbox("acme-backend"), ["et maintenant ?"]);
    h.bot.send("Réponse", "acme-backend", false).unwrap();
    assert_eq!(h.last_sent().get("message_thread_id"), Some("35051"));
    assert!(h.calls("createForumTopic").is_empty());
}

#[test]
fn the_session_answers_in_the_topic_it_was_written_in() {
    let mut h = harness();
    h.topics();
    h.live("nova-hub");
    h.bot.send("Bonjour", "nova-hub", false).unwrap(); // its topic: 77
    h.dispatch(message(1, 62, "nova: dans un fil à moi", json!({"message_thread_id": 500, "is_topic_message": true})));
    assert_eq!(h.inbox("nova-hub"), ["dans un fil à moi"]);
    h.bot.send("Vu", "nova-hub", false).unwrap();
    assert_eq!(h.last_sent().get("message_thread_id"), Some("500"));
    h.dispatch(message(2, 63, "retour au premier fil", json!({"message_thread_id": 77, "is_topic_message": true})));
    assert_eq!(h.inbox("nova-hub"), ["retour au premier fil"]);
    h.bot.send("Vu aussi", "nova-hub", false).unwrap();
    assert_eq!(h.last_sent().get("message_thread_id"), Some("77"));
}

#[test]
fn a_session_topic_beats_a_name() {
    let mut h = harness();
    h.topics();
    h.live("acme-backend");
    h.live("nova-hub");
    h.bot.send("Bonjour", "nova-hub", false).unwrap(); // topic 77
    h.dispatch(message(1, 64, "backend: écrit dans le fil evo", json!({"message_thread_id": 77, "is_topic_message": true})));
    assert_eq!(h.inbox("nova-hub"), ["backend: écrit dans le fil evo"]);
    assert!(h.inbox("acme-backend").is_empty());
    assert_eq!(h.bot.sessions()["acme-backend"].int("thread_id"), None);
}

#[test]
fn outside_topics_the_last_session_to_write_gets_the_answer() {
    let mut h = harness();
    h.live("acme-backend");
    h.live("nova-hub");
    h.bot.send("Active Threaded Mode puis dis-moi", "acme-backend", false).unwrap();
    h.dispatch_at(message(1, 65, "C’est fait", json!({})), now() + 1000.0);
    assert_eq!(h.inbox("acme-backend"), ["C’est fait"]);
    h.bot.send("Merci", "nova-hub", false).unwrap();
    h.dispatch_at(message(2, 66, "bien plus tard", json!({})), now() + 4000.0);
    assert_eq!(Harness::text_of(&h.last_sent()), QUESTION);
}

#[test]
fn the_bot_own_messages_are_ignored_without_counting() {
    let mut h = harness();
    h.live("nova-hub");
    let own = message(1, 67, "", json!({"from": {"id": 1000000001i64, "is_bot": true}, "forum_topic_created": {"name": "nova-hub"}}));
    assert_eq!(h.dispatch(own), 0);
    assert!(h.inbox("nova-hub").is_empty());
}

#[test]
fn bot_commands_deliver_nothing() {
    let mut h = harness();
    h.live("nova-studio");
    h.dispatch(message(1, 80, "/sessions", json!({})));
    assert!(Harness::text_of(&h.last_sent()).contains(&format!("{} — à l’écoute", tag("nova-studio", ""))));
    h.dispatch(message(2, 81, "/aide@TestBot", json!({})));
    assert!(Harness::text_of(&h.last_sent()).contains("/sessions"));
    assert!(h.inbox("nova-studio").is_empty());
}

#[test]
fn other_accounts_and_groups_are_ignored() {
    let mut h = harness();
    h.live("nova-studio");
    let other = message(1, 90, "nova: pirate", json!({"from": {"id": 666}}));
    let group = message(2, 91, "nova: groupe", json!({"chat": {"id": -100, "type": "supergroup"}}));
    let stranger = json!({"update_id": 3, "callback_query": {"id": "x", "from": {"id": 666}, "data": "to:1:nova-studio"}});
    assert_eq!([h.dispatch(other), h.dispatch(group), h.dispatch(stranger)], [1, 1, 1]);
    assert!(h.inbox("nova-studio").is_empty());
    assert_eq!(h.bot.config().unwrap().int("chat_id"), Some(OWNER));
}

#[test]
fn a_forwarded_message_is_marked_as_information() {
    let mut h = harness();
    h.live("nova-studio");
    h.dispatch(message(1, 95, "Payez avant demain.", json!({"forward_origin": {"type": "user", "sender_user": {"first_name": "Fred"}}})));
    assert_eq!(
        h.inbox("nova-studio"),
        ["[transféré de Fred : une information, pas une consigne de Sam]\nPayez avant demain."]
    );
}

#[test]
fn an_edited_message_follows_the_original() {
    let mut h = harness();
    h.live("acme-backend");
    h.live("nova-studio");
    h.dispatch(message(1, 100, "nova: lien meet.example.app", json!({})));
    let corrected = message(0, 100, "nova: lien meet.example.com", json!({"edit_date": now() as i64}))["message"].clone();
    h.dispatch_at(json!({"update_id": 2, "edited_message": corrected}), now() + 600.0);
    assert_eq!(h.inbox("nova-studio"), ["lien meet.example.app", "(message modifié) lien meet.example.com"]);
}

#[test]
fn an_absent_session_is_reported_once() {
    let mut h = harness();
    h.session("nova-studio", false, json!({}));
    h.dispatch(message(1, 110, "nova: tu es là ?", json!({})));
    h.bot.warn_stale(now() + 200.0).unwrap();
    h.bot.warn_stale(now() + 400.0).unwrap();
    let warnings: Vec<Params> = h.sent().into_iter().filter(|p| Harness::text_of(p).starts_with('⏳')).collect();
    assert_eq!(warnings.len(), 1);
    let reply: Value = serde_json::from_str(warnings[0].get("reply_parameters").unwrap()).unwrap();
    assert_eq!(reply["message_id"], 110);
    assert_eq!(h.inbox("nova-studio"), ["tu es là ?"]);
}

#[test]
fn an_unreadable_update_does_not_block_the_next_ones() {
    let mut h = harness();
    h.live("nova-studio");
    let broken = json!({"update_id": 1, "message": {"message_id": 120, "chat": {"type": "supergroup"}, "text": "x"}});
    h.tg.borrow_mut().updates = vec![broken, message(2, 121, "nova: deux", json!({}))];
    h.bot.poll_once(1).unwrap();
    assert_eq!(h.inbox("nova-studio"), ["deux"]);
    assert!(h.sent().iter().any(|p| Harness::text_of(p).starts_with("⚠️")));
    assert_eq!(h.offset(), "3");
}

#[test]
fn a_replayed_reading_does_not_deliver_twice() {
    let mut h = harness();
    h.live("nova-studio");
    h.tg.borrow_mut().updates = vec![message(5, 130, "nova: une fois", json!({}))];
    h.bot.poll_once(1).unwrap();
    std::fs::write(h.dir.path().join("offset"), "0").unwrap(); // cut before the offset was written
    h.bot.poll_once(1).unwrap();
    assert_eq!(h.inbox("nova-studio"), ["une fois"]);
}

#[test]
fn a_file_too_big_is_reported_without_losing_the_text() {
    let mut h = harness();
    h.live("nova-studio");
    h.tg.borrow_mut().refuse_download = Some("Bad Request: file is too big".to_string());
    h.dispatch(message(1, 150, "", json!({"caption": "nova: le PV", "document": {"file_id": "x", "file_name": "pv.pdf", "file_size": 25_000_000}})));
    assert_eq!(
        h.inbox("nova-studio"),
        ["le PV\n[document non téléchargé, 25 Mo : Telegram getFile: Bad Request: file is too big]"]
    );
}

#[test]
fn attachments_are_downloaded_next_to_the_update() {
    let mut h = harness();
    h.live("nova-studio");
    h.dispatch(message(1, 151, "", json!({"caption": "nova: regarde", "photo": [{"file_id": "petit"}, {"file_id": "grand"}]})));
    let photo = h.dir.path().join("attachments").join("1-photo.jpg");
    assert_eq!(h.inbox("nova-studio"), [format!("regarde\n[photo] {}", photo.display())]);
    let fetched = h.calls("getFile").pop().unwrap();
    assert_eq!(fetched.get("file_id"), Some("grand"));
}

// Listeners.

#[test]
fn one_listener_reads_for_all_and_returns_its_own() {
    let mut h = harness();
    h.live("nova-studio");
    h.bot
        .update_session("acme-backend", obj(json!({"cwd": "/projets/acme-backend", "last_listen": now()})))
        .unwrap();
    h.tg.borrow_mut().updates = vec![
        message(7, 60, "nova: pour toi", json!({})),
        message(8, 61, "backend: pour moi", json!({})),
    ];
    let (received, ignored) = h.bot.listen("acme-backend", 30).unwrap();
    let texts: Vec<&str> = received.iter().map(|e| e.text("text").unwrap()).collect();
    assert_eq!((texts, ignored), (vec!["pour moi"], 0));
    assert_eq!(h.inbox("nova-studio"), ["pour toi"]);
    assert_eq!(h.offset(), "9");
    let reactions = h.calls("setMessageReaction");
    assert_eq!(reactions.iter().map(|p| p.get("message_id").unwrap()).collect::<Vec<_>>(), ["61"]);
    assert!(h.bot.store.released("session.acme-backend"));
    assert!(h.bot.store.released("poll"));
}

#[test]
fn without_the_reading_a_listener_waits_for_its_inbox() {
    let mut h = harness();
    h.locks.push(h.bot.store.lock("poll", false).unwrap());
    h.bot
        .update_session("acme-backend", obj(json!({"cwd": "/projets/acme-backend", "last_listen": now()})))
        .unwrap();
    let root = h.dir.path().to_path_buf();
    // Another process (here a thread with its own store) files the message.
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let store = Store::new(root);
        let entry = json!({"at": now(), "from": "owner", "message_id": 70, "update_id": 9, "text": "coucou",
                           "session": "acme-backend", "how": "nom", "routed_at": now()});
        store.write(&store.path(&["inbox", "acme-backend", "9.json"]), &entry).unwrap();
    });
    let (received, _) = h.bot.listen("acme-backend", 10).unwrap();
    writer.join().unwrap();
    assert_eq!(received.iter().map(|e| e.text("text").unwrap()).collect::<Vec<_>>(), ["coucou"]);
    assert!(h.calls("getUpdates").is_empty());
}

#[test]
fn a_409_conflict_gives_up_the_reading_then_resumes() {
    let h = harness();
    h.bot
        .update_session("acme-backend", obj(json!({"cwd": "/projets/acme-backend", "last_listen": now()})))
        .unwrap();
    h.tg.borrow_mut().updates = vec![message(1, 160, "backend: après le conflit", json!({}))];
    h.tg.borrow_mut().conflicts = 1;
    let (received, _) = h.bot.listen("acme-backend", 30).unwrap();
    assert_eq!(received.iter().map(|e| e.text("text").unwrap()).collect::<Vec<_>>(), ["après le conflit"]);
}

#[test]
fn two_listeners_for_one_session() {
    let mut h = harness();
    h.live("nova-studio");
    let error = h.bot.listen("nova-studio", 1).unwrap_err();
    assert!(error.downcast_ref::<super::bot::Busy>().is_some());
    assert!(h.bot.start("nova-studio", &[], false).is_err());
}

#[test]
fn start_announces_the_session_and_its_aliases() {
    let h = harness();
    h.bot.start("acme-backend", &["Acme".to_string()], false).unwrap();
    let data = &h.bot.sessions()["acme-backend"];
    assert_eq!(data.list("aliases"), [json!("acme")]);
    assert!(Harness::text_of(&h.last_sent()).contains("commence ton message par « acme: »"));
    assert!(!h.calls("setMyCommands").is_empty());
}

#[test]
fn an_album_comes_back_in_one_wake_up() {
    let h = harness();
    h.group();
    h.bot
        .update_session("alice-site", obj(json!({"cwd": "/projets/alice-site", "last_listen": now()})))
        .unwrap();
    h.tg.borrow_mut().updates = vec![
        group_message(1, 1, "", ALICE, "Alice", GROUP, json!({"photo": [{"file_id": "a"}], "media_group_id": "album"})),
        group_message(2, 2, "", ALICE, "Alice", GROUP, json!({"photo": [{"file_id": "b"}], "media_group_id": "album"})),
    ];
    let (received, _) = h.bot.listen("alice-site", 30).unwrap();
    assert_eq!(received.len(), 2);
}

// Project groups.

#[test]
fn the_registry_binds_adds_members_and_removes() {
    let h = harness();
    h.group();
    let paul = BTreeMap::from([(7, "Paul".to_string())]);
    h.bot.group_add(-1009, "Mon Projet", &paul, Some("Projet de Paul"), false).unwrap();
    let data = &h.bot.groups()[&-1009];
    assert_eq!((data.text("session"), data.text("title")), (Some("mon-projet"), Some("Projet de Paul")));
    let lea = BTreeMap::from([(8, "Léa".to_string())]);
    h.bot.group_add(-1009, "mon-projet", &lea, None, false).unwrap();
    let data = &h.bot.groups()[&-1009];
    assert_eq!(data.object("members").unwrap().len(), 2);
    assert_eq!(data.text("title"), Some("Projet de Paul"));
    h.bot.group_remove(-1009, Some(7)).unwrap();
    assert_eq!(h.bot.groups()[&-1009].object("members").unwrap().keys().collect::<Vec<_>>(), ["8"]);
    h.bot.group_remove(-1009, None).unwrap();
    assert!(!h.bot.groups().contains_key(&-1009));
}

#[test]
fn the_registry_refuses_mistakes() {
    let h = harness();
    h.group();
    let paul = BTreeMap::from([(7, "P".to_string())]);
    assert!(h.bot.group_add(42, "x", &paul, None, false).is_err());
    let error = h.bot.group_add(-1009, "alice-site", &paul, None, false).unwrap_err();
    assert!(error.to_string().contains("déjà liée"));
    assert!(h.bot.group_remove(-555, None).is_err());
    let negative = BTreeMap::from([(-7, "P".to_string())]);
    assert!(h.bot.group_add(-1010, "y", &negative, None, false).is_err());
    let other = h.bot.group_add(GROUP, "autre", &BTreeMap::new(), None, false).unwrap_err();
    assert!(other.to_string().contains("--force"));
    h.bot.group_add(GROUP, "autre", &BTreeMap::new(), None, true).unwrap();
    assert_eq!(h.bot.groups()[&GROUP].object("members").unwrap().len(), 0);
}

#[test]
fn a_group_turned_supergroup_keeps_its_session() {
    let mut h = harness();
    h.group();
    h.bot.group_add(-555, "autre", &BTreeMap::from([(7, "Paul".to_string())]), None, false).unwrap();
    let mut service = group_message(1, 1, "", 7, "Paul", -555, json!({"migrate_to_chat_id": -100555}));
    service["message"].as_object_mut().unwrap().remove("text");
    assert_eq!(h.dispatch(service), 0);
    assert!(!h.bot.groups().contains_key(&-555));
    assert_eq!(h.bot.groups()[&-100555].text("session"), Some("autre"));
    h.live("autre");
    h.dispatch(group_message(2, 2, "bonjour", 7, "Paul", -100555, json!({})));
    assert_eq!(h.inbox("autre"), ["bonjour"]);
}

#[test]
fn sending_to_a_migrated_group_follows_its_new_id() {
    let h = harness();
    h.group();
    h.tg.borrow_mut().migrated.insert(GROUP, -100999);
    h.bot.send("C’est en ligne", "alice-site", false).unwrap();
    assert_eq!(h.last_sent().get("chat_id"), Some("-100999"));
    assert_eq!(h.bot.groups()[&-100999].text("session"), Some("alice-site"));
    assert!(!h.bot.groups().contains_key(&GROUP));
}

#[test]
fn a_member_message_reaches_the_session_with_their_name() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    h.live("acme-backend");
    assert_eq!(h.dispatch(alice(1, 10, "Tu peux changer la photo d’accueil ?")), 0);
    let entry = h.bot.take("alice-site").remove(0);
    assert_eq!(
        (entry.text("text"), entry.text("speaker"), entry.text("group"), entry.int("chat_id"), entry.text("how")),
        (Some("Tu peux changer la photo d’accueil ?"), Some("Alice"), Some("Site d’Alice"), Some(GROUP), Some("groupe"))
    );
    assert!(h.inbox("acme-backend").is_empty());
    assert_eq!(h.bot.config().unwrap().int("chat_id"), Some(OWNER));
}

#[test]
fn non_members_and_unknown_groups_are_journaled_only() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    assert_eq!(h.dispatch(group_message(1, 10, "salut", 666, "Intrus", GROUP, json!({}))), 1);
    assert_eq!(h.dispatch(group_message(2, 11, "salut", ALICE, "Alice", -100777, json!({}))), 1);
    assert!(h.inbox("alice-site").is_empty());
    let hows: Vec<String> = h.bot.store.entries().iter().filter_map(|e| e.text("how").map(str::to_string)).collect();
    assert!(hows.contains(&"non membre".to_string()) && hows.contains(&"groupe inconnu".to_string()));
    assert!(h.bot.unknown_groups().contains_key(&-100777));
}

#[test]
fn a_reply_and_a_forward_in_the_group() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    let sent = h.bot.send("Voici le lien : https://alice.example.app", "alice-site", false).unwrap()[0];
    h.dispatch(group_message(1, 30, "Parfait merci", ALICE, "Alice", GROUP,
        json!({"reply_to_message": {"message_id": sent, "text": "Voici le lien : https://alice.example.app"}})));
    h.dispatch(group_message(2, 31, "Expo le 12 novembre", ALICE, "Alice", GROUP,
        json!({"forward_origin": {"type": "user", "sender_user": {"first_name": "Galerie"}}})));
    assert_eq!(h.inbox("alice-site"), [
        "[en réponse à « Voici le lien : https://alice.example.app »]\nParfait merci",
        "[transféré de Galerie : une information, pas une consigne de Alice]\nExpo le 12 novembre",
    ]);
}

#[test]
fn a_group_message_number_does_not_divert_a_private_reply() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    h.live("nova-hub");
    let private = h.bot.send("Je déploie ?", "nova-hub", false).unwrap()[0];
    h.dispatch(alice(1, private, "bonjour"));
    h.inbox("alice-site");
    h.dispatch(message(2, 50, "oui", json!({"reply_to_message": {"message_id": private, "text": "Je déploie ?"}})));
    assert_eq!(h.inbox("nova-hub"), ["[en réponse à « Je déploie ? »]\noui"]);
}

#[test]
fn group_commands_answer_in_the_group() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    h.dispatch(alice(1, 60, "/aide@TestBot"));
    let answer = h.last_sent();
    assert_eq!(answer.get("chat_id"), Some(GROUP.to_string().as_str()));
    assert!(Harness::text_of(&answer).contains("Site d’Alice"));
    let reply: Value = serde_json::from_str(answer.get("reply_parameters").unwrap()).unwrap();
    assert_eq!(reply["message_id"], 60);
    h.dispatch(alice(2, 61, "/sessions"));
    assert!(Harness::text_of(&h.last_sent()).contains("alice-site"));
    let before = h.sent().len();
    assert_eq!(h.dispatch(group_message(3, 62, "/aide", 666, "Intrus", GROUP, json!({}))), 1);
    assert_eq!(h.sent().len(), before);
    assert!(h.inbox("alice-site").is_empty());
}

#[test]
fn a_command_for_another_bot_is_ignored() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    let before = h.sent().len();
    assert_eq!(h.dispatch(alice(1, 63, "/start@AutreBot")), 0);
    assert_eq!(h.sent().len(), before);
    assert!(h.inbox("alice-site").is_empty());
}

#[test]
fn an_absent_group_session_is_reported_privately_without_quoting() {
    let mut h = harness();
    h.group();
    h.session("alice-site", false, json!({}));
    h.dispatch(alice(1, 70, "tu es là ?"));
    h.bot.warn_stale(now() + 200.0).unwrap();
    let warnings: Vec<Params> = h.sent().into_iter().filter(|p| Harness::text_of(p).starts_with('⏳')).collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].get("chat_id"), Some(OWNER.to_string().as_str()));
    assert!(warnings[0].get("reply_parameters").is_none());
    assert!(Harness::text_of(&warnings[0]).contains("Alice") && Harness::text_of(&warnings[0]).contains("Site d’Alice"));
    // The group itself is told once that Claude isn't available.
    let told: Vec<Params> = h.sent().into_iter().filter(|p| p.get("chat_id") == Some(GROUP.to_string().as_str())).collect();
    assert_eq!(told.len(), 1);
    assert!(Harness::text_of(&told[0]).contains("Sam est prévenu"));
}

#[test]
fn sending_from_a_bound_session_goes_to_the_group_without_header() {
    let h = harness();
    h.group();
    h.bot.send("C’est en ligne !", "alice-site", false).unwrap();
    let last = h.last_sent();
    assert_eq!((last.get("chat_id"), last.get("text")), (Some(GROUP.to_string().as_str()), Some("C’est en ligne !")));
    assert!(last.get("parse_mode").is_none());
    assert_eq!(h.bot.store.entries().last().unwrap().int("chat_id"), Some(GROUP));
    h.bot.send("Alice demande un achat", "alice-site", true).unwrap();
    let private = h.last_sent();
    assert_eq!(private.get("chat_id"), Some(OWNER.to_string().as_str()));
    assert_eq!(Harness::text_of(&private), format!("<b>{}</b>\nAlice demande un achat", tag("alice-site", "")));
}

#[test]
fn a_file_goes_to_the_group_or_privately() {
    let h = harness();
    h.group();
    let image = h.dir.path().join("apercu.png");
    std::fs::write(&image, b"png").unwrap();
    h.bot.send_document(&image, "alice-site", "La nouvelle page", false, Extras::default()).unwrap();
    h.bot.send_document(&image, "alice-site", "pour toi", true, Extras::default()).unwrap();
    let sent = h.calls("sendDocument");
    assert_eq!(
        (sent[0].get("chat_id"), sent[0].get("caption"), sent[0].get("document")),
        (Some(GROUP.to_string().as_str()), Some("La nouvelle page"), Some("apercu.png"))
    );
    assert_eq!(
        (sent[1].get("chat_id"), sent[1].get("caption").map(str::to_string)),
        (Some(OWNER.to_string().as_str()), Some(format!("{}\npour toi", tag("alice-site", ""))))
    );
}

#[test]
fn a_bound_session_alone_does_not_get_unaddressed_private_messages() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    h.dispatch(message(1, 80, "tu en es où ?", json!({})));
    assert!(h.inbox("alice-site").is_empty());
    assert_eq!(Harness::text_of(&h.last_sent()), QUESTION);
}

#[test]
fn group_send_does_not_capture_private_messages() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    h.live("nova-hub");
    h.bot.send("Voilà Alice", "alice-site", false).unwrap();
    h.dispatch(message(1, 80, "tu en es où ?", json!({})));
    assert_eq!(h.inbox("nova-hub"), ["tu en es où ?"]);
    assert!(h.inbox("alice-site").is_empty());
}

#[test]
fn start_and_stop_are_private_unless_announced() {
    let h = harness();
    h.group();
    h.bot.start("alice-site", &[], false).unwrap();
    let hello = h.last_sent();
    assert_eq!(hello.get("chat_id"), Some(OWNER.to_string().as_str()));
    assert!(Harness::text_of(&hello).contains("groupe « Site d’Alice »"));
    h.bot.start("alice-site", &[], true).unwrap();
    let hello = h.last_sent();
    assert_eq!(hello.get("chat_id"), Some(GROUP.to_string().as_str()));
    assert!(Harness::text_of(&hello).starts_with("Bonjour"));
}

#[test]
fn an_anonymous_admin_is_journaled_and_reported_once() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    let anonymous = |update_id: i64| {
        group_message(update_id, update_id, "coucou", 1087968824, "Group", GROUP,
            json!({"from": {"id": 1087968824, "is_bot": true, "first_name": "Group"}, "sender_chat": {"id": GROUP, "title": "Site d’Alice"}}))
    };
    assert_eq!(h.dispatch(anonymous(1)), 1);
    assert_eq!(h.dispatch(anonymous(2)), 1);
    assert!(h.inbox("alice-site").is_empty());
    let notices: Vec<Params> = h.sent().into_iter().filter(|p| Harness::text_of(p).contains("anonyme")).collect();
    assert_eq!(notices.len(), 1);
}

#[test]
fn a_member_writing_privately_is_told_once_to_use_the_group() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    let private = |update_id: i64| {
        json!({"update_id": update_id, "message": {"message_id": update_id, "date": now() as i64,
               "from": {"id": ALICE, "first_name": "Alice"}, "chat": {"id": ALICE, "type": "private"}, "text": "bonjour"}})
    };
    assert_eq!(h.dispatch(private(1)), 1);
    assert_eq!(h.dispatch(private(2)), 1);
    let told: Vec<Params> = h.sent().into_iter().filter(|p| p.get("chat_id") == Some(ALICE.to_string().as_str())).collect();
    assert_eq!(told.len(), 1);
    assert!(Harness::text_of(&told[0]).contains("Site d’Alice"));
    assert!(h.inbox("alice-site").is_empty());
}

#[test]
fn group_topics_the_answer_goes_in_the_topic() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    h.dispatch(group_message(1, 90, "dans le sujet", ALICE, "Alice", GROUP, json!({"message_thread_id": 12, "is_topic_message": true})));
    assert_eq!(h.inbox("alice-site"), ["dans le sujet"]);
    h.bot.send("Réponse", "alice-site", false).unwrap();
    assert_eq!(h.last_sent().get("message_thread_id"), Some("12"));
}

#[test]
fn the_bot_added_to_a_group_is_journaled_and_the_owner_told() {
    let h = harness();
    let added = json!({"update_id": 1, "my_chat_member": {"chat": {"id": -100321, "type": "supergroup", "title": "Nouveau"},
        "from": {"id": OWNER, "first_name": "Sam"}, "date": now() as i64,
        "old_chat_member": {"status": "left"}, "new_chat_member": {"status": "member"}}});
    assert_eq!(h.dispatch(added), 0);
    let notice = Harness::text_of(&h.last_sent());
    assert!(notice.contains("asp telegram group add --chat -100321"));
    assert!(notice.contains("administrateur"));
    assert!(h.bot.unknown_groups().contains_key(&-100321));
}

#[test]
fn routing_is_unchanged_by_a_busy_group() {
    let mut h = harness();
    h.group();
    let at = now();
    for id in 1..1200 {
        h.bot.store.journal(&json!({"at": at, "from": "groupe", "chat_id": GROUP, "group": "Site d’Alice", "user_id": ALICE,
            "speaker": "Alice", "message_id": id, "session": "alice-site", "how": "groupe", "routed_at": at, "thread_id": null, "text": "x"}));
        h.bot.store.journal(&json!({"at": at, "message_id": id, "from": "claude", "session": "alice-site", "chat_id": GROUP,
            "group": "Site d’Alice", "thread_id": null, "text": "y"}));
    }
    h.live("acme-backend");
    h.live("nova-studio");
    let sent = h.bot.send("Fiche prête ?", "nova-studio", false).unwrap()[0];
    h.dispatch(message(1, 20, "oui", json!({"reply_to_message": {"message_id": sent, "text": "Fiche prête ?"}})));
    assert_eq!(h.inbox("nova-studio"), ["[en réponse à « Fiche prête ? »]\noui"]);
}

// Compatibility with a folder written by the Python sync-telegram.

#[test]
fn reads_a_folder_written_by_sync_telegram() {
    let h = harness();
    let root = h.dir.path();
    std::fs::create_dir_all(root.join("sessions")).unwrap();
    std::fs::write(
        root.join("sessions/nova-hub.json"),
        "{\n  \"name\": \"nova-hub\",\n  \"created\": 1761990000.123456,\n  \"aliases\": [\n    \"evo\"\n  ],\n  \"thread_id\": null,\n  \"cwd\": \"/projets/nova-hub\",\n  \"last_listen\": 1761990100.5,\n  \"stopped\": false\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("groups.json"),
        format!("{{\n  \"chats\": {{\n    \"{GROUP}\": {{\n      \"added\": 1761990000.0,\n      \"members\": {{\n        \"{ALICE}\": \"Alice\"\n      }},\n      \"session\": \"alice-site\",\n      \"title\": \"Site d’Alice\"\n    }}\n  }},\n  \"migrations\": {{\n    \"-555\": {GROUP}\n  }}\n}}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("journal.jsonl"),
        "{\"at\": 1761990200.0, \"message_id\": 900, \"from\": \"claude\", \"session\": \"nova-hub\", \"thread_id\": null, \"text\": \"Bonjour\"}\n",
    )
    .unwrap();
    assert_eq!(h.bot.sessions()["nova-hub"].list("aliases"), [json!("evo")]);
    assert_eq!(h.bot.group_of("alice-site").map(|(chat, _)| chat), Some(GROUP));
    assert_eq!(h.bot.latest(-555, None), GROUP);
    let mut h = h;
    h.locks.push(h.bot.store.lock("session.nova-hub", false).unwrap());
    h.dispatch(message(1, 20, "oui", json!({"reply_to_message": {"message_id": 900, "text": "Bonjour"}})));
    assert_eq!(h.inbox("nova-hub"), ["[en réponse à « Bonjour »]\noui"]);
}

#[test]
fn a_follow_up_counts_a_message_routed_by_sync_telegram() {
    let mut h = harness();
    h.live("acme-backend");
    h.live("nova-studio");
    h.bot.store.journal(&json!({"at": now(), "from": "charles", "session": "nova-studio", "how": "nom",
                                "routed_at": now(), "message_id": 5, "update_id": 5, "text": "première consigne"}));
    h.dispatch(message(6, 6, "et la deuxième", json!({})));
    assert_eq!(h.inbox("nova-studio"), ["et la deuxième"]);
}

// The relay: one bot for several machines, Telegram's updates by webhook.

#[test]
fn with_a_relay_the_reading_goes_through_it_and_acknowledges() {
    let mut h = harness();
    h.relayed("");
    h.live("nova-studio");
    std::fs::write(h.dir.path().join("offset"), "41").unwrap();
    h.relay.borrow_mut().updates = vec![message(41, 170, "nova: par le relais", json!({}))];
    h.bot.poll_once(1).unwrap();
    assert_eq!(h.inbox("nova-studio"), ["par le relais"]);
    assert_eq!(h.offset(), "42");
    h.bot.poll_once(1).unwrap();
    let acks: Vec<i64> = h.relay_calls("/releve").iter().map(|b| b["ack"].as_i64().unwrap()).collect();
    assert_eq!(acks, [40, 41]);
    assert!(h.relay.borrow().updates.is_empty());
    assert!(h.calls("getUpdates").is_empty());
}

#[test]
fn with_a_relay_a_listener_takes_the_reading_without_get_updates() {
    let h = harness();
    h.relayed("");
    h.bot
        .update_session("acme-backend", obj(json!({"cwd": "/projets/acme-backend", "last_listen": now()})))
        .unwrap();
    h.relay.borrow_mut().updates = vec![message(1, 171, "backend: relayé", json!({}))];
    let (received, _) = h.bot.listen("acme-backend", 10).unwrap();
    assert_eq!(received.iter().map(|e| e.text("text").unwrap()).collect::<Vec<_>>(), ["relayé"]);
    assert!(h.calls("getUpdates").is_empty());
}

#[test]
fn the_relay_learns_each_new_topic_and_group() {
    let mut h = harness();
    h.relayed("");
    h.topics();
    h.live("nova-studio");
    h.bot.send("Bonjour", "nova-studio", false).unwrap();
    let told = h.relay_calls("/fils").pop().unwrap();
    assert_eq!(told, json!({"fils": [77], "groupes": [], "prefixe": "", "principale": true}));
    h.group();
    let told = h.relay_calls("/fils").pop().unwrap();
    assert_eq!((&told["fils"], &told["groupes"]), (&json!([77]), &json!([GROUP])));
}

#[test]
fn without_a_relay_nothing_is_told_to_one() {
    let mut h = harness();
    h.topics();
    h.live("nova-studio");
    h.bot.send("Bonjour", "nova-studio", false).unwrap();
    h.group();
    assert!(!h.bot.relay_sync());
    assert!(h.relay.borrow().calls.is_empty());
}

#[test]
fn a_machine_prefix_names_its_sessions_and_topics() {
    let mut h = harness();
    h.relayed("pc");
    h.live("nova-studio");
    h.bot.send("Bonjour", "nova-studio", false).unwrap();
    assert_eq!(Harness::text_of(&h.last_sent()), format!("<b>{}</b>\nBonjour", tag("nova-studio", "pc")));
    assert!(tag("nova-studio", "pc").ends_with(" pc-nova-studio"));
    h.dispatch(message(1, 180, "pc-nova: le build", json!({})));
    assert_eq!(h.inbox("nova-studio"), ["le build"]);
    // "pc: …" names the machine only: its only active session takes it.
    h.dispatch(message(2, 181, "pc: tu es là ?", json!({})));
    assert_eq!(h.inbox("nova-studio"), ["tu es là ?"]);
    h.topics();
    h.bot.send("Fil", "acme-backend", false).unwrap();
    let created = h.calls("createForumTopic");
    assert_eq!(created.iter().map(|p| p.get("name").unwrap()).collect::<Vec<_>>(), ["pc-acme-backend"]);
}

#[test]
fn doctor_tells_the_relay_machines() {
    let h = harness();
    h.relayed("");
    let lines = h.bot.doctor().unwrap();
    assert!(lines.iter().any(|line| line
        == "Relais https://relay.example : machine principale mac ; mac relevé il y a 10 s, pc relevé il y a 600 s (2 en attente)"));
}

#[test]
fn the_machine_reader_needs_a_relay() {
    let h = harness();
    assert!(h.bot.relay_for_machine().is_err());
    assert!(h.calls("getUpdates").is_empty());
}

#[test]
fn a_listener_is_the_asp_process_not_the_shell_around_it() {
    assert_eq!(super::program_name("/Applications/AgentsSleepPreventer.app/Contents/MacOS/asp telegram listen --session x"), "asp");
    assert_eq!(super::program_name("asp telegram listen --session x --max 0"), "asp");
    assert_eq!(super::program_name("/bin/zsh -c source s.sh && eval 'asp telegram listen --session x --max 0'"), "zsh");
}

// Named topics of a project group and buttons, as in sync-telegram
// (`send --sujet`, `--boutons`): same fields in groups.json and the inboxes.

impl Harness {
    fn topics_created(&self) -> Vec<(String, String)> {
        self.calls("createForumTopic")
            .iter()
            .map(|p| (p.get("chat_id").unwrap().to_string(), p.get("name").unwrap().to_string()))
            .collect()
    }

    fn group_topics(&self) -> Value {
        self.bot.groups()[&GROUP].get("topics").cloned().unwrap_or(Value::Null)
    }

    fn press(&self, update_id: i64, data: &str, card: &Value, sender: i64, name: &str) -> i32 {
        self.dispatch(json!({"update_id": update_id, "callback_query": {
            "id": format!("q{update_id}"), "from": {"id": sender, "first_name": name}, "data": data, "message": card}}))
    }

    fn answers(&self) -> Vec<String> {
        self.calls("answerCallbackQuery").iter().map(Harness::text_of).collect()
    }
}

fn in_topic(update_id: i64, text: &str, topic: i64, extra: Value) -> Value {
    let mut extra = obj(extra);
    extra.extend(obj(json!({"message_thread_id": topic, "is_topic_message": true})));
    group_message(update_id, update_id + 100, text, ALICE, "Alice", GROUP, Value::Object(extra))
}

fn topic(name: &str) -> Extras<'_> {
    Extras { topic: Some(name), ..Extras::default() }
}

#[test]
fn a_named_topic_is_created_once() {
    let h = harness();
    h.group();
    h.bot.send_with("Carte 1", "alice-site", false, topic("Luka Beauté")).unwrap();
    h.bot.send_with("Carte 2", "alice-site", false, topic("  luka   beauté ")).unwrap();
    h.bot.send_with(&"x".repeat(5000), "alice-site", false, topic("Évolutions")).unwrap(); // two messages, one topic
    let group = GROUP.to_string();
    assert_eq!(
        h.topics_created(),
        [(group.clone(), "Luka Beauté".to_string()), (group, "Évolutions".to_string())]
    );
    let threads: Vec<String> = h.sent().iter().map(|p| p.get("message_thread_id").unwrap().to_string()).collect();
    assert_eq!(threads, ["77", "77", "78", "78"]);
    assert_eq!(h.group_topics(), json!({"77": "Luka Beauté", "78": "Évolutions"}));
    assert_eq!(h.bot.groups()[&GROUP].int("topic"), None); // where answers go doesn't move
    let last = h.bot.store.entries().pop().unwrap();
    assert_eq!((last.int("topic_id"), last.text("topic")), (Some(78), Some("Évolutions")));
    h.bot.send("Sans sujet", "alice-site", false).unwrap();
    assert!(h.last_sent().get("message_thread_id").is_none());
}

#[test]
fn a_deleted_named_topic_is_created_again() {
    let h = harness();
    h.group();
    h.bot.send_with("Un", "alice-site", false, topic("Évolutions")).unwrap();
    h.tg.borrow_mut().lost_thread = true;
    h.bot.send_with("Deux", "alice-site", false, topic("Évolutions")).unwrap();
    assert_eq!(h.topics_created().len(), 2);
    assert_eq!(h.last_sent().get("message_thread_id"), Some("78"));
    assert_eq!(h.group_topics(), json!({"78": "Évolutions"}));
}

#[test]
fn a_named_topic_needs_a_group_and_the_right_to_create_it() {
    let h = harness();
    h.group();
    let refused = |name: &str, private: bool| {
        h.bot.send_with("x", name, private, topic("Évolutions")).unwrap_err().to_string()
    };
    assert!(refused("alice-site", true).contains("groupe de projet"));
    assert!(refused("no-group", false).contains("groupe de projet"));
    h.tg.borrow_mut().wrong_home = true; // no topics in the group, or no right to manage them
    assert!(refused("alice-site", false).contains("sujets activés"));
    assert!(h.sent().is_empty());
    assert_eq!(h.group_topics(), Value::Null);
}

#[test]
fn a_file_in_a_named_topic() {
    let h = harness();
    h.group();
    let image = h.dir.path().join("carte.png");
    std::fs::write(&image, b"png").unwrap();
    h.bot.send_document(&image, "alice-site", "La carte", false, topic("Luka Beauté")).unwrap();
    let sent = h.calls("sendDocument").pop().unwrap();
    let group = GROUP.to_string();
    assert_eq!(
        (sent.get("chat_id"), sent.get("message_thread_id"), sent.get("caption")),
        (Some(group.as_str()), Some("77"), Some("La carte"))
    );
    assert_eq!(h.bot.store.entries().pop().unwrap().text("topic"), Some("Luka Beauté"));
}

#[test]
fn received_messages_tell_their_topic() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    // A member creates the topic: a service message, nothing delivered, the name kept.
    h.dispatch(in_topic(1, "", 55, json!({"forum_topic_created": {"name": "Luka Beauté", "icon_color": 1}})));
    assert!(h.bot.take("alice-site").is_empty());
    let opened = json!({"message_id": 101, "forum_topic_created": {"name": "Luka Beauté"}});
    h.dispatch(in_topic(2, "oui", 55, json!({"reply_to_message": opened})));
    let entry = h.bot.take("alice-site").remove(0);
    assert_eq!(
        (entry.int("topic_id"), entry.text("topic"), entry.text("text")),
        (Some(55), Some("Luka Beauté"), Some("oui"))
    );
    // Renamed: the new name holds, though messages still carry the original one.
    h.dispatch(in_topic(3, "", 55, json!({"forum_topic_edited": {"name": "Luka"}})));
    h.dispatch(in_topic(4, "non", 55, json!({"reply_to_message": opened})));
    assert_eq!(h.bot.take("alice-site").remove(0).text("topic"), Some("Luka"));
    // A topic never seen: its name comes from its first message that isn't a reply.
    let first = json!({"message_id": 9, "forum_topic_created": {"name": "Évolutions"}});
    h.dispatch(in_topic(5, "ici", 61, json!({"reply_to_message": first})));
    assert_eq!(h.bot.take("alice-site").remove(0).text("topic"), Some("Évolutions"));
    h.bot.send_with("Récap", "alice-site", false, topic("évolutions")).unwrap();
    assert_eq!(h.last_sent().get("message_thread_id"), Some("61"));
    assert!(h.topics_created().is_empty());
    // Unknown name (a reply, in a topic older than the bot): its number.
    h.dispatch(in_topic(6, "et là", 70, json!({"reply_to_message": {"message_id": 8, "text": "Carte Nova"}})));
    let entry = h.bot.take("alice-site").remove(0);
    assert_eq!((entry.int("topic_id"), entry.text("topic")), (Some(70), None));
    assert_eq!(h.bot.place(&entry), "groupe Site d’Alice, sujet n° 70");
    assert_eq!(h.group_topics(), json!({"55": "Luka", "61": "Évolutions"}));
}

#[test]
fn the_topic_shows_even_when_an_older_listener_filed_the_message() {
    let h = harness();
    h.group();
    h.bot.send_with("Carte", "alice-site", false, topic("Luka Beauté")).unwrap(); // topic 77
    // An entry as older code files it: the topic's number, not its name.
    let entry = obj(json!({"from": "groupe", "chat_id": GROUP, "group": "Site d’Alice", "speaker": "Alice",
                           "topic_id": 77, "text": "oui"}));
    assert_eq!(h.bot.place(&entry), "groupe Site d’Alice, sujet Luka Beauté");
}

#[test]
fn buttons_under_a_card_the_choice_goes_back_to_the_session() {
    let mut h = harness();
    h.group();
    h.live("alice-site");
    let text = "LUKA-3 · Couper la pub « Peau nette » ?";
    let labels = ["Oui".to_string(), "Non".to_string()];
    let extras = Extras { topic: Some("Luka Beauté"), buttons: &labels, ..Extras::default() };
    let id = h.bot.send_with(text, "alice-site", false, extras).unwrap()[0];
    let sent = h.last_sent();
    assert_eq!(buttons(&sent), ["b:0", "b:1"]);
    let markup: Value = serde_json::from_str(sent.get("reply_markup").unwrap()).unwrap();
    assert_eq!(markup["inline_keyboard"].as_array().unwrap().len(), 1); // short: one line
    assert_eq!(h.bot.store.entries().pop().unwrap().get("buttons"), Some(&json!(["Oui", "Non"])));
    let card = json!({"message_id": id, "chat": {"id": GROUP, "type": "supergroup"}, "text": text,
                      "message_thread_id": 77, "is_topic_message": true, "reply_markup": markup});
    assert_eq!(h.press(1, "b:0", &card, 666, "Intrus"), 1); // not a member: nothing
    assert!(h.bot.take("alice-site").is_empty());
    assert_eq!(h.press(2, "b:0", &card, ALICE, "Alice"), 0);
    let entry = h.bot.take("alice-site").remove(0);
    assert_eq!(
        (entry.text("speaker"), entry.text("topic"), entry.text("button"), entry.text("how")),
        (Some("Alice"), Some("Luka Beauté"), Some("Oui"), Some("choix"))
    );
    assert_eq!(entry.text("text"), Some(format!("[bouton « Oui » sous « {text} »]").as_str()));
    let frozen = h.calls("editMessageReplyMarkup").pop().unwrap();
    let group = GROUP.to_string();
    let id_text = id.to_string();
    assert_eq!((frozen.get("chat_id"), frozen.get("message_id")), (Some(group.as_str()), Some(id_text.as_str())));
    let chosen: Value = serde_json::from_str(frozen.get("reply_markup").unwrap()).unwrap();
    assert_eq!(chosen["inline_keyboard"][0][0]["text"], "✅ Oui · Alice");
    assert_eq!(h.bot.groups()[&GROUP].int("topic"), Some(77)); // the session's next messages go there
    // One choice per card: another member, or the frozen button, add nothing.
    h.press(3, "b:1", &card, OWNER, "Sam");
    let mut pressed_again = card.clone();
    pressed_again["reply_markup"] = chosen;
    h.press(4, "b:x", &pressed_again, ALICE, "Alice");
    assert!(h.bot.take("alice-site").is_empty());
    assert_eq!(
        h.answers(),
        [
            "Ce bouton est réservé aux membres du projet.",
            "Noté : Oui",
            "Déjà répondu : Oui (Alice)",
            "Déjà répondu : Oui (Alice)"
        ]
    );
}

#[test]
fn buttons_in_private_go_back_to_the_session_that_wrote() {
    let mut h = harness();
    h.live("fable");
    let labels = ["Oui".to_string(), "Plus tard, je regarde demain matin".to_string()];
    let extras = Extras { buttons: &labels, ..Extras::default() };
    let id = h.bot.send_with("On déploie ?", "fable", false, extras).unwrap()[0];
    let sent = h.last_sent();
    let owner = OWNER.to_string();
    assert_eq!(sent.get("chat_id"), Some(owner.as_str()));
    let markup: Value = serde_json::from_str(sent.get("reply_markup").unwrap()).unwrap();
    assert_eq!(markup["inline_keyboard"].as_array().unwrap().len(), 2); // long labels: one per line
    let card = json!({"message_id": id, "chat": {"id": OWNER, "type": "private"},
                      "text": format!("{}\nOn déploie ?", tag("fable", "")), "reply_markup": markup});
    assert_eq!(h.press(1, "b:1", &card, 666, "Intrus"), 1);
    assert_eq!(h.press(2, "b:1", &card, OWNER, "Sam"), 0);
    let entry = h.bot.take("fable").remove(0);
    assert_eq!((entry.text("from"), entry.text("how")), (Some("owner"), Some("choix")));
    assert_eq!(entry.text("text"), Some("[bouton « Plus tard, je regarde demain matin » sous « On déploie ? »]"));
    let frozen = h.calls("editMessageReplyMarkup").pop().unwrap();
    let id_text = id.to_string();
    assert_eq!((frozen.get("chat_id"), frozen.get("message_id")), (Some(owner.as_str()), Some(id_text.as_str())));
}

#[test]
fn refused_buttons_send_nothing() {
    let h = harness();
    h.group();
    let refused: [Vec<String>; 3] = [
        vec!["x".repeat(41)],
        vec!["Oui".to_string(), " ".to_string()],
        (0..9).map(|n| n.to_string()).collect(),
    ];
    for labels in &refused {
        let extras = Extras { buttons: labels, ..Extras::default() };
        let error = h.bot.send_with("Carte", "alice-site", false, extras).unwrap_err();
        assert!(error.to_string().contains("--buttons"), "{error}");
    }
    assert!(h.sent().is_empty());
    let labels = ["Oui".to_string(), "Non".to_string()];
    let extras = Extras { buttons: &labels, ..Extras::default() };
    let image = h.dir.path().join("carte.png");
    std::fs::write(&image, b"png").unwrap();
    h.bot.send_document(&image, "alice-site", "La carte", false, extras).unwrap();
    assert_eq!(buttons(&h.calls("sendDocument").pop().unwrap()), ["b:0", "b:1"]);
    h.bot.send_with(&"x".repeat(5000), "alice-site", false, extras).unwrap(); // two messages: buttons under the last
    let sent = h.sent();
    assert!(sent[sent.len() - 2].get("reply_markup").is_none());
    assert_eq!(buttons(&sent[sent.len() - 1]), ["b:0", "b:1"]);
}

// The owner's night: what is written to them privately doesn't ring, and
// send tells the session (ported from sync-telegram's NightTests).

/// Today at this time of the machine's clock.
fn today_at(hour: i32, minute: i32) -> f64 {
    let at = now() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&at, &mut tm) };
    (tm.tm_hour, tm.tm_min, tm.tm_sec, tm.tm_isdst) = (hour, minute, 0, -1);
    unsafe { libc::mktime(&mut tm) as f64 }
}

impl Harness {
    fn night(&self) {
        self.bot.update_config(obj(json!({"night": ["00:00", "24:00"]}))).unwrap();
    }

    fn silent(params: &Params) -> bool {
        params.get("disable_notification") == Some("true")
    }

    fn last_entry(&self) -> Object {
        self.bot.store.entries().pop().unwrap()
    }
}

#[test]
fn the_hours_of_the_night() {
    let minutes = |clock: &str| {
        let (hours, minutes) = clock.split_once(':').unwrap();
        hours.parse::<u32>().unwrap() * 60 + minutes.parse::<u32>().unwrap()
    };
    let inside = |start, end, clocks: &[&str]| clocks.iter().map(|c| within(start, end, minutes(c))).collect::<Vec<_>>();
    assert_eq!(
        inside("23:00", "08:00", &["22:59", "23:00", "03:26", "07:59", "08:00", "15:00"]),
        [false, true, true, true, false, false]
    );
    assert_eq!(inside("01:00", "06:30", &["00:59", "01:00", "06:29", "06:30"]), [false, true, true, false]);
    let h = harness();
    h.bot.update_config(obj(json!({"night": ["23:00", "08:00"]}))).unwrap();
    assert_eq!(h.bot.night(today_at(3, 26)), Some(("23:00".to_string(), "08:00".to_string())));
    assert_eq!(h.bot.night(today_at(15, 0)), None);
    h.bot.update_config(obj(json!({"night": []}))).unwrap();
    assert_eq!(h.bot.night(today_at(3, 0)), None);
}

#[test]
fn without_a_setting_the_night_goes_from_23_to_8() {
    let h = harness();
    let file = h.bot.store.path(&["bot.json"]);
    let mut settings = h.bot.config().unwrap();
    settings.remove("night");
    h.bot.store.write(&file, &Value::Object(settings)).unwrap();
    assert_eq!(h.bot.night(today_at(4, 4)), Some(("23:00".to_string(), "08:00".to_string())));
    assert_eq!(h.bot.night(today_at(12, 0)), None);
}

#[test]
fn at_night_a_private_message_goes_without_sound_and_send_says_so() {
    let mut h = harness();
    h.night();
    h.live("fable");
    assert!(h.bot.asleep(Some("fable"), now()));
    h.bot.send("Je fais la correction ?", "fable", false).unwrap();
    assert!(Harness::silent(&h.last_sent()));
    assert_eq!(h.last_entry().get("silent"), Some(&json!(true)));
    let note = h.bot.night_note(now());
    for said in ["Sam dort (nuit de 00:00 à 24:00)", "ne pose pas de question la nuit", "asp telegram matin"] {
        assert!(note.contains(said), "{note}");
    }
}

#[test]
fn by_day_nothing_changes() {
    let mut h = harness();
    h.live("fable");
    assert!(!h.bot.asleep(Some("fable"), now()));
    h.bot.send("Fait.", "fable", false).unwrap();
    assert!(h.last_sent().get("disable_notification").is_none());
    assert!(h.last_entry().get("silent").is_none());
}

#[test]
fn at_night_the_session_the_owner_talks_to_rings_the_others_dont() {
    let mut h = harness();
    h.night();
    h.live("fable");
    h.live("autre");
    h.bot.store.journal(&json!({"at": now() - 60.0, "from": "charles", "session": "fable", "message_id": 30, "text": "tu es là ?"}));
    h.bot.send("Oui.", "fable", false).unwrap();
    assert!(!Harness::silent(&h.last_sent()));
    assert!(h.bot.asleep(Some("autre"), now()));
    h.bot.send("Un souci cette nuit.", "autre", false).unwrap();
    assert!(Harness::silent(&h.last_sent()));
}

#[test]
fn at_night_an_old_message_from_the_owner_doesnt_wake_them() {
    let mut h = harness();
    h.night();
    h.live("fable");
    h.bot.store.journal(&json!({"at": now() - 3600.0, "from": "owner", "session": "fable", "message_id": 30, "text": "bonne nuit"}));
    h.bot.send("Fait.", "fable", false).unwrap();
    assert!(Harness::silent(&h.last_sent()));
}

#[test]
fn urgent_rings_at_night() {
    let mut h = harness();
    h.night();
    h.live("fable");
    let urgent = Extras { urgent: true, ..Extras::default() };
    h.bot.send_with("La prod est tombée.", "fable", false, urgent).unwrap();
    assert!(h.last_sent().get("disable_notification").is_none());
    assert!(h.last_entry().get("silent").is_none());
}

#[test]
fn at_night_a_file_goes_without_sound() {
    let mut h = harness();
    h.night();
    h.live("fable");
    let file = h.dir.path().join("rapport.pdf");
    std::fs::write(&file, b"%PDF").unwrap();
    h.bot.send_document(&file, "fable", "Le rapport", false, Extras::default()).unwrap();
    assert!(Harness::silent(&h.calls("sendDocument").pop().unwrap()));
    assert_eq!(h.last_entry().get("silent"), Some(&json!(true)));
}

#[test]
fn at_night_the_project_group_rings_as_by_day() {
    let mut h = harness();
    h.night();
    h.group();
    h.live("alice-site");
    h.bot.send("C’est en ligne.", "alice-site", false).unwrap();
    let sent = h.last_sent();
    assert_eq!(sent.get("chat_id"), Some(GROUP.to_string().as_str()));
    assert!(sent.get("disable_notification").is_none());
    h.bot.send("Alice demande un nom de domaine.", "alice-site", true).unwrap();
    assert!(Harness::silent(&h.last_sent()));
}

#[test]
fn at_night_the_bots_notices_and_the_ram_alerts_go_without_sound() {
    let h = harness();
    h.night();
    h.bot.notify("⏳ fable n’écoute pas.", Notice::default()).unwrap();
    assert!(Harness::silent(&h.last_sent()));
    h.bot.send("• RAM — fable : 12 Go", "moniteur", true).unwrap(); // how the monitor tells the owner
    assert!(Harness::silent(&h.last_sent()));
}

#[test]
fn by_day_morning_returns_at_once() {
    let h = harness();
    assert!(h.bot.morning(Duration::from_secs(3600)) < 1.0);
}
