//! Persistent history of dictated messages.
//!
//! Plain-text file, one timestamped entry per dictation, opened from the
//! menu bar (••• → Dictation History). Capped to the newest entries.

use std::fs;
use std::path::PathBuf;

use crate::logging;

const MAX_ENTRIES: usize = 500;
/// Prune only once the file overshoots, so appends stay cheap.
const PRUNE_THRESHOLD: usize = MAX_ENTRIES + 50;

pub fn history_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("AgentsSleepPreventer")
        .join("dictation-history.txt")
}

fn timestamp() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// Append a dictated message; prunes the file to the newest MAX_ENTRIES.
pub fn append(text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }

    let path = history_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let mut content = fs::read_to_string(&path).unwrap_or_default();
    content.push_str(&format!("[{}]\n{}\n\n", timestamp(), text));

    let entry_count = content.lines().filter(|l| is_entry_header(l)).count();
    if entry_count > PRUNE_THRESHOLD {
        content = prune_to_newest(&content, MAX_ENTRIES);
    }

    if let Err(e) = fs::write(&path, content) {
        logging::log(&format!("[history] Failed to write history: {}", e));
    }
}

fn is_entry_header(line: &str) -> bool {
    line.len() >= 21 && line.starts_with('[') && line.ends_with(']')
}

/// Keep only the newest `keep` entries (entries start at a `[timestamp]` line).
fn prune_to_newest(content: &str, keep: usize) -> String {
    let header_positions: Vec<usize> = content
        .lines()
        .scan(0usize, |offset, line| {
            let position = *offset;
            *offset += line.len() + 1;
            Some((position, line))
        })
        .filter(|(_, line)| is_entry_header(line))
        .map(|(position, _)| position)
        .collect();

    if header_positions.len() <= keep {
        return content.to_string();
    }
    content[header_positions[header_positions.len() - keep]..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_newest_entries() {
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("[2026-07-27 10:00:{:02}]\nmessage {}\n\n", i, i));
        }
        let pruned = prune_to_newest(&content, 3);
        assert!(!pruned.contains("message 6"));
        assert!(pruned.starts_with("[2026-07-27 10:00:07]"));
        assert!(pruned.contains("message 9"));
        assert_eq!(pruned.lines().filter(|l| is_entry_header(l)).count(), 3);
    }
}
