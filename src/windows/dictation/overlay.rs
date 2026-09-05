use anyhow::{ensure, Result};
use std::ptr;
use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTOPRIMARY},
    UI::WindowsAndMessaging::*,
};

pub struct Overlay(HWND);
impl Overlay {
    pub fn new() -> Result<Self> {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                ptr::null(),
                WS_POPUP | 0x201,
                0,
                0,
                460,
                56,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        ensure!(
            !window.is_null(),
            "Could not create the dictation indicator"
        );
        Ok(Self(window))
    }
    pub fn show(&self, text: &str) {
        let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(monitor, &mut info);
            let x = (info.rcWork.left + info.rcWork.right - 460) / 2;
            let y = info.rcWork.bottom - 90;
            SetWindowTextW(self.0, text.as_ptr());
            SetWindowPos(
                self.0,
                HWND_TOPMOST,
                x,
                y,
                460,
                56,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }
    pub fn hide(&self) {
        unsafe {
            ShowWindow(self.0, SW_HIDE);
        }
    }
}
impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}
