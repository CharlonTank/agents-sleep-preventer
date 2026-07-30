//! Settings window with tabbed interface

use dispatch::Queue;
use objc::declare::ClassDecl;
use objc::runtime::{Object, Sel, BOOL};
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::objc_utils::{
    nsstring, nsstring_to_string, AutoreleasePool, CGFloat, Id, NSPoint, NSRect, NSSize, NIL,
    NS_BACKING_STORE_BUFFERED,
};

use super::{
    AppSettings, ResolvedHotkey, CG_FLAG_COMMAND, CG_FLAG_CONTROL, CG_FLAG_FN, CG_FLAG_OPTION,
    CG_FLAG_SHIFT,
};

const NS_WINDOW_STYLE_MASK_TITLED: usize = 1 << 0;
const NS_WINDOW_STYLE_MASK_CLOSABLE: usize = 1 << 1;

fn is_main_thread() -> bool {
    unsafe {
        let is_main: BOOL = msg_send![class!(NSThread), isMainThread];
        is_main
    }
}

fn run_on_main_thread<T, F>(work: F) -> T
where
    F: Send + FnOnce() -> T,
    T: Send,
{
    if is_main_thread() {
        work()
    } else {
        Queue::main().exec_sync(work)
    }
}

fn ns_color(red: CGFloat, green: CGFloat, blue: CGFloat, alpha: CGFloat) -> Id {
    unsafe {
        msg_send![
            class!(NSColor),
            colorWithRed: red
            green: green
            blue: blue
            alpha: alpha
        ]
    }
}

unsafe fn create_label(text: &str, frame: NSRect, font: Id, color: Id) -> Id {
    let label: Id = msg_send![class!(NSTextField), alloc];
    let label: Id = msg_send![label, initWithFrame: frame];
    let _: () = msg_send![label, setStringValue: nsstring(text)];
    let _: () = msg_send![label, setBezeled: false as BOOL];
    let _: () = msg_send![label, setDrawsBackground: false as BOOL];
    let _: () = msg_send![label, setEditable: false as BOOL];
    let _: () = msg_send![label, setSelectable: false as BOOL];
    let _: () = msg_send![label, setFont: font];
    let _: () = msg_send![label, setTextColor: color];
    label
}

#[derive(Clone, Copy)]
struct SendPtr(*mut c_void);

unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

impl SendPtr {
    fn into_ptr(self) -> *mut c_void {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    Save,
    Cancel,
}

struct SettingsState {
    action: Mutex<Option<SettingsAction>>,
    settings: Mutex<AppSettings>,
}

impl SettingsState {
    fn new(settings: AppSettings) -> Self {
        Self {
            action: Mutex::new(None),
            settings: Mutex::new(settings),
        }
    }

    fn set_action(&self, action_value: SettingsAction) {
        let mut action = self.action.lock().unwrap();
        *action = Some(action_value);
    }

    fn take_action(&self) -> Option<SettingsAction> {
        self.action.lock().unwrap().take()
    }

    fn get_settings(&self) -> AppSettings {
        self.settings.lock().unwrap().clone()
    }

    fn update_sleep_enabled(&self, enabled: bool) {
        let mut settings = self.settings.lock().unwrap();
        settings.sleep_prevention.enabled = enabled;
    }

    fn update_notifications_enabled(&self, enabled: bool) {
        let mut settings = self.settings.lock().unwrap();
        settings.notifications.enabled = enabled;
    }

    fn update_language(&self, language: String) {
        let mut settings = self.settings.lock().unwrap();
        settings.speech_to_text.language = language;
    }

    fn update_model(&self, model: String) {
        let mut settings = self.settings.lock().unwrap();
        settings.speech_to_text.model = model;
    }

    fn update_hotkey(&self, hotkey: String) {
        let mut settings = self.settings.lock().unwrap();
        settings.speech_to_text.hotkey = hotkey;
    }

    fn update_sound_volume(&self, volume: f32) {
        let mut settings = self.settings.lock().unwrap();
        settings.speech_to_text.sound_volume = volume.clamp(0.0, 1.0);
    }

    fn update_sound_muted(&self, muted: bool) {
        let mut settings = self.settings.lock().unwrap();
        settings.speech_to_text.sound_muted = muted;
    }

    fn update_vocabulary(&self, words: Vec<String>) {
        let mut settings = self.settings.lock().unwrap();
        settings.speech_to_text.vocabulary_words = words;
    }
}

extern "C" fn button_pressed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const SettingsState);
            let tag: i64 = msg_send![sender, tag];
            let action = if tag == 1 {
                SettingsAction::Save
            } else {
                SettingsAction::Cancel
            };
            state.set_action(action);
        }

        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, stopModal];
    }
}

extern "C" fn toggle_changed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const SettingsState);
            let checkbox_state: i64 = msg_send![sender, state];
            let enabled = checkbox_state == 1;
            state.update_sleep_enabled(enabled);
        }
    }
}

extern "C" fn notifications_toggle_changed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const SettingsState);
            let checkbox_state: i64 = msg_send![sender, state];
            state.update_notifications_enabled(checkbox_state == 1);
        }
    }
}

extern "C" fn language_changed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const SettingsState);
            let selected_index: i64 = msg_send![sender, indexOfSelectedItem];
            let languages = AppSettings::supported_languages();
            if (selected_index as usize) < languages.len() {
                let (code, _) = languages[selected_index as usize];
                state.update_language(code.to_string());
            }
        }
    }
}

extern "C" fn model_changed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const SettingsState);
            let selected_index: i64 = msg_send![sender, indexOfSelectedItem];
            let models = AppSettings::supported_models();
            if (selected_index as usize) < models.len() {
                let id = models[selected_index as usize].id;
                state.update_model(id.to_string());
            }
        }
    }
}

/// Slider tracks live so the volume can be judged against a test cue.
extern "C" fn sound_volume_changed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if state_ptr.is_null() {
            return;
        }
        let value: f64 = msg_send![sender, doubleValue];
        (*(state_ptr as *const SettingsState)).update_sound_volume(value as f32 / 100.0);
    }
}

extern "C" fn sound_mute_changed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if state_ptr.is_null() {
            return;
        }
        let checked: i64 = msg_send![sender, state];
        let muted = checked == 1;
        (*(state_ptr as *const SettingsState)).update_sound_muted(muted);

        // Grey the slider out while muted.
        let slider = SOUND_VOLUME_SLIDER.load(Ordering::SeqCst);
        if !slider.is_null() {
            let _: () = msg_send![slider as Id, setEnabled: (!muted) as BOOL];
        }
    }
}

extern "C" fn hotkey_changed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const SettingsState);
            let selected_index: i64 = msg_send![sender, indexOfSelectedItem];
            let hotkeys = AppSettings::supported_hotkeys();
            if (selected_index as usize) < hotkeys.len() {
                let id = hotkeys[selected_index as usize].id;
                state.update_hotkey(id.to_string());
                // Picking a preset discards any recorded custom combo.
                HOTKEY_CAPTURING.store(false, Ordering::SeqCst);
                set_record_button_title("Record…");
            }
        }
    }
}

// ---- Hotkey recording ------------------------------------------------------
//
// Clicking "Record…" arms capture mode; the ASPSettingsWindow subclass then
// intercepts keyDown/flagsChanged. A combo is either modifiers + a regular
// key (finalized on keyDown) or modifiers only (finalized when they are all
// released). Escape cancels.

const HOTKEY_MOD_MASK: u64 =
    CG_FLAG_FN | CG_FLAG_CONTROL | CG_FLAG_OPTION | CG_FLAG_SHIFT | CG_FLAG_COMMAND;

static HOTKEY_CAPTURING: AtomicBool = AtomicBool::new(false);
/// Modifiers seen held together while capturing a modifiers-only combo.
static HOTKEY_CAPTURE_SEEN: AtomicU64 = AtomicU64::new(0);
static HOTKEY_STATE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static HOTKEY_RECORD_BUTTON: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static HOTKEY_POPUP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// Disabled while the cues are muted.
static SOUND_VOLUME_SLIDER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn hotkey_symbols(mask: u64) -> String {
    let mut symbols = String::new();
    if mask & CG_FLAG_FN != 0 {
        symbols.push_str("fn");
    }
    if mask & CG_FLAG_CONTROL != 0 {
        symbols.push('⌃');
    }
    if mask & CG_FLAG_OPTION != 0 {
        symbols.push('⌥');
    }
    if mask & CG_FLAG_SHIFT != 0 {
        symbols.push('⇧');
    }
    if mask & CG_FLAG_COMMAND != 0 {
        symbols.push('⌘');
    }
    symbols
}

fn special_key_name(keycode: u16) -> Option<&'static str> {
    Some(match keycode {
        36 => "Return",
        48 => "Tab",
        49 => "Space",
        51 => "Delete",
        117 => "⌦",
        123 => "←",
        124 => "→",
        125 => "↓",
        126 => "↑",
        115 => "Home",
        119 => "End",
        116 => "PgUp",
        121 => "PgDn",
        122 => "F1",
        120 => "F2",
        99 => "F3",
        118 => "F4",
        96 => "F5",
        97 => "F6",
        98 => "F7",
        100 => "F8",
        101 => "F9",
        109 => "F10",
        103 => "F11",
        111 => "F12",
        105 => "F13",
        107 => "F14",
        113 => "F15",
        106 => "F16",
        64 => "F17",
        79 => "F18",
        80 => "F19",
        _ => return None,
    })
}

/// Keys usable without any modifier (they don't type text).
fn is_bare_key_allowed(keycode: u16) -> bool {
    matches!(special_key_name(keycode), Some(name) if name.starts_with('F'))
}

fn set_record_button_title(title: &str) {
    let button = HOTKEY_RECORD_BUTTON.load(Ordering::SeqCst);
    if !button.is_null() {
        unsafe {
            let _: () = msg_send![button as Id, setTitle: nsstring(title)];
        }
    }
}

fn finish_hotkey_capture(hotkey: ResolvedHotkey) {
    HOTKEY_CAPTURING.store(false, Ordering::SeqCst);
    HOTKEY_CAPTURE_SEEN.store(0, Ordering::SeqCst);

    let state_ptr = HOTKEY_STATE.load(Ordering::SeqCst);
    if !state_ptr.is_null() {
        unsafe {
            (*(state_ptr as *const SettingsState)).update_hotkey(hotkey.custom_id());
        }
    }
    set_record_button_title(&hotkey.label);

    // A recorded combo replaces any preset selection.
    let popup = HOTKEY_POPUP.load(Ordering::SeqCst);
    if !popup.is_null() {
        unsafe {
            let _: () = msg_send![popup as Id, selectItemAtIndex: -1i64];
        }
    }
}

fn cancel_hotkey_capture() {
    HOTKEY_CAPTURING.store(false, Ordering::SeqCst);
    HOTKEY_CAPTURE_SEEN.store(0, Ordering::SeqCst);

    let state_ptr = HOTKEY_STATE.load(Ordering::SeqCst);
    let title = if state_ptr.is_null() {
        "Record…".to_string()
    } else {
        let hotkey = unsafe { (*(state_ptr as *const SettingsState)).get_settings() }
            .speech_to_text
            .hotkey;
        ResolvedHotkey::parse_custom(&hotkey)
            .map(|h| h.label)
            .unwrap_or_else(|| "Record…".to_string())
    };
    set_record_button_title(&title);
}

unsafe fn captured_key_name(event: Id, keycode: u16) -> String {
    if let Some(name) = special_key_name(keycode) {
        return name.to_string();
    }
    let chars: Id = msg_send![event, charactersIgnoringModifiers];
    let text = nsstring_to_string(chars)
        .unwrap_or_default()
        .trim()
        .to_uppercase();
    if text.is_empty() || text.chars().any(char::is_control) {
        format!("Key{}", keycode)
    } else {
        text
    }
}

unsafe fn handle_capture_key_down(event: Id) {
    let keycode: u16 = msg_send![event, keyCode];
    if keycode == 53 {
        // Escape
        cancel_hotkey_capture();
        return;
    }
    let flags: u64 = msg_send![event, modifierFlags];
    let mask = flags & HOTKEY_MOD_MASK;
    if mask == 0 && !is_bare_key_allowed(keycode) {
        // A bare letter would fire (and be swallowed) on normal typing.
        set_record_button_title("Add a modifier…");
        return;
    }
    let label = format!("{}{}", hotkey_symbols(mask), captured_key_name(event, keycode));
    finish_hotkey_capture(ResolvedHotkey {
        mask,
        keycode: Some(keycode),
        label,
    });
}

extern "C" fn window_key_down(this: &Object, _: Sel, event: Id) {
    if !HOTKEY_CAPTURING.load(Ordering::SeqCst) {
        unsafe {
            let _: () = msg_send![super(this, class!(NSWindow)), keyDown: event];
        }
        return;
    }
    unsafe { handle_capture_key_down(event) }
}

extern "C" fn window_perform_key_equivalent(this: &Object, _: Sel, event: Id) -> BOOL {
    // ⌘-combos are routed here before keyDown; claim them while capturing.
    if HOTKEY_CAPTURING.load(Ordering::SeqCst) {
        unsafe { handle_capture_key_down(event) }
        return true as BOOL;
    }
    unsafe { msg_send![super(this, class!(NSWindow)), performKeyEquivalent: event] }
}

extern "C" fn window_flags_changed(this: &Object, _: Sel, event: Id) {
    if !HOTKEY_CAPTURING.load(Ordering::SeqCst) {
        unsafe {
            let _: () = msg_send![super(this, class!(NSWindow)), flagsChanged: event];
        }
        return;
    }
    let flags: u64 = unsafe { msg_send![event, modifierFlags] };
    let mask = flags & HOTKEY_MOD_MASK;

    if mask != 0 {
        let seen = HOTKEY_CAPTURE_SEEN.fetch_or(mask, Ordering::SeqCst) | mask;
        set_record_button_title(&format!("{}…", hotkey_symbols(seen)));
        return;
    }

    let seen = HOTKEY_CAPTURE_SEEN.swap(0, Ordering::SeqCst);
    if seen == 0 {
        return;
    }
    // Modifiers-only combo, finalized on release. Reject a single non-fn
    // modifier (it would fire on every plain Shift/⌘ press).
    if seen.count_ones() >= 2 || seen & CG_FLAG_FN != 0 {
        finish_hotkey_capture(ResolvedHotkey {
            mask: seen,
            keycode: None,
            label: hotkey_symbols(seen),
        });
    } else {
        cancel_hotkey_capture();
    }
}

extern "C" fn window_accepts_first_responder(_: &Object, _: Sel) -> BOOL {
    true as BOOL
}

fn settings_window_class() -> &'static objc::runtime::Class {
    static CLASS: OnceLock<ClassPtr> = OnceLock::new();
    let class_ptr = CLASS.get_or_init(|| {
        let superclass = class!(NSWindow);
        let mut decl = ClassDecl::new("ASPSettingsWindow", superclass)
            .expect("Failed to create ASPSettingsWindow class");
        unsafe {
            decl.add_method(
                sel!(keyDown:),
                window_key_down as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(flagsChanged:),
                window_flags_changed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(performKeyEquivalent:),
                window_perform_key_equivalent as extern "C" fn(&Object, Sel, Id) -> BOOL,
            );
            decl.add_method(
                sel!(acceptsFirstResponder),
                window_accepts_first_responder as extern "C" fn(&Object, Sel) -> BOOL,
            );
        }
        ClassPtr(decl.register() as *const objc::runtime::Class)
    });

    unsafe { &*class_ptr.0 }
}

extern "C" fn record_hotkey_pressed(_this: &Object, _: Sel, sender: Id) {
    HOTKEY_CAPTURING.store(true, Ordering::SeqCst);
    HOTKEY_CAPTURE_SEEN.store(0, Ordering::SeqCst);
    set_record_button_title("Press shortcut…");
    unsafe {
        let window: Id = msg_send![sender, window];
        let _: BOOL = msg_send![window, makeFirstResponder: window];
    }
}

extern "C" fn window_will_close(this: &Object, _: Sel, _notification: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const SettingsState);
            // If no action was set, treat as cancel
            if state.take_action().is_none() {
                state.set_action(SettingsAction::Cancel);
            }
        }

        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, stopModal];
    }
}

struct ClassPtr(*const objc::runtime::Class);

unsafe impl Send for ClassPtr {}
unsafe impl Sync for ClassPtr {}

fn settings_target_class() -> &'static objc::runtime::Class {
    static CLASS: OnceLock<ClassPtr> = OnceLock::new();
    let class_ptr = CLASS.get_or_init(|| {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("ASPSettingsTarget", superclass)
            .expect("Failed to create ASPSettingsTarget class");
        decl.add_ivar::<*mut c_void>("rustState");
        decl.add_ivar::<*mut c_void>("vocabularyTextView");
        unsafe {
            decl.add_method(
                sel!(buttonPressed:),
                button_pressed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(toggleChanged:),
                toggle_changed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(notificationsToggleChanged:),
                notifications_toggle_changed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(languageChanged:),
                language_changed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(modelChanged:),
                model_changed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(hotkeyChanged:),
                hotkey_changed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(recordHotkeyPressed:),
                record_hotkey_pressed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(soundVolumeChanged:),
                sound_volume_changed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(soundMuteChanged:),
                sound_mute_changed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(windowWillClose:),
                window_will_close as extern "C" fn(&Object, Sel, Id),
            );
        }
        ClassPtr(decl.register() as *const objc::runtime::Class)
    });

    unsafe { &*class_ptr.0 }
}

pub struct SettingsWindow {
    state: Arc<SettingsState>,
    state_ptr: *const SettingsState,
    window: SendPtr,
    target: SendPtr,
    vocabulary_text_view: SendPtr,
    previous_policy: i64,
}

impl SettingsWindow {
    pub fn new() -> Self {
        let settings = AppSettings::load();
        let state = Arc::new(SettingsState::new(settings.clone()));
        let state_ptr = Arc::into_raw(state.clone());
        let state_ptr_send = SendPtr(state_ptr as *mut c_void);

        let (window, target, vocabulary_text_view, previous_policy) = run_on_main_thread(
            move || unsafe {
                let _pool = AutoreleasePool::new();

                let app: Id = msg_send![class!(NSApplication), sharedApplication];
                let previous_policy: i64 = msg_send![app, activationPolicy];
                let _: () = msg_send![app, setActivationPolicy: 0i64];
                let _: () = msg_send![app, activateIgnoringOtherApps: true];

                let width: CGFloat = 480.0;
                let height: CGFloat = 460.0;
                let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
                let style_mask = NS_WINDOW_STYLE_MASK_TITLED | NS_WINDOW_STYLE_MASK_CLOSABLE;

                let window: Id = msg_send![settings_window_class(), alloc];
                let window: Id = msg_send![
                    window,
                    initWithContentRect: frame
                    styleMask: style_mask
                    backing: NS_BACKING_STORE_BUFFERED
                    defer: false as BOOL
                ];

                let title_str = nsstring("Settings");
                let _: () = msg_send![window, setTitle: title_str];

                // Dark appearance
                let appearance: Id = msg_send![
                    class!(NSAppearance),
                    appearanceNamed: nsstring("NSAppearanceNameDarkAqua")
                ];
                let _: () = msg_send![window, setAppearance: appearance];

                let content_view: Id = msg_send![window, contentView];

                // Create target for callbacks
                let target: Id = msg_send![settings_target_class(), new];
                let target_obj = target as *mut Object;
                (*target_obj).set_ivar("rustState", state_ptr_send.into_ptr());

                // Set window delegate for close notification
                let _: () = msg_send![window, setDelegate: target];

                // Create tab view
                let tab_view_frame = NSRect::new(
                    NSPoint::new(20.0, 60.0),
                    NSSize::new(width - 40.0, height - 80.0),
                );
                let tab_view: Id = msg_send![class!(NSTabView), alloc];
                let tab_view: Id = msg_send![tab_view, initWithFrame: tab_view_frame];

                let settings = AppSettings::load();

                // Tab 1: Sleep Preventer
                let tab1: Id = msg_send![class!(NSTabViewItem), alloc];
                let tab1: Id = msg_send![tab1, initWithIdentifier: nsstring("sleep")];
                let _: () = msg_send![tab1, setLabel: nsstring("Sleep Preventer")];

                let tab1_view: Id = msg_send![class!(NSView), alloc];
                let tab1_view: Id = msg_send![
                    tab1_view,
                    initWithFrame: NSRect::new(
                        NSPoint::new(0.0, 0.0),
                        NSSize::new(width - 60.0, height - 140.0)
                    )
                ];

                let title_font: Id =
                    msg_send![class!(NSFont), boldSystemFontOfSize: 14.0 as CGFloat];
                let body_font: Id = msg_send![class!(NSFont), systemFontOfSize: 13.0 as CGFloat];
                let title_color = ns_color(0.95, 0.95, 0.95, 1.0);
                let body_color = ns_color(0.70, 0.70, 0.70, 1.0);

                // Sleep prevention toggle - centered vertically in the tab
                let toggle_label_frame =
                    NSRect::new(NSPoint::new(20.0, 190.0), NSSize::new(300.0, 20.0));
                let toggle_label = create_label(
                    "Enable Sleep Prevention",
                    toggle_label_frame,
                    title_font,
                    title_color,
                );
                let _: () = msg_send![tab1_view, addSubview: toggle_label];

                let toggle_desc_frame =
                    NSRect::new(NSPoint::new(20.0, 145.0), NSSize::new(380.0, 40.0));
                let toggle_desc = create_label(
                    "When enabled, prevents your Mac from sleeping while coding agents are actively working.",
                    toggle_desc_frame,
                    body_font,
                    body_color,
                );
                let _: () = msg_send![tab1_view, addSubview: toggle_desc];

                let checkbox_frame =
                    NSRect::new(NSPoint::new(20.0, 105.0), NSSize::new(200.0, 24.0));
                let checkbox: Id = msg_send![class!(NSButton), alloc];
                let checkbox: Id = msg_send![checkbox, initWithFrame: checkbox_frame];
                let _: () = msg_send![checkbox, setButtonType: 3i64]; // NSButtonTypeSwitch
                let _: () = msg_send![checkbox, setTitle: nsstring("Enabled")];
                let _: () = msg_send![
                    checkbox,
                    setState: if settings.sleep_prevention.enabled { 1i64 } else { 0i64 }
                ];
                let _: () = msg_send![checkbox, setTarget: target];
                let _: () = msg_send![checkbox, setAction: sel!(toggleChanged:)];
                let _: () = msg_send![tab1_view, addSubview: checkbox];

                // Agent notifications
                let notif_label_frame =
                    NSRect::new(NSPoint::new(20.0, 65.0), NSSize::new(300.0, 20.0));
                let notif_label = create_label(
                    "Agent Notifications",
                    notif_label_frame,
                    title_font,
                    title_color,
                );
                let _: () = msg_send![tab1_view, addSubview: notif_label];

                let notif_checkbox_frame =
                    NSRect::new(NSPoint::new(20.0, 35.0), NSSize::new(380.0, 24.0));
                let notif_checkbox: Id = msg_send![class!(NSButton), alloc];
                let notif_checkbox: Id =
                    msg_send![notif_checkbox, initWithFrame: notif_checkbox_frame];
                let _: () = msg_send![notif_checkbox, setButtonType: 3i64]; // NSButtonTypeSwitch
                let _: () = msg_send![
                    notif_checkbox,
                    setTitle: nsstring("Notify when an agent finishes or needs attention")
                ];
                let _: () = msg_send![
                    notif_checkbox,
                    setState: if settings.notifications.enabled { 1i64 } else { 0i64 }
                ];
                let _: () = msg_send![notif_checkbox, setTarget: target];
                let _: () = msg_send![notif_checkbox, setAction: sel!(notificationsToggleChanged:)];
                let _: () = msg_send![tab1_view, addSubview: notif_checkbox];

                let _: () = msg_send![tab1, setView: tab1_view];
                let _: () = msg_send![tab_view, addTabViewItem: tab1];

                // Tab 2: Dictation
                let tab2: Id = msg_send![class!(NSTabViewItem), alloc];
                let tab2: Id = msg_send![tab2, initWithIdentifier: nsstring("speech")];
                let _: () = msg_send![tab2, setLabel: nsstring("Dictation")];

                let tab2_view: Id = msg_send![class!(NSView), alloc];
                let tab2_view: Id = msg_send![
                    tab2_view,
                    initWithFrame: NSRect::new(
                        NSPoint::new(0.0, 0.0),
                        NSSize::new(width - 60.0, height - 140.0)
                    )
                ];

                // Hotkey selector - at top of tab
                let hotkey_label_frame =
                    NSRect::new(NSPoint::new(20.0, 292.0), NSSize::new(200.0, 20.0));
                let hotkey_label = create_label(
                    "Dictation Hotkey",
                    hotkey_label_frame,
                    title_font,
                    title_color,
                );
                let _: () = msg_send![tab2_view, addSubview: hotkey_label];

                let hotkey_popup_frame =
                    NSRect::new(NSPoint::new(20.0, 266.0), NSSize::new(200.0, 26.0));
                let hotkey_popup: Id = msg_send![class!(NSPopUpButton), alloc];
                let hotkey_popup: Id = msg_send![
                    hotkey_popup,
                    initWithFrame: hotkey_popup_frame pullsDown: false as BOOL
                ];

                let custom_hotkey = ResolvedHotkey::parse_custom(&settings.speech_to_text.hotkey);
                let hotkeys = AppSettings::supported_hotkeys();
                let mut selected_hotkey_index: i64 = 0;
                for (i, hotkey) in hotkeys.iter().enumerate() {
                    let _: () = msg_send![hotkey_popup, addItemWithTitle: nsstring(hotkey.label)];
                    if hotkey.id == settings.speech_to_text.hotkey {
                        selected_hotkey_index = i as i64;
                    }
                }
                // A recorded custom combo leaves the preset list deselected.
                if custom_hotkey.is_some() {
                    selected_hotkey_index = -1;
                }
                let _: () = msg_send![hotkey_popup, selectItemAtIndex: selected_hotkey_index];
                let _: () = msg_send![hotkey_popup, setTarget: target];
                let _: () = msg_send![hotkey_popup, setAction: sel!(hotkeyChanged:)];
                let _: () = msg_send![tab2_view, addSubview: hotkey_popup];

                // "Record…" button: capture any modifier+key combination
                let record_frame =
                    NSRect::new(NSPoint::new(228.0, 266.0), NSSize::new(112.0, 26.0));
                let record_button: Id = msg_send![class!(NSButton), alloc];
                let record_button: Id = msg_send![record_button, initWithFrame: record_frame];
                let _: () = msg_send![record_button, setBezelStyle: 1i64];
                let record_title = custom_hotkey
                    .as_ref()
                    .map(|hotkey| hotkey.label.as_str())
                    .unwrap_or("Record…");
                let _: () = msg_send![record_button, setTitle: nsstring(record_title)];
                let _: () = msg_send![record_button, setTarget: target];
                let _: () = msg_send![record_button, setAction: sel!(recordHotkeyPressed:)];
                let _: () = msg_send![tab2_view, addSubview: record_button];

                HOTKEY_CAPTURING.store(false, Ordering::SeqCst);
                HOTKEY_STATE.store(state_ptr_send.into_ptr(), Ordering::SeqCst);
                HOTKEY_RECORD_BUTTON.store(record_button as *mut c_void, Ordering::SeqCst);
                HOTKEY_POPUP.store(hotkey_popup as *mut c_void, Ordering::SeqCst);

                // Model selector
                let model_label_frame =
                    NSRect::new(NSPoint::new(20.0, 232.0), NSSize::new(200.0, 20.0));
                let model_label = create_label(
                    "Dictation Model",
                    model_label_frame,
                    title_font,
                    title_color,
                );
                let _: () = msg_send![tab2_view, addSubview: model_label];

                let model_popup_frame =
                    NSRect::new(NSPoint::new(20.0, 206.0), NSSize::new(260.0, 26.0));
                let model_popup: Id = msg_send![class!(NSPopUpButton), alloc];
                let model_popup: Id =
                    msg_send![model_popup, initWithFrame: model_popup_frame pullsDown: false as BOOL];

                let models = AppSettings::supported_models();
                let mut selected_model_index: i64 = 0;
                for (i, model) in models.iter().enumerate() {
                    let _: () = msg_send![model_popup, addItemWithTitle: nsstring(model.label)];
                    if model.id == settings.speech_to_text.model {
                        selected_model_index = i as i64;
                    }
                }
                let _: () = msg_send![model_popup, selectItemAtIndex: selected_model_index];
                let _: () = msg_send![model_popup, setTarget: target];
                let _: () = msg_send![model_popup, setAction: sel!(modelChanged:)];
                let _: () = msg_send![tab2_view, addSubview: model_popup];

                // Language selector
                let lang_label_frame =
                    NSRect::new(NSPoint::new(20.0, 168.0), NSSize::new(200.0, 20.0));
                let lang_label = create_label(
                    "Dictation Language",
                    lang_label_frame,
                    title_font,
                    title_color,
                );
                let _: () = msg_send![tab2_view, addSubview: lang_label];

                let popup_frame = NSRect::new(NSPoint::new(20.0, 142.0), NSSize::new(200.0, 26.0));
                let popup: Id = msg_send![class!(NSPopUpButton), alloc];
                let popup: Id =
                    msg_send![popup, initWithFrame: popup_frame pullsDown: false as BOOL];

                let languages = AppSettings::supported_languages();
                let mut selected_index: i64 = 0;
                for (i, (code, name)) in languages.iter().enumerate() {
                    let _: () = msg_send![popup, addItemWithTitle: nsstring(name)];
                    if *code == settings.speech_to_text.language {
                        selected_index = i as i64;
                    }
                }
                let _: () = msg_send![popup, selectItemAtIndex: selected_index];
                let _: () = msg_send![popup, setTarget: target];
                let _: () = msg_send![popup, setAction: sel!(languageChanged:)];
                let _: () = msg_send![tab2_view, addSubview: popup];

                // Dictation cue volume (right column, next to the language).
                // tab2_view is 420 wide, so this column must end by x=400.
                let sound_label_frame =
                    NSRect::new(NSPoint::new(240.0, 168.0), NSSize::new(100.0, 20.0));
                let sound_label = create_label(
                    "Sound Volume",
                    sound_label_frame,
                    title_font,
                    title_color,
                );
                let _: () = msg_send![tab2_view, addSubview: sound_label];

                let mute_frame = NSRect::new(NSPoint::new(342.0, 166.0), NSSize::new(58.0, 22.0));
                let mute_checkbox: Id = msg_send![class!(NSButton), alloc];
                let mute_checkbox: Id = msg_send![mute_checkbox, initWithFrame: mute_frame];
                let _: () = msg_send![mute_checkbox, setButtonType: 3i64]; // NSButtonTypeSwitch
                let _: () = msg_send![mute_checkbox, setTitle: nsstring("Mute")];
                let muted = settings.speech_to_text.sound_muted;
                let _: () = msg_send![mute_checkbox, setState: (muted as i64)];
                let _: () = msg_send![mute_checkbox, setTarget: target];
                let _: () = msg_send![mute_checkbox, setAction: sel!(soundMuteChanged:)];
                let _: () = msg_send![tab2_view, addSubview: mute_checkbox];

                let slider_frame =
                    NSRect::new(NSPoint::new(240.0, 142.0), NSSize::new(160.0, 24.0));
                let slider: Id = msg_send![class!(NSSlider), alloc];
                let slider: Id = msg_send![slider, initWithFrame: slider_frame];
                let _: () = msg_send![slider, setMinValue: 0.0f64];
                let _: () = msg_send![slider, setMaxValue: 100.0f64];
                let _: () = msg_send![
                    slider,
                    setDoubleValue: (settings.speech_to_text.sound_volume.clamp(0.0, 1.0) * 100.0) as f64
                ];
                let _: () = msg_send![slider, setEnabled: (!muted) as BOOL];
                let _: () = msg_send![slider, setTarget: target];
                let _: () = msg_send![slider, setAction: sel!(soundVolumeChanged:)];
                let _: () = msg_send![tab2_view, addSubview: slider];
                SOUND_VOLUME_SLIDER.store(slider as *mut c_void, Ordering::SeqCst);

                // Vocabulary words
                let vocab_label_frame =
                    NSRect::new(NSPoint::new(20.0, 110.0), NSSize::new(300.0, 20.0));
                let vocab_label = create_label(
                    "Vocabulary Words",
                    vocab_label_frame,
                    title_font,
                    title_color,
                );
                let _: () = msg_send![tab2_view, addSubview: vocab_label];

                let vocab_desc_frame =
                    NSRect::new(NSPoint::new(20.0, 90.0), NSSize::new(380.0, 18.0));
                let vocab_desc = create_label(
                    "One word per line. These help with transcription accuracy.",
                    vocab_desc_frame,
                    body_font,
                    body_color,
                );
                let _: () = msg_send![tab2_view, addSubview: vocab_desc];

                // Vocabulary text view in scroll view
                let scroll_frame =
                    NSRect::new(NSPoint::new(20.0, 8.0), NSSize::new(width - 100.0, 78.0));
                let scroll_view: Id = msg_send![class!(NSScrollView), alloc];
                let scroll_view: Id = msg_send![scroll_view, initWithFrame: scroll_frame];
                let _: () = msg_send![scroll_view, setBorderType: 3i64]; // NSBezelBorder
                let _: () = msg_send![scroll_view, setHasVerticalScroller: true as BOOL];

                let text_view_frame = NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(scroll_frame.size.width - 20.0, scroll_frame.size.height),
                );
                let text_view: Id = msg_send![class!(NSTextView), alloc];
                let text_view: Id = msg_send![text_view, initWithFrame: text_view_frame];
                let _: () =
                    msg_send![text_view, setMinSize: NSSize::new(0.0, scroll_frame.size.height)];
                let _: () = msg_send![text_view, setMaxSize: NSSize::new(f64::MAX as CGFloat, f64::MAX as CGFloat)];
                let _: () = msg_send![text_view, setVerticallyResizable: true as BOOL];
                let _: () = msg_send![text_view, setHorizontallyResizable: false as BOOL];
                let _: () = msg_send![text_view, setFont: body_font];

                // Set initial vocabulary text
                let vocab_text = settings.speech_to_text.vocabulary_words.join("\n");
                let _: () = msg_send![text_view, setString: nsstring(&vocab_text)];

                let _: () = msg_send![scroll_view, setDocumentView: text_view];
                let _: () = msg_send![tab2_view, addSubview: scroll_view];

                let _: () = msg_send![tab2, setView: tab2_view];
                let _: () = msg_send![tab_view, addTabViewItem: tab2];

                let _: () = msg_send![content_view, addSubview: tab_view];

                // Buttons
                let cancel_frame =
                    NSRect::new(NSPoint::new(width - 200.0, 15.0), NSSize::new(80.0, 32.0));
                let cancel_btn: Id = msg_send![class!(NSButton), alloc];
                let cancel_btn: Id = msg_send![cancel_btn, initWithFrame: cancel_frame];
                let _: () = msg_send![cancel_btn, setBezelStyle: 1i64];
                let _: () = msg_send![cancel_btn, setTitle: nsstring("Cancel")];
                let _: () = msg_send![cancel_btn, setTag: 0i64];
                let _: () = msg_send![cancel_btn, setTarget: target];
                let _: () = msg_send![cancel_btn, setAction: sel!(buttonPressed:)];
                let _: () = msg_send![content_view, addSubview: cancel_btn];

                let save_frame =
                    NSRect::new(NSPoint::new(width - 105.0, 15.0), NSSize::new(80.0, 32.0));
                let save_btn: Id = msg_send![class!(NSButton), alloc];
                let save_btn: Id = msg_send![save_btn, initWithFrame: save_frame];
                let _: () = msg_send![save_btn, setBezelStyle: 1i64];
                let _: () = msg_send![save_btn, setTitle: nsstring("Save")];
                let _: () = msg_send![save_btn, setTag: 1i64];
                let _: () = msg_send![save_btn, setKeyEquivalent: nsstring("\r")];
                let _: () = msg_send![save_btn, setTarget: target];
                let _: () = msg_send![save_btn, setAction: sel!(buttonPressed:)];
                let _: () = msg_send![content_view, addSubview: save_btn];

                // Store text view reference in target for later retrieval
                (*target_obj).set_ivar("vocabularyTextView", text_view as *mut c_void);

                let _: () = msg_send![window, center];
                let _: () = msg_send![window, makeKeyAndOrderFront: NIL];

                (
                    SendPtr(window as *mut c_void),
                    SendPtr(target as *mut c_void),
                    SendPtr(text_view as *mut c_void),
                    previous_policy,
                )
            },
        );

        Self {
            state,
            state_ptr,
            window,
            target,
            vocabulary_text_view,
            previous_policy,
        }
    }

    /// Run the modal window and return the resulting settings if saved
    pub fn run_modal(&self) -> Option<AppSettings> {
        let window = self.window;
        let vocabulary_text_view = self.vocabulary_text_view;
        let state_ptr = SendPtr(self.state_ptr as *mut c_void);

        run_on_main_thread(move || unsafe {
            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            let window = window.into_ptr() as Id;
            let _: i64 = msg_send![app, runModalForWindow: window];
        });

        // Get vocabulary from text view before checking action
        run_on_main_thread(move || unsafe {
            let text_view = vocabulary_text_view.into_ptr() as Id;
            let string: Id = msg_send![text_view, string];
            if let Some(text) = nsstring_to_string(string) {
                let words: Vec<String> = text
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                let state = &*(state_ptr.into_ptr() as *const SettingsState);
                state.update_vocabulary(words);
            }
        });

        let action = self.state.take_action();
        match action {
            Some(SettingsAction::Save) => Some(self.state.get_settings()),
            _ => None,
        }
    }

    pub fn close(&self) {
        let window = self.window;
        let target = self.target;
        let previous_policy = self.previous_policy;
        let state_ptr = SendPtr(self.state_ptr as *mut c_void);

        run_on_main_thread(move || unsafe {
            let window = window.into_ptr() as Id;
            let _: () = msg_send![window, orderOut: NIL];
            let _: () = msg_send![window, close];
            let _: () = msg_send![window, release];

            let target = target.into_ptr() as Id;
            let _: () = msg_send![target, release];

            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, setActivationPolicy: previous_policy];

            drop(Arc::from_raw(state_ptr.into_ptr() as *const SettingsState));
        });
    }
}

/// Show the settings window and save if user clicks Save
pub fn show_settings() -> Option<AppSettings> {
    let window = SettingsWindow::new();
    let mut result = window.run_modal();

    if let Some(ref mut settings) = result {
        // The window doesn't edit the force override; keep whatever the
        // popover control set while the window was open.
        settings.sleep_prevention.force = AppSettings::load().sleep_prevention.force;
        if let Err(e) = settings.save() {
            crate::logging::log(&format!("[settings] Failed to save settings: {}", e));
        }
    }

    window.close();
    result
}
