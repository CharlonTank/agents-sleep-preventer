//! Per-user directory for the state hooks share with the app: working-PID
//! markers, attention markers and the notification spool.
//!
//! It lives in the per-user temp dir (/var/folders/…/T/, mode 0700) rather
//! than the shared /tmp, so another local user cannot forge markers to keep
//! the Mac awake or spoof agent notifications.

use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Locations used before 5.0.4, in the world-writable /tmp.
pub const LEGACY_TMP_DIRS: [&str; 4] = [
    "/tmp/agents_working_pids",
    "/tmp/claude_working_pids",
    "/tmp/asp_attention",
    "/tmp/asp_notifications",
];

/// Derived from the uid, not the environment, so hooks, the CLI and the app
/// agree even when TMPDIR differs between them.
fn darwin_user_temp_dir() -> Option<PathBuf> {
    let mut buf = [0 as libc::c_char; 1024];
    let len = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr(),
            buf.len(),
        )
    };
    if len == 0 || len > buf.len() {
        return None;
    }
    let dir = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().ok()?;
    Some(PathBuf::from(dir))
}

fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        darwin_user_temp_dir()
            .or_else(dirs::cache_dir)
            .unwrap_or_else(std::env::temp_dir)
            .join("AgentsSleepPreventer")
    })
}

pub fn pids_dir() -> PathBuf {
    root().join("working_pids")
}

pub fn attention_dir() -> PathBuf {
    root().join("attention")
}

pub fn notifications_dir() -> PathBuf {
    root().join("notifications")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dirs_are_private_to_the_user() {
        let dir = pids_dir();
        assert!(!dir.starts_with("/tmp"), "{}", dir.display());
        assert!(dir.ends_with("AgentsSleepPreventer/working_pids"));

        std::fs::create_dir_all(&dir).unwrap();
        use std::os::unix::fs::MetadataExt;
        let parent = std::fs::metadata(root().parent().unwrap()).unwrap();
        assert_eq!(parent.uid(), unsafe { libc::getuid() });
        assert_eq!(parent.mode() & 0o077, 0, "per-user temp dir must be 0700");
    }
}
