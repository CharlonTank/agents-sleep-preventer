//! Agent notifications: hooks spool JSON files here, the running app drains
//! the spool and posts macOS notifications with the app's identity.

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel, BOOL, YES};
use objc::{class, msg_send, sel, sel_impl};

use crate::logging;
use crate::objc_utils::{nsstring, Id};
use crate::settings::AppSettings;

const SPOOL_DIR: &str = "/tmp/asp_notifications";

/// Minimum task duration before a "finished" notification is worth sending.
/// Shorter tasks mean the user is probably still watching the terminal.
pub const TASK_DONE_MIN_SECS: u64 = 45;

#[derive(serde::Serialize, serde::Deserialize)]
struct SpooledNotification {
    title: String,
    body: String,
    /// Agent PID to focus when the notification is clicked.
    #[serde(default)]
    pid: Option<u32>,
}

/// Queue a notification for the running app to post. Cheap and safe to call
/// from short-lived hook processes.
pub fn spool(title: &str, body: &str, pid: Option<u32>) {
    if !AppSettings::load().notifications.enabled {
        return;
    }
    if fs::create_dir_all(SPOOL_DIR).is_err() {
        return;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = PathBuf::from(SPOOL_DIR).join(format!("{}-{}.json", std::process::id(), nanos));
    let payload = SpooledNotification {
        title: title.to_string(),
        body: body.to_string(),
        pid,
    };
    if let Ok(json) = serde_json::to_string(&payload) {
        let _ = fs::write(path, json);
    }
}

/// Post every spooled notification, then remove it. Called from the app's
/// main loops (~1s cadence).
pub fn drain_and_post() {
    let entries = match fs::read_dir(SPOOL_DIR) {
        Ok(e) => e,
        Err(_) => return,
    };

    let enabled = AppSettings::load().notifications.enabled;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if let Ok(content) = fs::read_to_string(&path) {
            if enabled {
                if let Ok(n) = serde_json::from_str::<SpooledNotification>(&content) {
                    post(&n.title, &n.body, n.pid);
                    logging::log(&format!("[notifications] Posted: {} — {}", n.title, n.body));
                }
            }
        }
        let _ = fs::remove_file(&path);
    }
}

extern "C" fn did_activate_notification(
    _this: &Object,
    _: Sel,
    _center: Id,
    notification: Id,
) {
    unsafe {
        let user_info: Id = msg_send![notification, userInfo];
        if user_info.is_null() {
            return;
        }
        let value: Id = msg_send![user_info, objectForKey: nsstring("pid")];
        if value.is_null() {
            return;
        }
        let pid: i64 = msg_send![value, longLongValue];
        if pid > 0 {
            logging::log(&format!("[notifications] Clicked, focusing PID {}", pid));
            crate::focus_terminal_by_pid(pid as u32);
        }
    }
}

extern "C" fn should_present_notification(
    _this: &Object,
    _: Sel,
    _center: Id,
    _notification: Id,
) -> BOOL {
    YES
}

struct ClassPtr(*const Class);
unsafe impl Send for ClassPtr {}
unsafe impl Sync for ClassPtr {}

fn delegate_class() -> &'static Class {
    static CLASS: OnceLock<ClassPtr> = OnceLock::new();
    let class_ptr = CLASS.get_or_init(|| {
        let mut decl = ClassDecl::new("ASPNotificationDelegate", class!(NSObject))
            .expect("Failed to create ASPNotificationDelegate class");
        unsafe {
            decl.add_method(
                sel!(userNotificationCenter:didActivateNotification:),
                did_activate_notification as extern "C" fn(&Object, Sel, Id, Id),
            );
            decl.add_method(
                sel!(userNotificationCenter:shouldPresentNotification:),
                should_present_notification as extern "C" fn(&Object, Sel, Id, Id) -> BOOL,
            );
        }
        ClassPtr(decl.register() as *const Class)
    });
    unsafe { &*class_ptr.0 }
}

/// Install the notification-click handler (focuses the agent's terminal).
/// Call once from the app's long-running loops before draining.
pub fn init_click_handler() {
    static DELEGATE: OnceLock<ClassPtr> = OnceLock::new();
    unsafe {
        let center: Id = msg_send![
            class!(NSUserNotificationCenter),
            defaultUserNotificationCenter
        ];
        if center.is_null() {
            return;
        }
        let delegate = DELEGATE.get_or_init(|| {
            let instance: Id = msg_send![delegate_class(), new];
            ClassPtr(instance as *const Class)
        });
        let _: () = msg_send![center, setDelegate: delegate.0 as Id];
    }
}

/// Deliver a macOS notification via NSUserNotificationCenter. Works because
/// the running process lives inside the app bundle (identity + icon).
fn post(title: &str, body: &str, pid: Option<u32>) {
    unsafe {
        let center: Id = msg_send![
            class!(NSUserNotificationCenter),
            defaultUserNotificationCenter
        ];
        if center.is_null() {
            logging::log("[notifications] No notification center (not running from bundle?)");
            return;
        }
        let notification: Id = msg_send![class!(NSUserNotification), new];
        let _: () = msg_send![notification, setTitle: nsstring(title)];
        let _: () = msg_send![notification, setInformativeText: nsstring(body)];
        if let Some(pid) = pid {
            let user_info: Id = msg_send![
                class!(NSDictionary),
                dictionaryWithObject: nsstring(&pid.to_string())
                forKey: nsstring("pid")
            ];
            let _: () = msg_send![notification, setUserInfo: user_info];
        }
        let _: () = msg_send![center, deliverNotification: notification];
        let _: () = msg_send![notification, release];
    }
}

/// Human-readable duration like "2m 05s".
pub fn format_duration(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(30), "30s");
        assert_eq!(format_duration(60), "1m 00s");
        assert_eq!(format_duration(125), "2m 05s");
    }
}
