//! Native macOS dialogs using Cocoa NSAlert
//! Replaces osascript "display dialog" calls

use dispatch::Queue;
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel, BOOL};
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use crate::objc_utils::{
    nsstring, AutoreleasePool, CGFloat, Id, NSPoint, NSRect, NSSize, NIL,
    NS_BACKING_STORE_BUFFERED, NS_WINDOW_STYLE_MASK_BORDERLESS,
};

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

fn run_on_main_async<F>(work: F)
where
    F: Send + 'static + FnOnce(),
{
    if is_main_thread() {
        work()
    } else {
        Queue::main().exec_async(work)
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

unsafe fn set_view_background(view: Id, color: Id, radius: CGFloat) {
    let _: () = msg_send![view, setWantsLayer: true as BOOL];
    let layer: Id = msg_send![view, layer];
    let cg_color: *mut c_void = msg_send![color, CGColor];
    let _: () = msg_send![layer, setBackgroundColor: cg_color];
    let _: () = msg_send![layer, setCornerRadius: radius];
    let _: () = msg_send![layer, setMasksToBounds: true as BOOL];
}

unsafe fn create_label(text: &str, frame: NSRect, font: Id, color: Id) -> Id {
    let label: Id = msg_send![class!(NSTextField), alloc];
    let label: Id = msg_send![label, initWithFrame: frame];
    let _: () = msg_send![label, setStringValue: nsstring(text)];
    let _: () = msg_send![label, setBezeled: false as BOOL];
    let _: () = msg_send![label, setDrawsBackground: false as BOOL];
    let _: () = msg_send![label, setEditable: false as BOOL];
    let _: () = msg_send![label, setSelectable: false as BOOL];
    let _: () = msg_send![label, setUsesSingleLineMode: false as BOOL];
    let _: () = msg_send![label, setLineBreakMode: 0i64];
    let _: () = msg_send![label, setFont: font];
    let _: () = msg_send![label, setTextColor: color];
    label
}

unsafe fn build_permission_row(
    content_view: Id,
    origin: NSPoint,
    size: NSSize,
    title: &str,
    description: &str,
    title_font: Id,
    desc_font: Id,
    title_color: Id,
    desc_color: Id,
    row_color: Id,
    button_title: &str,
    button_font: Id,
    target: Id,
    tag: i64,
) -> (Id, Id) {
    let row: Id = msg_send![class!(NSView), alloc];
    let row: Id = msg_send![row, initWithFrame: NSRect::new(origin, size)];
    set_view_background(row, row_color, 12.0);

    let button_width: CGFloat = 90.0;
    let button_height: CGFloat = 28.0;
    let button_x = size.width - button_width - 16.0;
    let button_y = (size.height - button_height) / 2.0;
    let label_width = size.width - button_width - 40.0;

    let title_frame = NSRect::new(
        NSPoint::new(16.0, size.height - 32.0),
        NSSize::new(label_width, 18.0),
    );
    let desc_frame = NSRect::new(NSPoint::new(16.0, 12.0), NSSize::new(label_width, 26.0));
    let title_label = create_label(title, title_frame, title_font, title_color);
    let desc_label = create_label(description, desc_frame, desc_font, desc_color);

    let button_frame = NSRect::new(
        NSPoint::new(button_x, button_y),
        NSSize::new(button_width, button_height),
    );
    let button: Id = msg_send![class!(NSButton), alloc];
    let button: Id = msg_send![button, initWithFrame: button_frame];
    let _: () = msg_send![button, setBezelStyle: 1i64];
    let _: () = msg_send![button, setTitle: nsstring(button_title)];
    let _: () = msg_send![button, setTag: tag];
    let _: () = msg_send![button, setFont: button_font];
    let _: () = msg_send![button, setTarget: target];
    let _: () = msg_send![button, setAction: sel!(togglePressed:)];
    style_permission_button(button, true);

    let _: () = msg_send![row, addSubview: title_label];
    let _: () = msg_send![row, addSubview: desc_label];
    let _: () = msg_send![row, addSubview: button];
    let _: () = msg_send![content_view, addSubview: row];

    (row, button)
}

unsafe fn style_permission_button(button: Id, enabled: bool) {
    let background = if enabled {
        ns_color(0.34, 0.34, 0.34, 1.0)
    } else {
        ns_color(0.26, 0.26, 0.26, 1.0)
    };
    let border = if enabled {
        ns_color(0.45, 0.45, 0.45, 1.0)
    } else {
        ns_color(0.30, 0.30, 0.30, 1.0)
    };

    let _: () = msg_send![button, setBordered: false as BOOL];
    let _: () = msg_send![button, setBezelStyle: 0i64];
    let _: () = msg_send![button, setWantsLayer: true as BOOL];
    let layer: Id = msg_send![button, layer];
    let bg_color: *mut c_void = msg_send![background, CGColor];
    let border_color: *mut c_void = msg_send![border, CGColor];
    let _: () = msg_send![layer, setBackgroundColor: bg_color];
    let _: () = msg_send![layer, setBorderColor: border_color];
    let _: () = msg_send![layer, setBorderWidth: 1.0];
    let _: () = msg_send![layer, setCornerRadius: 10.0];
}

/// Show an informational dialog with OK button
pub fn show_dialog(message: &str, title: &str) {
    run_on_main_thread(|| unsafe {
        let _pool = AutoreleasePool::new();

        // Activate the app to bring dialog to front
        // For LSUIElement apps, we need to set activation policy to Regular temporarily
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let previous_policy: i64 = msg_send![app, activationPolicy];
        // NSApplicationActivationPolicyRegular = 0
        let _: () = msg_send![app, setActivationPolicy: 0i64];
        let _: () = msg_send![app, activateIgnoringOtherApps: true];

        let alert: Id = msg_send![class!(NSAlert), new];

        // NSAlertStyleInformational = 1
        let _: () = msg_send![alert, setAlertStyle: 1i64];

        let title_str = nsstring(&title);
        let _: () = msg_send![alert, setMessageText: title_str];

        let message_str = nsstring(&message);
        let _: () = msg_send![alert, setInformativeText: message_str];

        let ok_str = nsstring("OK");
        let _: () = msg_send![alert, addButtonWithTitle: ok_str];

        let _: i64 = msg_send![alert, runModal];
        let _: () = msg_send![app, setActivationPolicy: previous_policy];
    });
}

/// Show a confirmation dialog with two buttons, returns true if confirmed
pub fn show_confirm_dialog(message: &str, title: &str, confirm: &str, cancel: &str) -> bool {
    run_on_main_thread(|| unsafe {
        let _pool = AutoreleasePool::new();

        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let previous_policy: i64 = msg_send![app, activationPolicy];
        // NSApplicationActivationPolicyRegular = 0
        let _: () = msg_send![app, setActivationPolicy: 0i64];
        let _: () = msg_send![app, activateIgnoringOtherApps: true];

        let alert: Id = msg_send![class!(NSAlert), new];

        // NSAlertStyleWarning = 0
        let _: () = msg_send![alert, setAlertStyle: 0i64];

        let title_str = nsstring(title);
        let _: () = msg_send![alert, setMessageText: title_str];

        let message_str = nsstring(message);
        let _: () = msg_send![alert, setInformativeText: message_str];

        // First button is default (confirm)
        let confirm_str = nsstring(confirm);
        let _: () = msg_send![alert, addButtonWithTitle: confirm_str];

        // Second button (cancel)
        let cancel_str = nsstring(cancel);
        let _: () = msg_send![alert, addButtonWithTitle: cancel_str];

        let response: i64 = msg_send![alert, runModal];

        // NSAlertFirstButtonReturn = 1000
        let confirmed = response == 1000;
        let _: () = msg_send![app, setActivationPolicy: previous_policy];
        confirmed
    })
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
pub enum SetupAction {
    Primary,
    Secondary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionToggle {
    Microphone,
    Accessibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionsAction {
    Primary,
    Secondary,
    Toggle(PermissionToggle),
}

struct DialogState {
    action: Mutex<Option<SetupAction>>,
}

impl DialogState {
    fn new() -> Self {
        Self {
            action: Mutex::new(None),
        }
    }

    fn clear(&self) {
        let mut action = self.action.lock().unwrap();
        *action = None;
    }

    fn set_action(&self, action_value: SetupAction) {
        let mut action = self.action.lock().unwrap();
        *action = Some(action_value);
    }

    fn take_action(&self) -> Option<SetupAction> {
        self.action.lock().unwrap().take()
    }
}

struct PermissionsState {
    action: Mutex<Option<PermissionsAction>>,
}

impl PermissionsState {
    fn new() -> Self {
        Self {
            action: Mutex::new(None),
        }
    }

    fn clear(&self) {
        let mut action = self.action.lock().unwrap();
        *action = None;
    }

    fn set_action(&self, action_value: PermissionsAction) {
        let mut action = self.action.lock().unwrap();
        *action = Some(action_value);
    }

    fn take_action(&self) -> Option<PermissionsAction> {
        self.action.lock().unwrap().take()
    }
}

extern "C" fn setup_button_pressed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const DialogState);
            let tag: i64 = msg_send![sender, tag];
            let action = if tag == 1 {
                SetupAction::Primary
            } else {
                SetupAction::Secondary
            };
            state.set_action(action);
        }

        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, stopModal];
    }
}

extern "C" fn permissions_button_pressed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const PermissionsState);
            let tag: i64 = msg_send![sender, tag];
            let action = if tag == 1 {
                PermissionsAction::Primary
            } else {
                PermissionsAction::Secondary
            };
            state.set_action(action);
        }

        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, stopModal];
    }
}

extern "C" fn permissions_toggle_pressed(this: &Object, _: Sel, sender: Id) {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar("rustState");
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const PermissionsState);
            let tag: i64 = msg_send![sender, tag];
            let toggle = match tag {
                1 => PermissionToggle::Microphone,
                2 => PermissionToggle::Accessibility,
                _ => return,
            };
            state.set_action(PermissionsAction::Toggle(toggle));
        }

        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, stopModal];
    }
}

struct ClassPtr(*const Class);

unsafe impl Send for ClassPtr {}
unsafe impl Sync for ClassPtr {}

struct WindowClassPtr(*const Class);

unsafe impl Send for WindowClassPtr {}
unsafe impl Sync for WindowClassPtr {}

extern "C" fn borderless_can_become_key(_this: &Object, _: Sel) -> BOOL {
    true as BOOL
}

extern "C" fn borderless_can_become_main(_this: &Object, _: Sel) -> BOOL {
    true as BOOL
}

fn borderless_window_class() -> &'static Class {
    static CLASS: OnceLock<WindowClassPtr> = OnceLock::new();
    let class_ptr = CLASS.get_or_init(|| {
        let superclass = class!(NSWindow);
        let mut decl = ClassDecl::new("ASPBorderlessWindow", superclass)
            .expect("Failed to create ASPBorderlessWindow class");
        unsafe {
            decl.add_method(
                sel!(canBecomeKeyWindow),
                borderless_can_become_key as extern "C" fn(&Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(canBecomeMainWindow),
                borderless_can_become_main as extern "C" fn(&Object, Sel) -> BOOL,
            );
        }
        WindowClassPtr(decl.register() as *const Class)
    });

    unsafe { &*class_ptr.0 }
}

fn setup_target_class() -> &'static Class {
    static CLASS: OnceLock<ClassPtr> = OnceLock::new();
    let class_ptr = CLASS.get_or_init(|| {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("ASPSetupTarget", superclass)
            .expect("Failed to create ASPSetupTarget class");
        decl.add_ivar::<*mut c_void>("rustState");
        unsafe {
            decl.add_method(
                sel!(buttonPressed:),
                setup_button_pressed as extern "C" fn(&Object, Sel, Id),
            );
        }
        ClassPtr(decl.register() as *const Class)
    });

    unsafe { &*class_ptr.0 }
}

fn permissions_target_class() -> &'static Class {
    static CLASS: OnceLock<ClassPtr> = OnceLock::new();
    let class_ptr = CLASS.get_or_init(|| {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("ASPPermissionsTarget", superclass)
            .expect("Failed to create ASPPermissionsTarget class");
        decl.add_ivar::<*mut c_void>("rustState");
        unsafe {
            decl.add_method(
                sel!(buttonPressed:),
                permissions_button_pressed as extern "C" fn(&Object, Sel, Id),
            );
            decl.add_method(
                sel!(togglePressed:),
                permissions_toggle_pressed as extern "C" fn(&Object, Sel, Id),
            );
        }
        ClassPtr(decl.register() as *const Class)
    });

    unsafe { &*class_ptr.0 }
}

#[derive(Clone, Copy)]
pub struct SetupWindowHandle {
    window: SendPtr,
    title_label: SendPtr,
    message: SendPtr,
    progress: SendPtr,
    primary_button: SendPtr,
    secondary_button: SendPtr,
}

impl SetupWindowHandle {
    pub fn set_message(&self, message: &str) {
        let message = message.to_string();
        let label = self.message;
        run_on_main_async(move || unsafe {
            let message_str = nsstring(&message);
            let label = label.into_ptr() as Id;
            let _: () = msg_send![label, setStringValue: message_str];
        });
    }

    pub fn set_title(&self, title: &str) {
        let title = title.to_string();
        let window = self.window;
        let title_label = self.title_label;
        run_on_main_async(move || unsafe {
            let title_str = nsstring(&title);
            let window = window.into_ptr() as Id;
            let _: () = msg_send![window, setTitle: title_str];
            let title_label = title_label.into_ptr() as Id;
            let _: () = msg_send![title_label, setStringValue: title_str];
        });
    }

    pub fn set_primary_button(&self, title: &str) {
        let title = title.to_string();
        let button = self.primary_button;
        run_on_main_async(move || unsafe {
            let title_str = nsstring(&title);
            let button = button.into_ptr() as Id;
            let _: () = msg_send![button, setTitle: title_str];
        });
    }

    pub fn set_secondary_button(&self, title: &str) {
        let title = title.to_string();
        let button = self.secondary_button;
        run_on_main_async(move || unsafe {
            let title_str = nsstring(&title);
            let button = button.into_ptr() as Id;
            let _: () = msg_send![button, setTitle: title_str];
        });
    }

    pub fn set_primary_enabled(&self, enabled: bool) {
        let button = self.primary_button;
        run_on_main_async(move || unsafe {
            let button = button.into_ptr() as Id;
            let _: () = msg_send![button, setEnabled: enabled as BOOL];
        });
    }

    pub fn set_secondary_visible(&self, visible: bool) {
        let button = self.secondary_button;
        run_on_main_async(move || unsafe {
            let button = button.into_ptr() as Id;
            let _: () = msg_send![button, setHidden: (!visible) as BOOL];
        });
    }

    pub fn show_progress(&self, show: bool) {
        let progress = self.progress;
        run_on_main_async(move || unsafe {
            let progress = progress.into_ptr() as Id;
            let _: () = msg_send![progress, setHidden: (!show) as BOOL];
        });
    }

    pub fn set_progress(&self, percent: f64) {
        let progress = self.progress;
        let value = percent.clamp(0.0, 100.0);
        run_on_main_async(move || unsafe {
            let progress = progress.into_ptr() as Id;
            let _: () = msg_send![progress, setDoubleValue: value];
        });
    }

    pub fn stop_modal(&self) {
        run_on_main_async(|| unsafe {
            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, stopModal];
        });
    }
}

pub struct SetupWindow {
    handle: SetupWindowHandle,
    state: Arc<DialogState>,
    state_ptr: *const DialogState,
    target: SendPtr,
    previous_policy: i64,
}

impl SetupWindow {
    pub fn new(title: &str, message: &str) -> Self {
        let title = title.to_string();
        let message = message.to_string();
        let state = Arc::new(DialogState::new());
        let state_ptr = Arc::into_raw(state.clone());
        let state_ptr_send = SendPtr(state_ptr as *mut c_void);

        let (handle, target, previous_policy) = run_on_main_thread(move || unsafe {
            let _pool = AutoreleasePool::new();

            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            let previous_policy: i64 = msg_send![app, activationPolicy];
            let _: () = msg_send![app, setActivationPolicy: 0i64];
            let _: () = msg_send![app, activateIgnoringOtherApps: true];

            let width: CGFloat = 560.0;
            let height: CGFloat = 460.0;
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
            let window: Id = msg_send![borderless_window_class(), alloc];
            let window: Id = msg_send![
                window,
                initWithContentRect: frame
                styleMask: NS_WINDOW_STYLE_MASK_BORDERLESS
                backing: NS_BACKING_STORE_BUFFERED
                defer: false as BOOL
            ];

            let title_str = nsstring(&title);
            let _: () = msg_send![window, setTitle: title_str];
            let _: () = msg_send![window, setOpaque: false as BOOL];
            let _: () = msg_send![window, setHasShadow: true as BOOL];
            let _: () = msg_send![window, setMovableByWindowBackground: true as BOOL];

            let background = ns_color(0.18, 0.18, 0.18, 1.0);
            let _: () = msg_send![window, setBackgroundColor: background];

            let appearance: Id = msg_send![class!(NSAppearance), appearanceNamed: nsstring("NSAppearanceNameDarkAqua")];
            let _: () = msg_send![window, setAppearance: appearance];

            let content_view: Id = msg_send![window, contentView];
            set_view_background(content_view, background, 16.0);

            let title_font: Id = msg_send![class!(NSFont), boldSystemFontOfSize: 22.0 as CGFloat];
            let body_font: Id = msg_send![class!(NSFont), systemFontOfSize: 13.0 as CGFloat];
            let title_color = ns_color(0.95, 0.95, 0.95, 1.0);
            let body_color = ns_color(0.70, 0.70, 0.70, 1.0);

            let progress_frame = NSRect::new(
                NSPoint::new(24.0, height - 18.0),
                NSSize::new(width - 48.0, 6.0),
            );
            let progress: Id = msg_send![class!(NSProgressIndicator), alloc];
            let progress: Id = msg_send![progress, initWithFrame: progress_frame];
            let _: () = msg_send![progress, setIndeterminate: false as BOOL];
            let _: () = msg_send![progress, setMinValue: 0.0];
            let _: () = msg_send![progress, setMaxValue: 100.0];
            let _: () = msg_send![progress, setDoubleValue: 0.0];
            let _: () = msg_send![progress, setStyle: 0i64];
            let _: () = msg_send![progress, setHidden: true as BOOL];

            let title_frame = NSRect::new(
                NSPoint::new(24.0, height - 64.0),
                NSSize::new(width - 48.0, 28.0),
            );
            let title_label = create_label(&title, title_frame, title_font, title_color);

            let message_frame =
                NSRect::new(NSPoint::new(24.0, 220.0), NSSize::new(width - 48.0, 150.0));
            let label = create_label(&message, message_frame, body_font, body_color);

            let secondary_frame =
                NSRect::new(NSPoint::new(24.0, 76.0), NSSize::new(width - 48.0, 36.0));
            let secondary: Id = msg_send![class!(NSButton), alloc];
            let secondary: Id = msg_send![secondary, initWithFrame: secondary_frame];
            let _: () = msg_send![secondary, setBezelStyle: 1i64];
            let _: () = msg_send![secondary, setTitle: nsstring("Annuler")];
            let _: () = msg_send![secondary, setTag: 0i64];
            let secondary_font: Id = msg_send![class!(NSFont), systemFontOfSize: 13.0 as CGFloat];
            let _: () = msg_send![secondary, setFont: secondary_font];

            let primary_frame =
                NSRect::new(NSPoint::new(24.0, 24.0), NSSize::new(width - 48.0, 44.0));
            let primary: Id = msg_send![class!(NSButton), alloc];
            let primary: Id = msg_send![primary, initWithFrame: primary_frame];
            let _: () = msg_send![primary, setBezelStyle: 1i64];
            let _: () = msg_send![primary, setTitle: nsstring("OK")];
            let _: () = msg_send![primary, setTag: 1i64];
            let _: () = msg_send![primary, setKeyEquivalent: nsstring("\r")];
            let primary_font: Id = msg_send![class!(NSFont), boldSystemFontOfSize: 14.0 as CGFloat];
            let _: () = msg_send![primary, setFont: primary_font];

            let target: Id = msg_send![setup_target_class(), new];
            let target_obj = target as *mut Object;
            (*target_obj).set_ivar("rustState", state_ptr_send.into_ptr());

            let _: () = msg_send![primary, setTarget: target];
            let _: () = msg_send![primary, setAction: sel!(buttonPressed:)];
            let _: () = msg_send![secondary, setTarget: target];
            let _: () = msg_send![secondary, setAction: sel!(buttonPressed:)];
            let _: () = msg_send![content_view, addSubview: title_label];
            let _: () = msg_send![content_view, addSubview: label];
            let _: () = msg_send![content_view, addSubview: progress];
            let _: () = msg_send![content_view, addSubview: primary];
            let _: () = msg_send![content_view, addSubview: secondary];

            let _: () = msg_send![window, center];
            let _: () = msg_send![window, makeKeyAndOrderFront: NIL];

            (
                SetupWindowHandle {
                    window: SendPtr(window as *mut c_void),
                    title_label: SendPtr(title_label as *mut c_void),
                    message: SendPtr(label as *mut c_void),
                    progress: SendPtr(progress as *mut c_void),
                    primary_button: SendPtr(primary as *mut c_void),
                    secondary_button: SendPtr(secondary as *mut c_void),
                },
                SendPtr(target as *mut c_void),
                previous_policy,
            )
        });

        Self {
            handle,
            state,
            state_ptr,
            target,
            previous_policy,
        }
    }

    pub fn handle(&self) -> SetupWindowHandle {
        self.handle
    }

    pub fn set_title(&self, title: &str) {
        self.handle.set_title(title);
    }

    pub fn set_message(&self, message: &str) {
        self.handle.set_message(message);
    }

    pub fn set_primary_button(&self, title: &str) {
        self.handle.set_primary_button(title);
    }

    pub fn set_secondary_button(&self, title: &str) {
        self.handle.set_secondary_button(title);
    }

    pub fn set_primary_enabled(&self, enabled: bool) {
        self.handle.set_primary_enabled(enabled);
    }

    pub fn set_secondary_visible(&self, visible: bool) {
        self.handle.set_secondary_visible(visible);
    }

    pub fn show_progress(&self, show: bool) {
        self.handle.show_progress(show);
    }

    pub fn set_progress(&self, percent: f64) {
        self.handle.set_progress(percent);
    }

    pub fn run_modal(&self) {
        let window = self.handle.window;
        run_on_main_thread(move || unsafe {
            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            let window = window.into_ptr() as Id;
            let _: i64 = msg_send![app, runModalForWindow: window];
        });
    }

    pub fn wait_for_action(&self) -> SetupAction {
        self.state.clear();
        self.run_modal();
        self.state.take_action().unwrap_or(SetupAction::Secondary)
    }

    pub fn close(&self) {
        let window = self.handle.window;
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

            drop(Arc::from_raw(state_ptr.into_ptr() as *const DialogState));
        });
    }
}

#[derive(Clone, Copy)]
pub struct PermissionsWindowHandle {
    window: SendPtr,
    progress: SendPtr,
    mic_row: SendPtr,
    accessibility_row: SendPtr,
    mic_toggle: SendPtr,
    accessibility_toggle: SendPtr,
    primary_button: SendPtr,
    secondary_button: SendPtr,
}

impl PermissionsWindowHandle {
    pub fn set_primary_button(&self, title: &str) {
        let title = title.to_string();
        let button = self.primary_button;
        run_on_main_async(move || unsafe {
            let title_str = nsstring(&title);
            let button = button.into_ptr() as Id;
            let _: () = msg_send![button, setTitle: title_str];
        });
    }

    pub fn set_secondary_button(&self, title: &str) {
        let title = title.to_string();
        let button = self.secondary_button;
        run_on_main_async(move || unsafe {
            let title_str = nsstring(&title);
            let button = button.into_ptr() as Id;
            let _: () = msg_send![button, setTitle: title_str];
        });
    }

    pub fn set_secondary_visible(&self, visible: bool) {
        let button = self.secondary_button;
        run_on_main_async(move || unsafe {
            let button = button.into_ptr() as Id;
            let _: () = msg_send![button, setHidden: (!visible) as BOOL];
        });
    }

    pub fn set_progress(&self, percent: f64) {
        let progress = self.progress;
        let value = percent.clamp(0.0, 100.0);
        run_on_main_async(move || unsafe {
            let progress = progress.into_ptr() as Id;
            let _: () = msg_send![progress, setDoubleValue: value];
        });
    }

    pub fn set_toggle(&self, toggle: PermissionToggle, label: &str, checked: bool) {
        let label = label.to_string();
        let (button, row) = match toggle {
            PermissionToggle::Microphone => (self.mic_toggle, self.mic_row),
            PermissionToggle::Accessibility => (self.accessibility_toggle, self.accessibility_row),
        };
        run_on_main_async(move || unsafe {
            let label_str = nsstring(&label);
            let button = button.into_ptr() as Id;
            let _: () = msg_send![button, setTitle: label_str];
            let enabled = !checked;
            let _: () = msg_send![button, setEnabled: enabled as BOOL];
            style_permission_button(button, enabled);
            let row = row.into_ptr() as Id;
            let _: () = msg_send![row, setAlphaValue: if checked { 0.6 } else { 1.0 }];
        });
    }
}

pub struct PermissionsWindow {
    handle: PermissionsWindowHandle,
    state: Arc<PermissionsState>,
    state_ptr: *const PermissionsState,
    target: SendPtr,
    previous_policy: i64,
}

impl PermissionsWindow {
    pub fn new(title: &str, message: &str) -> Self {
        let title = title.to_string();
        let message = message.to_string();
        let state = Arc::new(PermissionsState::new());
        let state_ptr = Arc::into_raw(state.clone());
        let state_ptr_send = SendPtr(state_ptr as *mut c_void);

        let (handle, target, previous_policy) = run_on_main_thread(move || unsafe {
            let _pool = AutoreleasePool::new();

            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            let previous_policy: i64 = msg_send![app, activationPolicy];
            let _: () = msg_send![app, setActivationPolicy: 0i64];
            let _: () = msg_send![app, activateIgnoringOtherApps: true];

            let width: CGFloat = 560.0;
            let height: CGFloat = 476.0;
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
            let window: Id = msg_send![borderless_window_class(), alloc];
            let window: Id = msg_send![
                window,
                initWithContentRect: frame
                styleMask: NS_WINDOW_STYLE_MASK_BORDERLESS
                backing: NS_BACKING_STORE_BUFFERED
                defer: false as BOOL
            ];

            let title_str = nsstring(&title);
            let _: () = msg_send![window, setTitle: title_str];
            let _: () = msg_send![window, setOpaque: false as BOOL];
            let _: () = msg_send![window, setHasShadow: true as BOOL];
            let _: () = msg_send![window, setMovableByWindowBackground: true as BOOL];

            let background = ns_color(0.18, 0.18, 0.18, 1.0);
            let _: () = msg_send![window, setBackgroundColor: background];

            let appearance: Id = msg_send![class!(NSAppearance), appearanceNamed: nsstring("NSAppearanceNameDarkAqua")];
            let _: () = msg_send![window, setAppearance: appearance];

            let content_view: Id = msg_send![window, contentView];
            set_view_background(content_view, background, 16.0);

            let target: Id = msg_send![permissions_target_class(), new];
            let target_obj = target as *mut Object;
            (*target_obj).set_ivar("rustState", state_ptr_send.into_ptr());

            let title_font: Id = msg_send![class!(NSFont), boldSystemFontOfSize: 22.0 as CGFloat];
            let subtitle_font: Id = msg_send![class!(NSFont), systemFontOfSize: 13.0 as CGFloat];
            let row_title_font: Id =
                msg_send![class!(NSFont), boldSystemFontOfSize: 14.0 as CGFloat];
            let row_desc_font: Id = msg_send![class!(NSFont), systemFontOfSize: 12.0 as CGFloat];
            let button_font: Id = msg_send![class!(NSFont), systemFontOfSize: 12.0 as CGFloat];

            let title_color = ns_color(0.95, 0.95, 0.95, 1.0);
            let subtitle_color = ns_color(0.70, 0.70, 0.70, 1.0);
            let desc_color = ns_color(0.60, 0.60, 0.60, 1.0);
            let row_color = ns_color(0.23, 0.23, 0.23, 1.0);

            let progress_frame = NSRect::new(
                NSPoint::new(24.0, height - 18.0),
                NSSize::new(width - 48.0, 6.0),
            );
            let progress: Id = msg_send![class!(NSProgressIndicator), alloc];
            let progress: Id = msg_send![progress, initWithFrame: progress_frame];
            let _: () = msg_send![progress, setIndeterminate: false as BOOL];
            let _: () = msg_send![progress, setMinValue: 0.0];
            let _: () = msg_send![progress, setMaxValue: 100.0];
            let _: () = msg_send![progress, setDoubleValue: 0.0];
            let _: () = msg_send![progress, setStyle: 0i64];

            let title_frame = NSRect::new(
                NSPoint::new(24.0, height - 64.0),
                NSSize::new(width - 48.0, 28.0),
            );
            let title_label = create_label(&title, title_frame, title_font, title_color);

            let subtitle_frame = NSRect::new(
                NSPoint::new(24.0, height - 110.0),
                NSSize::new(width - 48.0, 40.0),
            );
            let subtitle_label =
                create_label(&message, subtitle_frame, subtitle_font, subtitle_color);

            let row_width = width - 48.0;
            let row_height: CGFloat = 72.0;
            let row_spacing: CGFloat = 12.0;
            let row2_y: CGFloat = 150.0;
            let row1_y = row2_y + row_height + row_spacing;

            let (mic_row, mic_toggle) = build_permission_row(
                content_view,
                NSPoint::new(24.0, row1_y),
                NSSize::new(row_width, row_height),
                "Allow Microphone Access",
                "Required to capture audio during dictation.",
                row_title_font,
                row_desc_font,
                title_color,
                desc_color,
                row_color,
                "Allow",
                button_font,
                target,
                1,
            );

            let (accessibility_row, accessibility_toggle) = build_permission_row(
                content_view,
                NSPoint::new(24.0, row2_y),
                NSSize::new(row_width, row_height),
                "Allow Accessibility",
                "Required to detect the dictation shortcut and insert transcribed text.",
                row_title_font,
                row_desc_font,
                title_color,
                desc_color,
                row_color,
                "Allow",
                button_font,
                target,
                2,
            );

            let secondary_frame =
                NSRect::new(NSPoint::new(24.0, 76.0), NSSize::new(width - 48.0, 36.0));
            let secondary: Id = msg_send![class!(NSButton), alloc];
            let secondary: Id = msg_send![secondary, initWithFrame: secondary_frame];
            let _: () = msg_send![secondary, setBezelStyle: 1i64];
            let _: () = msg_send![secondary, setTitle: nsstring("Later")];
            let _: () = msg_send![secondary, setTag: 0i64];
            let secondary_font: Id = msg_send![class!(NSFont), systemFontOfSize: 13.0 as CGFloat];
            let _: () = msg_send![secondary, setFont: secondary_font];

            let primary_frame =
                NSRect::new(NSPoint::new(24.0, 24.0), NSSize::new(width - 48.0, 44.0));
            let primary: Id = msg_send![class!(NSButton), alloc];
            let primary: Id = msg_send![primary, initWithFrame: primary_frame];
            let _: () = msg_send![primary, setBezelStyle: 1i64];
            let _: () = msg_send![primary, setTitle: nsstring("Continuer")];
            let _: () = msg_send![primary, setTag: 1i64];
            let _: () = msg_send![primary, setKeyEquivalent: nsstring("\r")];
            let primary_font: Id = msg_send![class!(NSFont), boldSystemFontOfSize: 14.0 as CGFloat];
            let _: () = msg_send![primary, setFont: primary_font];

            let _: () = msg_send![primary, setTarget: target];
            let _: () = msg_send![primary, setAction: sel!(buttonPressed:)];
            let _: () = msg_send![secondary, setTarget: target];
            let _: () = msg_send![secondary, setAction: sel!(buttonPressed:)];

            let _: () = msg_send![content_view, addSubview: progress];
            let _: () = msg_send![content_view, addSubview: title_label];
            let _: () = msg_send![content_view, addSubview: subtitle_label];
            let _: () = msg_send![content_view, addSubview: primary];
            let _: () = msg_send![content_view, addSubview: secondary];

            let _: () = msg_send![window, center];
            let _: () = msg_send![window, makeKeyAndOrderFront: NIL];

            (
                PermissionsWindowHandle {
                    window: SendPtr(window as *mut c_void),
                    progress: SendPtr(progress as *mut c_void),
                    mic_row: SendPtr(mic_row as *mut c_void),
                    accessibility_row: SendPtr(accessibility_row as *mut c_void),
                    mic_toggle: SendPtr(mic_toggle as *mut c_void),
                    accessibility_toggle: SendPtr(accessibility_toggle as *mut c_void),
                    primary_button: SendPtr(primary as *mut c_void),
                    secondary_button: SendPtr(secondary as *mut c_void),
                },
                SendPtr(target as *mut c_void),
                previous_policy,
            )
        });

        Self {
            handle,
            state,
            state_ptr,
            target,
            previous_policy,
        }
    }

    pub fn set_primary_button(&self, title: &str) {
        self.handle.set_primary_button(title);
    }

    pub fn set_secondary_button(&self, title: &str) {
        self.handle.set_secondary_button(title);
    }

    pub fn set_secondary_visible(&self, visible: bool) {
        self.handle.set_secondary_visible(visible);
    }

    pub fn set_progress(&self, percent: f64) {
        self.handle.set_progress(percent);
    }

    pub fn set_toggle(&self, toggle: PermissionToggle, label: &str, checked: bool) {
        self.handle.set_toggle(toggle, label, checked);
    }

    pub fn handle(&self) -> PermissionsWindowHandle {
        self.handle
    }

    pub fn run_modal(&self) {
        let window = self.handle.window;
        run_on_main_thread(move || unsafe {
            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            let window = window.into_ptr() as Id;
            let _: i64 = msg_send![app, runModalForWindow: window];
        });
    }

    pub fn wait_for_action(&self) -> PermissionsAction {
        self.state.clear();
        self.run_modal();
        self.state
            .take_action()
            .unwrap_or(PermissionsAction::Secondary)
    }

    pub fn close(&self) {
        let window = self.handle.window;
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

            drop(Arc::from_raw(
                state_ptr.into_ptr() as *const PermissionsState
            ));
        });
    }
}
