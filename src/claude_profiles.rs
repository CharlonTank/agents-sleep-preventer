//! Where Claude Code keeps its state, on every platform.

use std::fs;
use std::path::{Path, PathBuf};

/// Claude Code profiles: ~/.claude plus every other `~/.claude-*` config
/// dir (CLAUDE_CONFIG_DIR) that has been used, i.e. has a projects/ dir.
pub fn claude_config_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".claude")];
    if let Ok(entries) = fs::read_dir(home) {
        let mut profiles: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".claude-"))
            .map(|entry| entry.path())
            .filter(|path| path.join("projects").is_dir())
            .collect();
        profiles.sort();
        dirs.extend(profiles);
    }
    dirs
}

/// A profile as people call it: "default" for ~/.claude, "work" for
/// ~/.claude-work, else its folder's name.
pub fn profile_label(home: &Path, dir: &Path) -> String {
    if dir == home.join(".claude") {
        return "default".to_string();
    }
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());
    match name.strip_prefix(".claude-") {
        Some(short) if !short.is_empty() => short.to_string(),
        _ => name,
    }
}

/// The profile a session runs under: its CLAUDE_CONFIG_DIR, else ~/.claude.
pub fn profile_dir(home: &Path, config_dir: Option<&str>) -> PathBuf {
    match config_dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(dir) => match dir.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(dir.trim_end_matches('/')),
        },
        None => home.join(".claude"),
    }
}

/// The Claude account a profile is logged into: `oauthAccount.emailAddress`
/// in its .claude.json. Claude Code without CLAUDE_CONFIG_DIR keeps the
/// default profile's in ~/.claude.json, so the fresher of the two counts.
/// None with an API key, or logged out.
pub fn profile_account(home: &Path, dir: &Path) -> Option<String> {
    let mut files = vec![dir.join(".claude.json")];
    if dir == home.join(".claude") {
        files.push(home.join(".claude.json"));
    }
    files
        .into_iter()
        .filter_map(|file| {
            let modified = fs::metadata(&file).ok()?.modified().ok()?;
            let config: serde_json::Value = serde_json::from_str(&fs::read_to_string(&file).ok()?).ok()?;
            let email = config.get("oauthAccount")?.get("emailAddress")?.as_str()?.to_string();
            Some((modified, email))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, email)| email)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_named_after_their_folder() {
        let home = Path::new("/Users/x");
        assert_eq!(profile_label(home, &home.join(".claude")), "default");
        assert_eq!(profile_label(home, &home.join(".claude-work")), "work");
        assert_eq!(profile_label(home, Path::new("/opt/claude-team")), "claude-team");
        assert_eq!(profile_dir(home, None), home.join(".claude"));
        assert_eq!(profile_dir(home, Some("~/.claude-work")), home.join(".claude-work"));
        assert_eq!(profile_dir(home, Some("/Users/x/.claude-work/")), home.join(".claude-work"));
    }

    #[test]
    fn the_account_is_the_fresher_config_for_the_default_profile() {
        let home = tempfile::tempdir().unwrap();
        let default = home.path().join(".claude");
        fs::create_dir_all(&default).unwrap();
        let work = home.path().join(".claude-work");
        fs::create_dir_all(&work).unwrap();
        let account = |email: &str| format!(r#"{{"oauthAccount":{{"emailAddress":"{email}"}}}}"#);
        fs::write(home.path().join(".claude.json"), account("old@example.com")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(default.join(".claude.json"), account("new@example.com")).unwrap();
        fs::write(work.join(".claude.json"), r#"{"primaryApiKey":"set"}"#).unwrap();
        assert_eq!(profile_account(home.path(), &default).as_deref(), Some("new@example.com"));
        assert_eq!(profile_account(home.path(), &work), None);
    }
}
