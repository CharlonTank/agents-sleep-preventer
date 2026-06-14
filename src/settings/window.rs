//! Settings window with tabbed interface

use dispatch::Queue;
use objc::declare::ClassDecl;
use objc::runtime::{Object, Sel, BOOL};
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use crate::objc_utils::{
    nsstring, nsstring_to_string, AutoreleasePool, CGFloat, Id, NSPoint, NSRect, NSSize, NIL,
    NS_BACKING_STORE_BUFFERED,
};

use super::AppSettings;

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

    fn update_language(&self, language: String) {
        let mut settings = self.settings.lock().unwrap();
        settings.speech_to_text.language = language;
    }

    fn update_model(&self, model: String) {
        let mut settings = self.settings.lock().unwrap();
        settings.speech_to_text.model = model;
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
                sel!(languageChanged:),
                language_changed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(modelChanged:),
                model_changed as extern "C" fn(&Object, Sel, Id),
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
                let height: CGFloat = 400.0;
                let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
                let style_mask = NS_WINDOW_STYLE_MASK_TITLED | NS_WINDOW_STYLE_MASK_CLOSABLE;

                let window: Id = msg_send![class!(NSWindow), alloc];
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
                    NSRect::new(NSPoint::new(20.0, 160.0), NSSize::new(300.0, 20.0));
                let toggle_label = create_label(
                    "Enable Sleep Prevention",
                    toggle_label_frame,
                    title_font,
                    title_color,
                );
                let _: () = msg_send![tab1_view, addSubview: toggle_label];

                let toggle_desc_frame =
                    NSRect::new(NSPoint::new(20.0, 115.0), NSSize::new(380.0, 40.0));
                let toggle_desc = create_label(
                    "When enabled, prevents your Mac from sleeping while coding agents are actively working.",
                    toggle_desc_frame,
                    body_font,
                    body_color,
                );
                let _: () = msg_send![tab1_view, addSubview: toggle_desc];

                let checkbox_frame =
                    NSRect::new(NSPoint::new(20.0, 75.0), NSSize::new(200.0, 24.0));
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

                // Model selector - at top of tab
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
    let result = window.run_modal();

    if let Some(ref settings) = result {
        if let Err(e) = settings.save() {
            crate::logging::log(&format!("[settings] Failed to save settings: {}", e));
        }
    }

    window.close();
    result
}
