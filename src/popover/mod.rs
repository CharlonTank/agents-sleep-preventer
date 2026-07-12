mod ui;

use objc::runtime::{NO, YES};
use objc::{class, msg_send, sel, sel_impl};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::objc_utils::{
    CGFloat, Id, NSPoint, NSRect, NSSize, NIL, NS_BACKING_STORE_BUFFERED,
    NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES, NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE,
    NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY, NS_WINDOW_STYLE_MASK_BORDERLESS,
};

static POPOVER_VISIBLE: AtomicBool = AtomicBool::new(false);

const POPOVER_WIDTH: CGFloat = 280.0;
const POPOVER_HEIGHT: CGFloat = 400.0;

pub struct PopoverState {
    pub manual_enabled: bool,
    pub instances: Vec<(u32, u64, f32, String)>,
    pub inactive: Vec<u32>,
    pub thermal_warning: bool,
    pub dictation_enabled: bool,
    pub dictation_available: bool,
    pub sleep_disabled: bool,
}

pub struct PopoverWindow {
    window: Option<Id>,
}

impl PopoverWindow {
    pub fn new() -> Self {
        Self { window: None }
    }

    pub fn is_visible(&self) -> bool {
        POPOVER_VISIBLE.load(Ordering::SeqCst)
    }

    pub fn show(&mut self, icon_rect: (f64, f64, f64, f64), state: &PopoverState) {
        crate::logging::log(&format!(
            "[popover] show() called, icon_rect={:?}",
            icon_rect
        ));
        if self.window.is_some() {
            crate::logging::log("[popover] window already exists, returning");
            return;
        }

        let (icon_x, icon_y, icon_w, _icon_h) = icon_rect;

        let icon_center_x = icon_x + (icon_w / 2.0);
        let popover_x = icon_center_x - (POPOVER_WIDTH as f64 / 2.0);
        let popover_y = icon_y - POPOVER_HEIGHT as f64 - 5.0;

        unsafe {
            let frame = NSRect::new(
                NSPoint::new(popover_x as CGFloat, popover_y as CGFloat),
                NSSize::new(POPOVER_WIDTH, POPOVER_HEIGHT),
            );

            let window: Id = msg_send![class!(NSWindow), alloc];
            let window: Id = msg_send![
                window,
                initWithContentRect: frame
                styleMask: NS_WINDOW_STYLE_MASK_BORDERLESS
                backing: NS_BACKING_STORE_BUFFERED
                defer: NO
            ];

            if window.is_null() {
                eprintln!("[popover] ERROR: Failed to create NSWindow");
                return;
            }

            let _: () = msg_send![window, setLevel: 25i64];
            let _: () = msg_send![window, setOpaque: NO];
            let _: () = msg_send![window, setHasShadow: YES];

            let behavior = NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
                | NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY
                | NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE;
            let _: () = msg_send![window, setCollectionBehavior: behavior];

            let _: () = msg_send![window, setHidesOnDeactivate: NO];

            // Background color
            let bg_color: Id = msg_send![
                class!(NSColor),
                colorWithRed: 0.95 as CGFloat
                green: 0.95 as CGFloat
                blue: 0.95 as CGFloat
                alpha: 0.98 as CGFloat
            ];
            let _: () = msg_send![window, setBackgroundColor: bg_color];

            // Get content view and build UI
            let content_view: Id = msg_send![window, contentView];

            // Enable layer for rounded corners
            let _: () = msg_send![content_view, setWantsLayer: YES];
            let layer: Id = msg_send![content_view, layer];
            let _: () = msg_send![layer, setCornerRadius: 12.0 as CGFloat];

            // Build UI components
            self.build_ui(content_view, state);

            let _: () = msg_send![window, makeKeyAndOrderFront: NIL];

            self.window = Some(window);
            POPOVER_VISIBLE.store(true, Ordering::SeqCst);
            crate::logging::log("[popover] Window created and visible");
        }
    }

    unsafe fn build_ui(&self, content_view: Id, state: &PopoverState) {
        let mut y = POPOVER_HEIGHT - 40.0;

        // Title
        let title = ui::create_label("Agents Sleep Preventer", 20.0, y, 240.0, 24.0, true);
        let _: () = msg_send![content_view, addSubview: title];
        y -= 40.0;

        // Toggle switch
        let toggle_text = if state.manual_enabled {
            if state.instances.is_empty() {
                "Sleep Prevention (Idle)"
            } else {
                "Sleep Prevention (Active)"
            }
        } else {
            "Sleep Prevention (Off)"
        };
        let toggle =
            ui::create_toggle_switch(state.manual_enabled, toggle_text, 20.0, y, 240.0, 26.0);
        let _: () = msg_send![content_view, addSubview: toggle];
        y -= 35.0;

        // Separator
        let separator = ui::create_separator(20.0, y, 240.0);
        let _: () = msg_send![content_view, addSubview: separator];
        y -= 15.0;

        // Active instances header
        let header_text = if state.instances.is_empty() {
            "No Active Instances"
        } else {
            "Active Instances"
        };
        let header = ui::create_label(header_text, 20.0, y, 240.0, 18.0, false);
        let _: () = msg_send![content_view, addSubview: header];
        y -= 22.0;

        // Active instances list
        for (pid, age, cpu, location) in &state.instances {
            let text = format!("  {} [{}] - {}s - {:.1}%", location, pid, age, cpu);
            let label = ui::create_label(&text, 20.0, y, 240.0, 18.0, false);
            let _: () = msg_send![content_view, addSubview: label];
            y -= 20.0;

            // Limit display
            if y < 100.0 {
                break;
            }
        }

        y -= 10.0;

        // Inactive instances header
        if !state.inactive.is_empty() {
            let inactive_header =
                ui::create_label("Inactive Instances", 20.0, y, 240.0, 18.0, false);
            let _: () = msg_send![content_view, addSubview: inactive_header];
            y -= 22.0;

            let inactive_text = format!("  {} inactive", state.inactive.len());
            let inactive_label = ui::create_label(&inactive_text, 20.0, y, 240.0, 18.0, false);
            let _: () = msg_send![content_view, addSubview: inactive_label];
            y -= 30.0;
        }

        // Dictation status
        let dictation_text = if !state.dictation_available {
            "🎤 Dictation: Unavailable"
        } else if state.dictation_enabled {
            "🎤 Dictation: On"
        } else {
            "🎤 Dictation: Off"
        };
        let dictation = ui::create_label(dictation_text, 20.0, y, 240.0, 18.0, false);
        let _: () = msg_send![content_view, addSubview: dictation];
        y -= 22.0;

        // Sleep status
        let sleep_text = if state.sleep_disabled {
            "disablesleep = 1 (sleep blocked)"
        } else {
            "disablesleep = 0 (sleep allowed)"
        };
        let sleep_label = ui::create_label(sleep_text, 20.0, y, 240.0, 18.0, false);
        let _: () = msg_send![content_view, addSubview: sleep_label];
        y -= 22.0;

        // Thermal status
        let thermal_text = if state.thermal_warning {
            "🔥 Thermal: WARNING!"
        } else {
            "✓ Thermal: OK"
        };
        let thermal = ui::create_label(thermal_text, 20.0, y, 240.0, 18.0, false);
        let _: () = msg_send![content_view, addSubview: thermal];

        // Settings button at bottom left
        let settings_btn = ui::create_button("Settings", 20.0, 15.0, 80.0, 28.0);
        let _: () = msg_send![content_view, addSubview: settings_btn];

        // Quit button at bottom right
        let quit_btn = ui::create_button("Quit", 180.0, 15.0, 80.0, 28.0);
        let _: () = msg_send![content_view, addSubview: quit_btn];
    }

    pub fn hide(&mut self) {
        if let Some(window) = self.window.take() {
            unsafe {
                let _: () = msg_send![window, orderOut: NIL];
                let _: () = msg_send![window, close];
            }
            POPOVER_VISIBLE.store(false, Ordering::SeqCst);
        }
    }
}

impl Drop for PopoverWindow {
    fn drop(&mut self) {
        self.hide();
    }
}
