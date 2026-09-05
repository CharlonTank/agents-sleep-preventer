use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// Hooks normally refresh during a turn. Bound orphaned markers even if an
// interrupted agent stays open, without mistaking a quiet LLM call for idle.
const MAX_SESSION_AGE_SECS: u64 = 6 * 60 * 60;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Auto,
    Awake,
    Sleep,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub pid: u32,
    pub process_started_at: u64,
    pub updated_at: u64,
    pub attention: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub mode: Mode,
    pub sessions: BTreeMap<u32, Session>,
    pub shutdown: bool,
}

impl State {
    pub fn working_count(&self) -> usize {
        self.sessions.values().filter(|s| !s.attention).count()
    }

    pub fn should_prevent_sleep(&self) -> bool {
        !self.shutdown
            && match self.mode {
                Mode::Auto => self.working_count() > 0,
                Mode::Awake => true,
                Mode::Sleep => false,
            }
    }

    pub fn start(&mut self, pid: u32, process_started_at: u64, now: u64) {
        self.shutdown = false;
        self.sessions.insert(
            pid,
            Session {
                pid,
                process_started_at,
                updated_at: now,
                attention: false,
            },
        );
    }

    pub fn prune(&mut self, now: u64, process_start: impl Fn(u32) -> Option<u64>) {
        self.sessions.retain(|pid, session| {
            process_start(*pid) == Some(session.process_started_at)
                && now.saturating_sub(session.updated_at) < MAX_SESSION_AGE_SECS
        });
    }
}

pub fn agent_kind(name: &str, args: &[String]) -> Option<&'static str> {
    fn basename(path: &str) -> String {
        path.rsplit(['/', '\\'])
            .next()
            .unwrap_or(path)
            .to_ascii_lowercase()
    }
    let executable = basename(name);
    match executable.trim_end_matches(".exe") {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "hermes" => Some("hermes"),
        "node" | "bun" => args.get(1).and_then(|script| {
            let path = script.replace('\\', "/").to_ascii_lowercase();
            if path.ends_with("/@anthropic-ai/claude-code/cli.js") {
                Some("claude")
            } else if path.ends_with("/@openai/codex/bin/codex.js") {
                Some("codex")
            } else {
                None
            }
        }),
        name if name.starts_with("python") => {
            let module = args.windows(2).any(|pair| {
                pair[0] == "-m" && matches!(pair[1].as_str(), "hermes" | "hermes_cli.main")
            });
            module.then_some("hermes")
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_waits_for_all_agents_and_respects_overrides() {
        let mut state = State::default();
        assert!(!state.should_prevent_sleep());
        state.start(10, 1, 100);
        state.start(20, 2, 100);
        state.sessions.remove(&10);
        assert!(state.should_prevent_sleep());
        state.sessions.get_mut(&20).unwrap().attention = true;
        assert!(!state.should_prevent_sleep());
        state.mode = Mode::Awake;
        assert!(state.should_prevent_sleep());
        state.mode = Mode::Sleep;
        state.start(30, 3, 100);
        assert!(!state.should_prevent_sleep());
        state.mode = Mode::Auto;
        assert!(state.should_prevent_sleep());
        state.shutdown = true;
        assert!(!state.should_prevent_sleep());
    }

    #[test]
    fn cleanup_removes_exited_reused_and_expired_processes() {
        let mut state = State::default();
        for pid in 1..=4 {
            state.start(pid, 10, 100);
        }
        state.sessions.get_mut(&4).unwrap().updated_at = 30_000;
        state.prune(30_001, |pid| match pid {
            1 => None,
            2 => Some(20),
            _ => Some(10),
        });
        assert_eq!(state.sessions.keys().copied().collect::<Vec<_>>(), vec![4]);
    }

    #[test]
    fn detects_native_and_npm_agents_without_matching_arbitrary_arguments() {
        assert_eq!(agent_kind("C:\\Tools\\CODEX.EXE", &[]), Some("codex"));
        assert_eq!(agent_kind("claude.exe", &[]), Some("claude"));
        assert_eq!(
            agent_kind(
                "node.exe",
                &[
                    "node".into(),
                    "C:\\Program Files\\node_modules\\@anthropic-ai\\claude-code\\cli.js".into()
                ]
            ),
            Some("claude")
        );
        assert_eq!(agent_kind("powershell.exe", &["claude.exe".into()]), None);
        assert_eq!(
            agent_kind("node.exe", &["node".into(), "my-codex-tool.js".into()]),
            None
        );
    }
}
