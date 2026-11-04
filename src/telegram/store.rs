//! The bot's state on disk, shared by every listener: JSON files written
//! atomically, flock locks, and an append-only journal. The layout is the
//! one sync-telegram (Python) uses, file for file, so a Python listener and
//! an asp listener can run side by side on the same folder and hand over
//! the reading of Telegram to each other.

use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub type Object = Map<String, Value>;

/// Seconds since the epoch, as Python's `time.time()`.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// A held `locks/<name>.lock`; dropping it releases the lock, and so does
/// the process ending, however it ends.
pub struct Lock {
    _file: File,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, parts: &[&str]) -> PathBuf {
        parts.iter().fold(self.root.clone(), |path, part| path.join(part))
    }

    /// The JSON in `file`; None when it is missing or unreadable.
    pub fn read(&self, file: &Path) -> Option<Value> {
        let text = fs::read_to_string(file).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn read_object(&self, file: &Path) -> Option<Object> {
        match self.read(file)? {
            Value::Object(object) => Some(object),
            _ => None,
        }
    }

    /// Atomic write: a reader sees the old file or the new one, never a part.
    pub fn write(&self, file: &Path, data: &Value) -> io::Result<()> {
        let parent = file.parent().unwrap_or(&self.root);
        fs::create_dir_all(parent)?;
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));
        let mut text = serde_json::to_string_pretty(data).map_err(io::Error::other)?;
        text.push('\n');
        fs::write(&temporary, text)?;
        fs::rename(&temporary, file)
    }

    /// `locks/<name>.lock`, or None when another process (or another
    /// descriptor of this one) holds it. `wait` blocks until it is free.
    pub fn lock(&self, name: &str, wait: bool) -> Option<Lock> {
        let file = self.path(&["locks", &format!("{name}.lock")]);
        fs::create_dir_all(file.parent()?).ok()?;
        let handle = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&file)
            .ok()?;
        let operation = if wait {
            libc::LOCK_EX
        } else {
            libc::LOCK_EX | libc::LOCK_NB
        };
        loop {
            if unsafe { libc::flock(handle.as_raw_fd(), operation) } == 0 {
                return Some(Lock { _file: handle });
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return None;
            }
        }
    }

    pub fn held(&self, name: &str) -> bool {
        self.lock(name, false).is_none()
    }

    /// One line in the journal, written in a single call: several listeners
    /// append to it at the same time.
    pub fn journal(&self, entry: &Value) {
        let Ok(mut line) = serde_json::to_string(entry) else {
            return;
        };
        line.push('\n');
        if fs::create_dir_all(&self.root).is_err() {
            return;
        }
        if let Ok(mut file) = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(self.path(&["journal.jsonl"]))
        {
            let _ = file.write_all(line.as_bytes());
        }
    }

    pub fn entries(&self) -> Vec<Object> {
        let Ok(text) = fs::read_to_string(self.path(&["journal.jsonl"])) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| match serde_json::from_str(line) {
                Ok(Value::Object(object)) => Some(object),
                _ => None,
            })
            .collect()
    }

    /// The `*.json` files of a folder, sorted by name.
    pub fn json_files(&self, folder: &Path) -> Vec<PathBuf> {
        let Ok(read) = fs::read_dir(folder) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = read
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        files
    }
}

/// Field accessors on a JSON object, with Python's `dict.get` leniency.
pub trait Fields {
    fn int(&self, key: &str) -> Option<i64>;
    fn float(&self, key: &str) -> f64;
    fn text(&self, key: &str) -> Option<&str>;
    fn flag(&self, key: &str) -> bool;
    fn object(&self, key: &str) -> Option<&Object>;
    fn list(&self, key: &str) -> &[Value];
}

impl Fields for Object {
    fn int(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|f| f as i64)),
            _ => None,
        }
    }

    fn float(&self, key: &str) -> f64 {
        self.get(key).and_then(Value::as_f64).unwrap_or(0.0)
    }

    fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    fn flag(&self, key: &str) -> bool {
        match self.get(key) {
            Some(Value::Bool(value)) => *value,
            Some(Value::Null) | None => false,
            Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
            Some(Value::String(text)) => !text.is_empty(),
            Some(Value::Array(items)) => !items.is_empty(),
            Some(Value::Object(object)) => !object.is_empty(),
        }
    }

    fn object(&self, key: &str) -> Option<&Object> {
        self.get(key).and_then(Value::as_object)
    }

    fn list(&self, key: &str) -> &[Value] {
        self.get(key)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_is_exclusive_even_within_one_process() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let first = store.lock("poll", false);
        assert!(first.is_some());
        assert!(store.lock("poll", false).is_none());
        assert!(store.held("poll"));
        drop(first);
        // A test running in parallel may fork right then: its child shares
        // the lock until it execs, a few milliseconds.
        let released = (0..100).any(|_| {
            let free = !store.held("poll");
            if !free {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            free
        });
        assert!(released);
    }

    #[test]
    fn writes_are_atomic_and_journal_lines_parse_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let file = store.path(&["sessions", "asp.json"]);
        store.write(&file, &serde_json::json!({"name": "asp", "thread_id": null})).unwrap();
        assert_eq!(store.read_object(&file).unwrap().text("name"), Some("asp"));
        store.journal(&serde_json::json!({"from": "claude", "text": "un\ndeux"}));
        store.journal(&serde_json::json!({"from": "bot"}));
        let entries = store.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text("text"), Some("un\ndeux"));
    }
}
