use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

static LOG_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Past this size the log becomes `asp.log.1` (one previous log kept).
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Initialize logging to ~/Library/Logs/AgentsSleepPreventer/asp.log
/// (Linux: ~/.local/state/AgentsSleepPreventer/asp.log)
pub fn init() {
    init_with_startup_message(true);
}

/// Initialize logging without writing a startup line.
pub fn init_quiet() {
    init_with_startup_message(false);
}

fn init_with_startup_message(write_startup: bool) {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    #[cfg(target_os = "macos")]
    let log_dir = home.join("Library/Logs/AgentsSleepPreventer");
    #[cfg(not(target_os = "macos"))]
    let log_dir = dirs::state_dir()
        .unwrap_or_else(|| home.join(".local/state"))
        .join("AgentsSleepPreventer");

    if fs::create_dir_all(&log_dir).is_ok() {
        let log_path = log_dir.join("asp.log");
        *LOG_FILE.lock().unwrap() = Some(log_path.clone());

        if write_startup {
            remove_logged_transcripts(&log_path);
            log_internal(&format!(
                "=== ASP {} started ===",
                env!("CARGO_PKG_VERSION")
            ));
            log_internal(&format!("Executable: {:?}", std::env::current_exe().ok()));
        }
    }
}

/// Versions before 5.0.4 logged every dictation verbatim; drop those lines
/// so dictated secrets do not outlive the upgrade.
fn remove_logged_transcripts(log_path: &Path) {
    const TRANSCRIPT_MARKER: &str = "] [dictation] Transcription: ";
    let Ok(content) = fs::read_to_string(log_path) else {
        return;
    };
    if !content.contains(TRANSCRIPT_MARKER) {
        return;
    }
    let kept: String = content
        .lines()
        .filter(|line| !line.contains(TRANSCRIPT_MARKER))
        .flat_map(|line| [line, "\n"])
        .collect();
    let _ = fs::write(log_path, kept);
}

/// Log a message with timestamp
pub fn log(message: &str) {
    log_internal(message);
    eprintln!("{}", message);
}

fn log_internal(message: &str) {
    let log_path = LOG_FILE.lock().unwrap();
    if let Some(path) = log_path.as_ref() {
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        append(path, &entry(timestamp, message), MAX_LOG_BYTES);
    }
}

/// One entry is one line: a message's own line breaks (an API error, a
/// notification body) would otherwise leave lines without a timestamp.
fn entry(timestamp: u64, message: &str) -> String {
    let flat = message
        .trim_end()
        .replace("\r\n", "\n")
        .replace(['\n', '\r'], " ⏎ ");
    format!("[{timestamp}] {flat}\n")
}

/// Every hook is its own process appending to the same file, so the line
/// goes out in a single write: `writeln!` writes it in pieces, and two hooks
/// firing together interleaved them ("[[17912354841791235484] ] …").
fn append(path: &Path, line: &str, max_bytes: u64) {
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = file.write_all(line.as_bytes());
    if file
        .metadata()
        .is_ok_and(|metadata| metadata.len() > max_bytes)
    {
        rotate(path, &file);
    }
}

/// Only a process whose handle is still the file at `path` renames it: one
/// that saw the same full log a moment late would otherwise push the fresh
/// log over the previous one.
fn rotate(path: &Path, file: &File) {
    let (Ok(open), Ok(current)) = (file.metadata(), fs::metadata(path)) else {
        return;
    };
    if open.ino() == current.ino() && open.dev() == current.dev() {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_logged_transcripts_keeps_other_lines() {
        let path = std::env::temp_dir().join(format!("asp-log-scrub-{}.log", std::process::id()));
        fs::write(
            &path,
            "[1] [dictation] DictateStart event received\n\
             [2] [dictation] Transcription: my secret password\n\
             [3] [dictation] Transcription error: No audio recorded\n",
        )
        .unwrap();

        remove_logged_transcripts(&path);

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[1] [dictation] DictateStart event received\n\
             [3] [dictation] Transcription error: No audio recorded\n"
        );
        let _ = fs::remove_file(path);
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asp-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_entry_is_one_line_even_for_a_message_on_several() {
        assert_eq!(
            entry(7, "[hook-stop] Kept PID 4"),
            "[7] [hook-stop] Kept PID 4\n"
        );
        assert_eq!(
            entry(7, "API Error: 500\r\nDetails: `[cyber]`\n"),
            "[7] API Error: 500 ⏎ Details: `[cyber]`\n"
        );
    }

    #[test]
    fn a_full_log_becomes_the_previous_one() {
        let dir = scratch_dir("rotate");
        let path = dir.join("asp.log");
        fs::write(dir.join("asp.log.1"), "older\n").unwrap();
        append(&path, "[1] first\n", 16);
        append(&path, "[2] second\n", 16);
        assert_eq!(
            fs::read_to_string(dir.join("asp.log.1")).unwrap(),
            "[1] first\n[2] second\n"
        );
        assert!(!path.exists());
        append(&path, "[3] third\n", 16);
        assert_eq!(fs::read_to_string(&path).unwrap(), "[3] third\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_late_process_does_not_rotate_the_fresh_log() {
        let dir = scratch_dir("late");
        let path = dir.join("asp.log");
        fs::write(&path, "[1] full\n").unwrap();
        let late = File::open(&path).unwrap();
        fs::rename(&path, dir.join("asp.log.1")).unwrap();
        fs::write(&path, "[2] fresh\n").unwrap();
        rotate(&path, &late);
        assert_eq!(fs::read_to_string(&path).unwrap(), "[2] fresh\n");
        assert_eq!(
            fs::read_to_string(dir.join("asp.log.1")).unwrap(),
            "[1] full\n"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
