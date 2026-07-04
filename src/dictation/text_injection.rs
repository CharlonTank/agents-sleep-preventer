use core_foundation::base::{CFIndex, CFRelease, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use std::ffi::c_void;
use std::ptr;

use crate::logging;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: CGKeyCode,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventKeyboardSetUnicodeString(
        event: CGEventRef,
        string_length: CFIndex,
        unicode_string: *const UniChar,
    );
    fn CGEventPost(tap_location: CGEventTapLocation, event: CGEventRef);
}

type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;
type CGEventTapLocation = u32;
type CGKeyCode = u16;
type UniChar = u16;

const K_CG_HID_EVENT_TAP: CGEventTapLocation = 0;

/// Inject text by posting key events (does not touch the clipboard).
pub fn inject_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }

    // Check if we have accessibility permission
    let trusted = unsafe { AXIsProcessTrusted() };
    if !trusted {
        return Err("Accessibility permission required".to_string());
    }

    inject_via_keystrokes(text)?;

    logging::log(&format!(
        "[text_injection] Successfully injected {} chars via keystrokes",
        text.len()
    ));
    Ok(())
}

fn inject_via_keystrokes(text: &str) -> Result<(), String> {
    let normalized = text.replace('\n', "\r");
    let utf16: Vec<UniChar> = normalized.encode_utf16().collect();
    if utf16.is_empty() {
        return Ok(());
    }

    unsafe {
        let key_down = CGEventCreateKeyboardEvent(ptr::null_mut(), 0, true);
        if key_down.is_null() {
            return Err("Failed to create key down event".to_string());
        }
        CGEventKeyboardSetUnicodeString(key_down, utf16.len() as CFIndex, utf16.as_ptr());
        CGEventPost(K_CG_HID_EVENT_TAP, key_down);
        CFRelease(key_down as *const c_void);

        let key_up = CGEventCreateKeyboardEvent(ptr::null_mut(), 0, false);
        if key_up.is_null() {
            return Err("Failed to create key up event".to_string());
        }
        CGEventKeyboardSetUnicodeString(key_up, utf16.len() as CFIndex, utf16.as_ptr());
        CGEventPost(K_CG_HID_EVENT_TAP, key_up);
        CFRelease(key_up as *const c_void);
    }

    Ok(())
}

/// Check if accessibility is enabled for this app
pub fn check_accessibility_permission() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Show the system Accessibility prompt. Crucially, this also adds the app to
/// the Accessibility list in System Settings (switch off), so the user only
/// has to flip a switch instead of clicking "+" and hunting for the app.
/// macOS shows the dialog only once per app; later calls are no-ops.
pub fn request_accessibility_permission() -> bool {
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let options = CFDictionary::from_CFType_pairs(&[(
            key.as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )]);
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as *const c_void)
    }
}
