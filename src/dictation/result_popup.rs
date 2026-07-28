//! Fallback popup for dictations that landed nowhere.
//!
//! When the transcribed text could not be delivered to a focused UI element,
//! a small pill at the bottom of the screen shows it with a Copy button and
//! a visible countdown; it disappears after 10 seconds. The text is also in
//! the dictation history, so nothing is ever lost.

use objc::declare::ClassDecl;
use objc::runtime::{Object, Sel, BOOL, YES};
use objc::{class, msg_send, sel, sel_impl};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::logging;
use crate::objc_utils::{
    nsstring, CGFloat, Id, NSPoint, NSRect, NSSize, NIL, NS_BACKING_STORE_BUFFERED,
    NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES, NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE,
    NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY, NS_WINDOW_STYLE_MASK_BORDERLESS,
};

const DISMISS_SECS: u64 = 10;

/// Text currently shown, read by the Copy button's Objective-C callback.
static POPUP_TEXT: Mutex<String> = Mutex::new(String::new());
/// Set by the Copy callback; the update loop shortens the countdown on it.
static COPIED: AtomicBool = AtomicBool::new(false);

extern "C" fn copy_pressed(_this: &Object, _: Sel, sender: Id) {
    let text = POPUP_TEXT.lock().unwrap().clone();
    unsafe {
        let pasteboard: Id = msg_send![class!(NSPasteboard), generalPasteboard];
        let _: () = msg_send![pasteboard, clearContents];
        let _: BOOL = msg_send![
            pasteboard,
            setString: nsstring(&text)
            forType: nsstring("public.utf8-plain-text")
        ];
        let _: () = msg_send![sender, setTitle: nsstring("Copied!")];
        let _: () = msg_send![sender, setEnabled: false as BOOL];
    }
    logging::log("[result_popup] Copied dictation to clipboard");
    COPIED.store(true, Ordering::SeqCst);
}

extern "C" fn button_accepts_first_mouse(_this: &Object, _: Sel, _event: Id) -> BOOL {
    // The popup window never becomes key: respond to the very first click
    // instead of consuming it for focus.
    true as BOOL
}

struct ClassPtr(*const objc::runtime::Class);
unsafe impl Send for ClassPtr {}
unsafe impl Sync for ClassPtr {}

fn popup_target_class() -> &'static objc::runtime::Class {
    static CLASS: OnceLock<ClassPtr> = OnceLock::new();
    let class_ptr = CLASS.get_or_init(|| {
        let mut decl = ClassDecl::new("ASPDictationPopupTarget", class!(NSObject))
            .expect("Failed to create ASPDictationPopupTarget class");
        unsafe {
            decl.add_method(
                sel!(copyPressed:),
                copy_pressed as extern "C" fn(&Object, Sel, Id),
            );
        }
        ClassPtr(decl.register() as *const objc::runtime::Class)
    });
    unsafe { &*class_ptr.0 }
}

fn popup_button_class() -> &'static objc::runtime::Class {
    static CLASS: OnceLock<ClassPtr> = OnceLock::new();
    let class_ptr = CLASS.get_or_init(|| {
        let mut decl = ClassDecl::new("ASPPopupButton", class!(NSButton))
            .expect("Failed to create ASPPopupButton class");
        unsafe {
            decl.add_method(
                sel!(acceptsFirstMouse:),
                button_accepts_first_mouse as extern "C" fn(&Object, Sel, Id) -> BOOL,
            );
        }
        ClassPtr(decl.register() as *const objc::runtime::Class)
    });
    unsafe { &*class_ptr.0 }
}

pub struct ResultPopup {
    window: Option<Id>,
    countdown_label: Option<Id>,
    deadline: Option<Instant>,
    shown_secs_left: u64,
    copied: bool,
}

impl ResultPopup {
    pub fn new() -> Self {
        Self {
            window: None,
            countdown_label: None,
            deadline: None,
            shown_secs_left: 0,
            copied: false,
        }
    }

    /// Show the undelivered dictation at the bottom of the screen.
    pub fn show(&mut self, text: &str) {
        self.hide();
        *POPUP_TEXT.lock().unwrap() = text.to_string();
        COPIED.store(false, Ordering::SeqCst);
        self.copied = false;

        unsafe {
            let screen: Id = msg_send![class!(NSScreen), mainScreen];
            if screen.is_null() {
                return;
            }
            let screen_frame: NSRect = msg_send![screen, frame];

            let width: CGFloat = (screen_frame.size.width * 0.5).min(640.0);
            let height: CGFloat = 52.0;
            let frame = NSRect::new(
                NSPoint::new((screen_frame.size.width - width) / 2.0, 40.0),
                NSSize::new(width, height),
            );

            let window: Id = msg_send![class!(NSWindow), alloc];
            let window: Id = msg_send![
                window,
                initWithContentRect: frame
                styleMask: NS_WINDOW_STYLE_MASK_BORDERLESS
                backing: NS_BACKING_STORE_BUFFERED
                defer: false as BOOL
            ];
            if window.is_null() {
                return;
            }

            let _: () = msg_send![window, setLevel: 25i64];
            let _: () = msg_send![window, setOpaque: false as BOOL];
            let _: () = msg_send![window, setHasShadow: YES];
            // The pill is always dark; without this the button bezel/title
            // follow the system (light) appearance and turn black-on-dark.
            let appearance: Id = msg_send![
                class!(NSAppearance),
                appearanceNamed: nsstring("NSAppearanceNameDarkAqua")
            ];
            let _: () = msg_send![window, setAppearance: appearance];
            let clear: Id = msg_send![class!(NSColor), clearColor];
            let _: () = msg_send![window, setBackgroundColor: clear];
            let behavior = NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
                | NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY
                | NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE;
            let _: () = msg_send![window, setCollectionBehavior: behavior];

            let content_view: Id = msg_send![window, contentView];
            let _: () = msg_send![content_view, setWantsLayer: YES];
            let layer: Id = msg_send![content_view, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: 12.0 as CGFloat];
                let bg: Id = msg_send![
                    class!(NSColor),
                    colorWithRed: 0.08
                    green: 0.08
                    blue: 0.08
                    alpha: 0.94
                ];
                let cg_color: Id = msg_send![bg, CGColor];
                let _: () = msg_send![layer, setBackgroundColor: cg_color];
            }

            // Countdown ("10s") pinned right, Copy button next to it, the
            // text fills the rest.
            let countdown_width: CGFloat = 34.0;
            let button_width: CGFloat = 78.0;

            let countdown_frame = NSRect::new(
                NSPoint::new(width - countdown_width - 12.0, (height - 16.0) / 2.0),
                NSSize::new(countdown_width, 16.0),
            );
            let countdown: Id = msg_send![class!(NSTextField), alloc];
            let countdown: Id = msg_send![countdown, initWithFrame: countdown_frame];
            let _: () = msg_send![countdown, setBezeled: false as BOOL];
            let _: () = msg_send![countdown, setDrawsBackground: false as BOOL];
            let _: () = msg_send![countdown, setEditable: false as BOOL];
            let _: () = msg_send![countdown, setSelectable: false as BOOL];
            let small_font: Id = msg_send![class!(NSFont), systemFontOfSize: 11.0 as CGFloat];
            let _: () = msg_send![countdown, setFont: small_font];
            let gray: Id = msg_send![class!(NSColor), secondaryLabelColor];
            let _: () = msg_send![countdown, setTextColor: gray];
            let _: () = msg_send![countdown, setStringValue: nsstring(&format!("{}s", DISMISS_SECS))];
            let _: () = msg_send![content_view, addSubview: countdown];

            let target: Id = msg_send![popup_target_class(), new];
            let button_frame = NSRect::new(
                NSPoint::new(
                    width - countdown_width - button_width - 20.0,
                    (height - 26.0) / 2.0,
                ),
                NSSize::new(button_width, 26.0),
            );
            let button: Id = msg_send![popup_button_class(), alloc];
            let button: Id = msg_send![button, initWithFrame: button_frame];
            let _: () = msg_send![button, setTitle: nsstring("Copy")];
            let _: () = msg_send![button, setBezelStyle: 1i64];
            let _: () = msg_send![button, setTarget: target];
            let _: () = msg_send![button, setAction: sel!(copyPressed:)];
            let _: () = msg_send![content_view, addSubview: button];

            let label_frame = NSRect::new(
                NSPoint::new(16.0, (height - 20.0) / 2.0),
                NSSize::new(width - countdown_width - button_width - 48.0, 20.0),
            );
            let label: Id = msg_send![class!(NSTextField), alloc];
            let label: Id = msg_send![label, initWithFrame: label_frame];
            let _: () = msg_send![label, setBezeled: false as BOOL];
            let _: () = msg_send![label, setDrawsBackground: false as BOOL];
            let _: () = msg_send![label, setEditable: false as BOOL];
            let _: () = msg_send![label, setSelectable: false as BOOL];
            let font: Id = msg_send![class!(NSFont), systemFontOfSize: 13.0 as CGFloat];
            let _: () = msg_send![label, setFont: font];
            let white: Id = msg_send![class!(NSColor), whiteColor];
            let _: () = msg_send![label, setTextColor: white];
            let cell: Id = msg_send![label, cell];
            let _: () = msg_send![cell, setLineBreakMode: 4u64]; // NSLineBreakByTruncatingTail
            let _: () = msg_send![label, setStringValue: nsstring(text)];
            let _: () = msg_send![content_view, addSubview: label];

            let _: () = msg_send![window, makeKeyAndOrderFront: NIL];

            self.window = Some(window);
            self.countdown_label = Some(countdown);
            self.deadline = Some(Instant::now() + std::time::Duration::from_secs(DISMISS_SECS));
            self.shown_secs_left = DISMISS_SECS;
            logging::log("[result_popup] Shown (dictation had no focused target)");
        }
    }

    /// Drive the countdown from the agent update loop.
    pub fn tick(&mut self) {
        if self.window.is_none() {
            return;
        }
        if COPIED.swap(false, Ordering::SeqCst) {
            // Leave "Copied!" visible for a beat, then dismiss.
            self.copied = true;
            self.deadline = Some(Instant::now() + std::time::Duration::from_millis(1500));
            if let Some(label) = self.countdown_label {
                unsafe {
                    let _: () = msg_send![label, setStringValue: nsstring("")];
                }
            }
        }
        let Some(deadline) = self.deadline else {
            return;
        };
        let now = Instant::now();
        if now >= deadline {
            self.hide();
            return;
        }
        if self.copied {
            return;
        }
        let secs_left = (deadline - now).as_secs() + 1;
        if secs_left != self.shown_secs_left {
            self.shown_secs_left = secs_left;
            if let Some(label) = self.countdown_label {
                unsafe {
                    let _: () = msg_send![label, setStringValue: nsstring(&format!("{}s", secs_left))];
                }
            }
        }
    }

    pub fn hide(&mut self) {
        self.countdown_label = None;
        self.deadline = None;
        if let Some(window) = self.window.take() {
            unsafe {
                let _: () = msg_send![window, orderOut: NIL];
                let _: () = msg_send![window, close];
            }
        }
    }
}

impl Drop for ResultPopup {
    fn drop(&mut self) {
        self.hide();
    }
}
