use core_foundation::runloop::kCFRunLoopDefaultMode;
use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl, Encode, Encoding};
use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;

pub type Id = *mut Object;

#[cfg(target_pointer_width = "64")]
pub type CGFloat = f64;
#[cfg(target_pointer_width = "32")]
pub type CGFloat = f32;

pub const NIL: Id = std::ptr::null_mut();

pub type NSWindowStyleMask = usize;
pub const NS_WINDOW_STYLE_MASK_BORDERLESS: NSWindowStyleMask = 0;

pub type NSBackingStoreType = usize;
pub const NS_BACKING_STORE_BUFFERED: NSBackingStoreType = 2;

pub type NSWindowCollectionBehavior = usize;
pub const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES: NSWindowCollectionBehavior = 1 << 0;
pub const NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY: NSWindowCollectionBehavior = 1 << 4;
pub const NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE: NSWindowCollectionBehavior = 1 << 8;

pub struct AutoreleasePool(Id);

impl AutoreleasePool {
    pub fn new() -> Self {
        unsafe {
            let pool: Id = msg_send![class!(NSAutoreleasePool), new];
            Self(pool)
        }
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                let _: () = msg_send![self.0, drain];
            }
        }
    }
}

pub fn nsstring(text: &str) -> Id {
    let cstr = CString::new(text).unwrap_or_else(|_| {
        CString::new(text.replace('\0', "")).expect("CString replacement failed")
    });
    unsafe { msg_send![class!(NSString), stringWithUTF8String: cstr.as_ptr()] }
}

pub fn nsstring_to_string(value: Id) -> Option<String> {
    if value.is_null() {
        return None;
    }
    unsafe {
        let c_str: *const c_char = msg_send![value, UTF8String];
        if c_str.is_null() {
            return None;
        }
        Some(CStr::from_ptr(c_str).to_string_lossy().into_owned())
    }
}

pub fn main_bundle_path() -> Option<String> {
    unsafe {
        let bundle: Id = msg_send![class!(NSBundle), mainBundle];
        if bundle.is_null() {
            return None;
        }
        let path: Id = msg_send![bundle, bundlePath];
        nsstring_to_string(path)
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopRunInMode(
        mode: *const c_void,
        seconds: f64,
        return_after_source_handled: bool,
    ) -> i32;
}

pub fn pump_run_loop_once() {
    unsafe {
        // Dispatch pending NSEvents first: CFRunLoop alone never delivers
        // AppKit mouse events, so clicks on agent-owned windows (dictation
        // fallback popup) would go nowhere.
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        loop {
            let distant_past: Id = msg_send![class!(NSDate), distantPast];
            let event: Id = msg_send![
                app,
                nextEventMatchingMask: u64::MAX
                untilDate: distant_past
                inMode: nsstring("kCFRunLoopDefaultMode")
                dequeue: objc::runtime::YES
            ];
            if event.is_null() {
                break;
            }
            let _: () = msg_send![app, sendEvent: event];
        }

        CFRunLoopRunInMode(kCFRunLoopDefaultMode as *const c_void, 0.0, true);
    }
}

/// Convert a tray-icon rect (top-left origin, physical pixels) into AppKit
/// screen coordinates (bottom-left origin, logical points).
pub fn tray_rect_to_appkit(rect: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let (x_px, y_px, w_px, h_px) = rect;
    unsafe {
        let screen: Id = msg_send![class!(NSScreen), mainScreen];
        if screen.is_null() {
            return rect;
        }

        let frame: NSRect = msg_send![screen, frame];
        let scale: CGFloat = msg_send![screen, backingScaleFactor];
        if scale <= 0.0 {
            return rect;
        }

        let x_pt = x_px / scale;
        let y_top_pt = y_px / scale;
        let w_pt = w_px / scale;
        let h_pt = h_px / scale;
        let y_pt = frame.size.height - y_top_pt - h_pt;

        (x_pt, y_pt, w_pt, h_pt)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NSPoint {
    pub x: CGFloat,
    pub y: CGFloat,
}

impl NSPoint {
    pub fn new(x: CGFloat, y: CGFloat) -> Self {
        Self { x, y }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NSSize {
    pub width: CGFloat,
    pub height: CGFloat,
}

impl NSSize {
    pub fn new(width: CGFloat, height: CGFloat) -> Self {
        Self { width, height }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NSRect {
    pub origin: NSPoint,
    pub size: NSSize,
}

impl NSRect {
    pub fn new(origin: NSPoint, size: NSSize) -> Self {
        Self { origin, size }
    }
}

fn point_encoding() -> Encoding {
    #[cfg(target_pointer_width = "64")]
    {
        unsafe { Encoding::from_str("{CGPoint=dd}") }
    }
    #[cfg(target_pointer_width = "32")]
    {
        unsafe { Encoding::from_str("{CGPoint=ff}") }
    }
}

fn size_encoding() -> Encoding {
    #[cfg(target_pointer_width = "64")]
    {
        unsafe { Encoding::from_str("{CGSize=dd}") }
    }
    #[cfg(target_pointer_width = "32")]
    {
        unsafe { Encoding::from_str("{CGSize=ff}") }
    }
}

fn rect_encoding() -> Encoding {
    #[cfg(target_pointer_width = "64")]
    {
        unsafe { Encoding::from_str("{CGRect={CGPoint=dd}{CGSize=dd}}") }
    }
    #[cfg(target_pointer_width = "32")]
    {
        unsafe { Encoding::from_str("{CGRect={CGPoint=ff}{CGSize=ff}}") }
    }
}

unsafe impl Encode for NSPoint {
    fn encode() -> Encoding {
        point_encoding()
    }
}

unsafe impl Encode for NSSize {
    fn encode() -> Encoding {
        size_encoding()
    }
}

unsafe impl Encode for NSRect {
    fn encode() -> Encoding {
        rect_encoding()
    }
}
