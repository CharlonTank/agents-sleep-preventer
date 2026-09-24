use objc::runtime::{BOOL, YES};
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::logging;
use crate::objc_utils::{
    nsstring, AutoreleasePool, CGFloat, Id, NSPoint, NSRect, NSSize, NIL,
    NS_BACKING_STORE_BUFFERED, NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES,
    NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE, NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY,
    NS_WINDOW_STYLE_MASK_BORDERLESS,
};

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {}

const BORDER_WIDTH: CGFloat = 20.0;
const MAX_BORDER_WIDTH: CGFloat = 32.0;
const ENTRANCE_WIDTH: CGFloat = 36.0;
const ENTRANCE_DURATION: f64 = 0.7;
const INNER_CORNER_RADIUS: CGFloat = 28.0;
const MASK_STEPS: usize = 32;
const DEPTH_PROFILES: [[CGFloat; 5]; 5] = [
    [0.92, 0.62, 0.28, 0.07, 0.0],
    [1.00, 0.90, 0.48, 0.10, 0.0],
    [0.82, 0.74, 0.62, 0.18, 0.0],
    [0.88, 0.58, 0.34, 0.12, 0.0],
    [0.92, 0.62, 0.28, 0.07, 0.0],
];

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPathCreateMutable() -> *mut c_void;
    fn CGPathAddRect(path: *mut c_void, transform: *const c_void, rect: NSRect);
    fn CGPathAddRoundedRect(
        path: *mut c_void,
        transform: *const c_void,
        rect: NSRect,
        corner_width: CGFloat,
        corner_height: CGFloat,
    );
    fn CGPathRelease(path: *const c_void);
}

static OVERLAY_VISIBLE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverlayMode {
    Recording,
    Transcribing,
}

pub struct RecordingOverlay {
    window: Option<Id>,
    mode: OverlayMode,
    mask_layers: Vec<Id>,
    border_size: NSSize,
    voice_level: f64,
    last_level_update: Instant,
    reduce_motion: bool,
    entrance_started: Option<Instant>,
    preview_window: Option<Id>,
    preview_label: Option<Id>,
}

impl RecordingOverlay {
    pub fn new() -> Self {
        Self {
            window: None,
            mode: OverlayMode::Recording,
            mask_layers: Vec::new(),
            border_size: NSSize::default(),
            voice_level: 0.0,
            last_level_update: Instant::now(),
            reduce_motion: false,
            entrance_started: None,
            preview_window: None,
            preview_label: None,
        }
    }

    pub fn show(&mut self) {
        self.show_with_mode(OverlayMode::Recording);
    }

    pub fn show_with_mode(&mut self, mode: OverlayMode) {
        self.mode = mode;
        self.entrance_started = if self.window.is_none() && mode == OverlayMode::Recording {
            Some(Instant::now())
        } else {
            None
        };

        if let Some(window) = self.window {
            unsafe {
                let content_view: Id = msg_send![window, contentView];
                self.configure_border(content_view, mode);
            }
            logging::log(&format!("[overlay] Updated color: {:?}", mode));
            return;
        }

        unsafe {
            let screen: Id = msg_send![class!(NSScreen), mainScreen];
            if screen.is_null() {
                logging::log("[overlay] ERROR: NSScreen::mainScreen returned nil");
                return;
            }
            let screen_frame: NSRect = msg_send![screen, frame];

            let window: Id = msg_send![class!(NSWindow), alloc];
            let window: Id = msg_send![
                window,
                initWithContentRect: screen_frame
                styleMask: NS_WINDOW_STYLE_MASK_BORDERLESS
                backing: NS_BACKING_STORE_BUFFERED
                defer: false as BOOL
            ];

            if window.is_null() {
                logging::log("[overlay] ERROR: Failed to create NSWindow");
                return;
            }

            let _: () = msg_send![window, setLevel: 25i64];
            let _: () = msg_send![window, setOpaque: false as BOOL];
            let _: () = msg_send![window, setHasShadow: false as BOOL];
            let _: () = msg_send![window, setIgnoresMouseEvents: YES];
            let _: () = msg_send![window, setFrame: screen_frame display: YES];

            let behavior = NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
                | NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY
                | NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE;
            let _: () = msg_send![window, setCollectionBehavior: behavior];

            // A transparent, click-through window outlines the entire display.
            let clear: Id = msg_send![class!(NSColor), clearColor];
            let _: () = msg_send![window, setBackgroundColor: clear];
            let content_view: Id = msg_send![window, contentView];
            let _: () = msg_send![content_view, setWantsLayer: YES];
            self.configure_border(content_view, mode);

            let _: () = msg_send![window, orderFrontRegardless];

            self.window = Some(window);
            OVERLAY_VISIBLE.store(true, Ordering::SeqCst);
            logging::log(&format!("[overlay] Shown: {:?}", mode));
        }
    }

    /// Nested rounded rings produce one continuous feathered opening, without
    /// the square inner corners or doubled opacity of four intersecting strips.
    fn configure_border(&mut self, content_view: Id, mode: OverlayMode) {
        let pool = AutoreleasePool::new();
        self.mask_layers.clear();
        self.voice_level = 0.0;
        self.last_level_update = Instant::now();
        unsafe {
            // Build the complete halo before showing it: no implicit growth from
            // zero-sized layers (which used to look like a floating thin line).
            let _: () = msg_send![class!(CATransaction), begin];
            let _: () = msg_send![class!(CATransaction), setDisableActions: YES];
            let bounds: NSRect = msg_send![content_view, bounds];
            let root: Id = msg_send![content_view, layer];
            let gradient: Id = msg_send![class!(CAGradientLayer), layer];
            let _: () = msg_send![gradient, setFrame: bounds];
            let _: () = msg_send![gradient, setStartPoint: NSPoint::new(0.0, 0.0)];
            let _: () = msg_send![gradient, setEndPoint: NSPoint::new(1.0, 1.0)];
            let _: () = msg_send![gradient, setColors: border_palette(mode, 0)];

            let workspace: Id = msg_send![class!(NSWorkspace), sharedWorkspace];
            let reduce_motion: BOOL = msg_send![workspace, accessibilityDisplayShouldReduceMotion];
            self.reduce_motion = reduce_motion == YES;
            if !self.reduce_motion {
                let frames: Id = msg_send![class!(NSMutableArray), array];
                for phase in 0..=6 {
                    let _: () = msg_send![frames, addObject: border_palette(mode, phase)];
                }
                add_border_animation(gradient, "colors", frames, 9.0);
            }

            let mask: Id = msg_send![class!(CALayer), layer];
            let _: () = msg_send![mask, setFrame: bounds];
            let _: () = msg_send![mask, setMasksToBounds: YES];
            self.border_size = bounds.size;
            let window: Id = msg_send![content_view, window];
            let scale: CGFloat = msg_send![window, backingScaleFactor];
            let white: Id = msg_send![class!(NSColor), whiteColor];
            let white_cg: Id = msg_send![white, CGColor];
            let thickness = if !self.reduce_motion && self.entrance_started.is_some() {
                ENTRANCE_WIDTH
            } else {
                BORDER_WIDTH
            };
            for index in 0..MASK_STEPS {
                let ring: Id = msg_send![class!(CAShapeLayer), layer];
                let _: () = msg_send![ring, setFrame: bounds];
                let _: () = msg_send![ring, setContentsScale: scale];
                let _: () = msg_send![ring, setFillRule: nsstring("even-odd")];
                let _: () = msg_send![ring, setFillColor: white_cg];
                let _: () = msg_send![ring, setOpacity: ring_opacity(index, DEPTH_PROFILES[0])];
                set_ring_path(ring, bounds.size, thickness, index, false);
                if !self.reduce_motion {
                    let frames: Id = msg_send![class!(NSMutableArray), array];
                    for profile in DEPTH_PROFILES {
                        let value: Id = msg_send![class!(NSNumber), numberWithFloat: ring_opacity(index, profile)];
                        let _: () = msg_send![frames, addObject: value];
                    }
                    let duration = match mode {
                        OverlayMode::Recording => 1.8,
                        OverlayMode::Transcribing => 2.8,
                    };
                    add_border_animation(ring, "opacity", frames, duration);
                }
                let _: () = msg_send![mask, addSublayer: ring];
                self.mask_layers.push(ring);
            }
            let _: () = msg_send![gradient, setMask: mask];
            // Replacing the owned layer tree also retires the previous animations.
            let layers: Id = msg_send![class!(NSArray), arrayWithObject: gradient];
            let _: () = msg_send![root, setSublayers: layers];
            let _: () = msg_send![class!(CATransaction), commit];
            let _: () = msg_send![class!(CATransaction), flush];
        }
        drop(pool);
    }

    pub fn set_voice_level(&mut self, level: f32) {
        if self.window.is_none() || self.mode != OverlayMode::Recording || self.reduce_motion {
            return;
        }
        let now = Instant::now();
        let elapsed = now
            .duration_since(self.last_level_update)
            .as_secs_f64()
            .min(0.1);
        self.last_level_update = now;
        let target = if level.is_finite() {
            f64::from(level.clamp(0.0, 1.0))
        } else {
            0.0
        };
        // A quick attack follows syllables; a slower release settles between words.
        let response = if target > self.voice_level {
            0.07
        } else {
            0.28
        };
        self.voice_level += (target - self.voice_level) * (1.0 - (-elapsed / response).exp());
        let entrance = self.entrance_started.map_or(0.0, |started| {
            entrance_expansion(now.duration_since(started).as_secs_f64())
        });
        if entrance == 0.0 {
            self.entrance_started = None;
        }
        let thickness =
            BORDER_WIDTH + (MAX_BORDER_WIDTH - BORDER_WIDTH) * self.voice_level + entrance;
        // The agent pumps AppKit manually, so release the temporary animation
        // objects every tick instead of retaining them throughout a dictation.
        let pool = AutoreleasePool::new();
        unsafe {
            let _: () = msg_send![class!(CATransaction), begin];
            let _: () = msg_send![class!(CATransaction), setDisableActions: YES];
            for (index, ring) in self.mask_layers.iter().enumerate() {
                set_ring_path(*ring, self.border_size, thickness, index, true);
            }
            let _: () = msg_send![class!(CATransaction), commit];
        }
        drop(pool);
    }

    pub fn set_mode(&mut self, mode: OverlayMode) {
        if self.window.is_some() {
            self.show_with_mode(mode);
        }
    }

    /// Show (or update) the live transcription preview pill above the bottom edge.
    pub fn set_preview_text(&mut self, text: &str) {
        unsafe {
            if self.preview_window.is_none() {
                let screen: Id = msg_send![class!(NSScreen), mainScreen];
                if screen.is_null() {
                    return;
                }
                let screen_frame: NSRect = msg_send![screen, frame];

                let width: CGFloat = (screen_frame.size.width * 0.6).min(700.0);
                let height: CGFloat = 34.0;
                let frame = NSRect::new(
                    NSPoint::new(
                        screen_frame.origin.x + (screen_frame.size.width - width) / 2.0,
                        screen_frame.origin.y + ENTRANCE_WIDTH + 12.0,
                    ),
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
                let _: () = msg_send![window, setHasShadow: false as BOOL];
                let _: () = msg_send![window, setIgnoresMouseEvents: YES];
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
                    let _: () = msg_send![layer, setCornerRadius: 10.0 as CGFloat];
                    let bg: Id = msg_send![
                        class!(NSColor),
                        colorWithRed: 0.08
                        green: 0.08
                        blue: 0.08
                        alpha: 0.88
                    ];
                    let cg_color: Id = msg_send![bg, CGColor];
                    let _: () = msg_send![layer, setBackgroundColor: cg_color];
                }

                let label_frame = NSRect::new(
                    NSPoint::new(14.0, 7.0),
                    NSSize::new(width - 28.0, height - 14.0),
                );
                let label: Id = msg_send![class!(NSTextField), alloc];
                let label: Id = msg_send![label, initWithFrame: label_frame];
                let _: () = msg_send![label, setBezeled: false as BOOL];
                let _: () = msg_send![label, setDrawsBackground: false as BOOL];
                let _: () = msg_send![label, setEditable: false as BOOL];
                let _: () = msg_send![label, setSelectable: false as BOOL];
                let font: Id = msg_send![class!(NSFont), systemFontOfSize: 14.0 as CGFloat];
                let _: () = msg_send![label, setFont: font];
                let color: Id = msg_send![class!(NSColor), whiteColor];
                let _: () = msg_send![label, setTextColor: color];
                // Truncate the head so the newest words stay visible.
                let cell: Id = msg_send![label, cell];
                let _: () = msg_send![cell, setLineBreakMode: 3u64]; // NSLineBreakByTruncatingHead
                let _: () = msg_send![content_view, addSubview: label];

                let _: () = msg_send![window, makeKeyAndOrderFront: NIL];

                self.preview_window = Some(window);
                self.preview_label = Some(label);
            }

            if let Some(label) = self.preview_label {
                let _: () = msg_send![label, setStringValue: crate::objc_utils::nsstring(text)];
            }
        }
    }

    fn hide_preview(&mut self) {
        self.preview_label = None;
        if let Some(window) = self.preview_window.take() {
            unsafe {
                let _: () = msg_send![window, orderOut: NIL];
                let _: () = msg_send![window, close];
            }
        }
    }

    pub fn hide(&mut self) {
        self.mask_layers.clear();
        self.voice_level = 0.0;
        self.entrance_started = None;
        self.hide_preview();
        if let Some(window) = self.window.take() {
            unsafe {
                let _: () = msg_send![window, orderOut: NIL];
                let _: () = msg_send![window, close];
            }
            logging::log("[overlay] Hidden");
        }
        OVERLAY_VISIBLE.store(false, Ordering::SeqCst);
    }
}

impl Drop for RecordingOverlay {
    fn drop(&mut self) {
        self.hide();
    }
}

/// A cyclic palette lets the colours flow without a jump at the loop boundary.
fn border_palette(mode: OverlayMode, phase: usize) -> Id {
    let palette = match mode {
        OverlayMode::Recording => [
            (0.12, 0.40, 1.00), // electric blue
            (0.28, 0.86, 1.00), // icy glass highlight
            (0.44, 0.28, 1.00), // blue violet
            (1.00, 0.32, 0.27), // coral red
            (1.00, 0.64, 0.22), // orange
            (0.72, 0.28, 1.00), // violet
        ],
        OverlayMode::Transcribing => [
            (0.44, 0.28, 1.00),
            (0.75, 0.38, 1.00),
            (1.00, 0.42, 0.28),
            (1.00, 0.70, 0.32),
            (1.00, 0.38, 0.48),
            (0.32, 0.50, 1.00),
        ],
    };
    unsafe {
        let colors: Id = msg_send![class!(NSMutableArray), array];
        for index in 0..=palette.len() {
            let (red, green, blue) = palette[(index + phase) % palette.len()];
            let color: Id =
                msg_send![class!(NSColor), colorWithRed: red green: green blue: blue alpha: 1.0f64];
            let cg_color: Id = msg_send![color, CGColor];
            let _: () = msg_send![colors, addObject: cg_color];
        }
        colors
    }
}

fn add_border_animation(layer: Id, key: &str, values: Id, duration: f64) {
    unsafe {
        let key = nsstring(key);
        let animation: Id = msg_send![class!(CAKeyframeAnimation), animationWithKeyPath: key];
        let _: () = msg_send![animation, setValues: values];
        let _: () = msg_send![animation, setDuration: duration];
        let _: () = msg_send![animation, setRepeatCount: f32::MAX];
        let _: () = msg_send![layer, addAnimation: animation forKey: key];
    }
}

/// Start with a fully formed halo and settle without ever collapsing to a line.
fn entrance_expansion(elapsed: f64) -> CGFloat {
    let remaining = 1.0 - (elapsed / ENTRANCE_DURATION).clamp(0.0, 1.0);
    (ENTRANCE_WIDTH - BORDER_WIDTH) * remaining.powi(3)
}

fn depth_alpha(depth: CGFloat, profile: [CGFloat; 5]) -> CGFloat {
    let position = depth.clamp(0.0, 1.0) * 4.0;
    let index = (position.floor() as usize).min(3);
    let fraction = position - index as CGFloat;
    profile[index] + (profile[index + 1] - profile[index]) * fraction
}

/// Account for alpha compositing between overlapping rings, so the complete
/// border follows the intended gradient and has no bright seams at the joins.
fn ring_opacity(index: usize, profile: [CGFloat; 5]) -> f32 {
    let outer = depth_alpha(index as CGFloat / MASK_STEPS as CGFloat, profile);
    let inner = depth_alpha((index + 1) as CGFloat / MASK_STEPS as CGFloat, profile);
    ((outer - inner) / (1.0 - inner)).clamp(0.0, 1.0) as f32
}

fn set_ring_path(layer: Id, size: NSSize, thickness: CGFloat, index: usize, animated: bool) {
    unsafe {
        let depth = thickness * (index + 1) as CGFloat / MASK_STEPS as CGFloat;
        let path = CGPathCreateMutable();
        CGPathAddRect(
            path,
            std::ptr::null(),
            NSRect::new(NSPoint::new(0.0, 0.0), size),
        );
        let opening = NSRect::new(
            NSPoint::new(depth, depth),
            NSSize::new(
                (size.width - 2.0 * depth).max(1.0),
                (size.height - 2.0 * depth).max(1.0),
            ),
        );
        // The outermost contours stay close to the physical corner. Rounding
        // increases smoothly inwards instead of cutting a rectangular opening.
        let radius = INNER_CORNER_RADIUS * (depth / thickness).sqrt();
        CGPathAddRoundedRect(path, std::ptr::null(), opening, radius, radius);
        if animated {
            let presentation: Id = msg_send![layer, presentationLayer];
            let source = if presentation.is_null() {
                layer
            } else {
                presentation
            };
            let previous: Id = msg_send![source, path];
            let animation: Id =
                msg_send![class!(CABasicAnimation), animationWithKeyPath: nsstring("path")];
            let _: () = msg_send![animation, setFromValue: previous];
            let _: () = msg_send![animation, setToValue: path as Id];
            let _: () = msg_send![animation, setDuration: 0.08f64];
            let _: () = msg_send![layer, addAnimation: animation forKey: nsstring("voice-path")];
        }
        let _: () = msg_send![layer, setPath: path];
        CGPathRelease(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entrance_starts_full_and_settles_to_the_requested_width() {
        assert_eq!(BORDER_WIDTH + entrance_expansion(0.0), ENTRANCE_WIDTH);
        assert!(entrance_expansion(0.2) > entrance_expansion(0.5));
        assert_eq!(entrance_expansion(ENTRANCE_DURATION), 0.0);
        assert_eq!(entrance_expansion(3.0), 0.0);
    }

    #[test]
    fn rounded_rings_preserve_the_feathered_alpha_without_seams() {
        for profile in DEPTH_PROFILES {
            for depth in 0..MASK_STEPS {
                let transparency = (depth..MASK_STEPS)
                    .map(|index| 1.0 - f64::from(ring_opacity(index, profile)))
                    .product::<f64>();
                let expected = depth_alpha(depth as f64 / MASK_STEPS as f64, profile);
                assert!((1.0 - transparency - expected).abs() < 1e-6);
            }
        }
    }
}
