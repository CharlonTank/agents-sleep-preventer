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
