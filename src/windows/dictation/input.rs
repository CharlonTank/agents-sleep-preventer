use super::super::speech_config::Hotkey;
use anyhow::{ensure, Context, Result};
use std::{mem::size_of, ptr, sync::mpsc, thread};
use windows_sys::Win32::{
    Foundation::{GlobalFree, HWND},
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
        Ole::CF_UNICODETEXT,
        Threading::GetCurrentThreadId,
    },
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

pub struct Shortcut {
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
}
impl Shortcut {
    pub fn register(spec: Hotkey, pressed: impl Fn() + Send + 'static) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || unsafe {
            // RegisterHotKey creates the thread queue. GetMessage blocks until
            // an event arrives, unlike polling a held shortcut in a busy loop.
            if RegisterHotKey(ptr::null_mut(), 1, spec.modifiers | MOD_NOREPEAT, spec.key) == 0 {
                let _ = tx.send(Err(std::io::Error::last_os_error()));
                return;
            }
            let _ = tx.send(Ok(GetCurrentThreadId()));
            let mut message: MSG = std::mem::zeroed();
            while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
                if message.message == WM_HOTKEY && message.wParam == 1 {
                    pressed();
                }
            }
            UnregisterHotKey(ptr::null_mut(), 1);
        });
        let thread_id = rx.recv().context("Shortcut listener stopped")??;
        Ok(Self {
            thread_id,
            thread: Some(thread),
        })
    }
}
impl Drop for Shortcut {
    fn drop(&mut self) {
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy)]
pub struct Target {
    window: HWND,
    process: u32,
}
impl Target {
    pub fn capture() -> Self {
        let window = unsafe { GetForegroundWindow() };
        let mut process = 0;
        unsafe {
            GetWindowThreadProcessId(window, &mut process);
        }
        Self { window, process }
    }
    pub fn insert(self, text: &str) -> Result<()> {
        let current = Self::capture();
        ensure!(
            !self.window.is_null()
                && current.window == self.window
                && current.process == self.process,
            "The active window changed. Use Copy Last Dictation to paste the text where you want."
        );
        ensure!(
            !modifiers_down(),
            "Release the modifier keys, then use Copy Last Dictation"
        );
        let inputs = unicode_inputs(text);
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<INPUT>() as i32,
            )
        };
        ensure!(sent as usize == inputs.len(), "Windows blocked insertion (for example in an administrator app). Use Copy Last Dictation.");
        Ok(())
    }
}

pub fn modifiers_down() -> bool {
    [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
        .iter()
        .any(|key| unsafe { GetAsyncKeyState(*key as i32) < 0 })
}

fn unicode_inputs(text: &str) -> Vec<INPUT> {
    text.encode_utf16()
        .flat_map(|unit| {
            [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP].map(|flags| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: 0,
                        wScan: unit,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            })
        })
        .collect()
}

pub fn copy_text(text: &str) -> Result<()> {
    let units: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let owner = CreateWindowExW(
            0,
            class.as_ptr(),
            ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        );
        ensure!(!owner.is_null(), "Could not create the clipboard owner");
        if OpenClipboard(owner) == 0 {
            DestroyWindow(owner);
            anyhow::bail!("Clipboard is busy; try again");
        }
        let result = (|| -> Result<()> {
            let memory = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
            ensure!(!memory.is_null(), "Could not allocate clipboard text");
            let buffer = GlobalLock(memory);
            if buffer.is_null() {
                GlobalFree(memory);
                anyhow::bail!("Could not access clipboard memory");
            }
            ptr::copy_nonoverlapping(units.as_ptr(), buffer.cast(), units.len());
            GlobalUnlock(memory);
            if EmptyClipboard() == 0 || SetClipboardData(CF_UNICODETEXT as u32, memory).is_null() {
                GlobalFree(memory);
                anyhow::bail!("Could not copy the dictation");
            }
            Ok(())
        })();
        CloseClipboard();
        DestroyWindow(owner);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_typing_preserves_accents_and_surrogate_pairs() {
        let text = "été 世界 🙂";
        let inputs = unicode_inputs(text);
        assert_eq!(inputs.len(), text.encode_utf16().count() * 2);
        let words = inputs
            .chunks_exact(2)
            .map(|pair| unsafe {
                assert_eq!(
                    pair[1].Anonymous.ki.dwFlags,
                    KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                );
                assert_eq!(pair[0].Anonymous.ki.wVk, 0);
                pair[0].Anonymous.ki.wScan
            })
            .collect::<Vec<_>>();
        assert_eq!(String::from_utf16(&words).unwrap(), text);
    }
}
