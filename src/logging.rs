use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

static LOG_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Initialize logging to ~/Library/Logs/AgentsSleepPreventer/asp.log
pub fn init() {
    init_with_startup_message(true);
}

/// Initialize logging without writing a startup line.
pub fn init_quiet() {
    init_with_startup_message(false);
}

fn init_with_startup_message(write_startup: bool) {
    let log_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Library/Logs/AgentsSleepPreventer");

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
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(file, "[{}] {}", timestamp, message);
        }
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
}
