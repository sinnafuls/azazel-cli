//! Seeing the user's Esc anywhere on the desktop, whatever has the focus. Windows has a
//! low-level keyboard hook; the key still goes on to the application, which is never denied it.

use std::sync::Arc;

/// Called, on the hook's thread, for every press of Esc (the tool's own included).
pub type OnEscape = Arc<dyn Fn() + Send + Sync>;

#[cfg(windows)]
pub use windows::Monitor;

#[cfg(not(windows))]
pub use other::Monitor;

#[cfg(windows)]
mod windows {
    use std::ptr::null_mut;
    use std::sync::mpsc;
    use std::sync::{Mutex, PoisonError};
    use std::thread::JoinHandle;

    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, PeekMessageW,
        PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT,
        WM_SYSKEYDOWN,
    };

    use super::OnEscape;

    const VK_ESCAPE: u32 = 0x1B;

    // A hook procedure is a bare function and carries no state of its own, so what it calls
    // lives here. One monitor is armed at a time.
    static SINK: Mutex<Option<OnEscape>> = Mutex::new(None);

    unsafe extern "system" fn procedure(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 && matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
            // SAFETY: for HC_ACTION, `lparam` points to the KBDLLHOOKSTRUCT of this event.
            let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            if key.vkCode == VK_ESCAPE {
                let sink = SINK.lock().unwrap_or_else(PoisonError::into_inner).clone();
                if let Some(sink) = sink {
                    sink();
                }
            }
        }
        // SAFETY: passes the event on unchanged.
        unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
    }

    /// The keyboard hook and the thread that pumps its messages; dropping it removes the hook.
    pub struct Monitor {
        thread: u32,
        join: Option<JoinHandle<()>>,
    }

    impl Monitor {
        pub fn start(on_escape: OnEscape) -> Result<Monitor, String> {
            *SINK.lock().unwrap_or_else(PoisonError::into_inner) = Some(on_escape);
            let (tx, rx) = mpsc::channel();
            let join = std::thread::spawn(move || {
                // SAFETY: plain Win32 calls on this thread; the hook is removed before it ends.
                unsafe {
                    let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(procedure), null_mut(), 0);
                    if hook.is_null() {
                        let _ = tx.send(Err("the keyboard hook could not be set".to_string()));
                        return;
                    }
                    // Makes the thread's message queue, so that the quit message has somewhere to go.
                    let mut message: MSG = std::mem::zeroed();
                    PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE);
                    let _ = tx.send(Ok(GetCurrentThreadId()));
                    while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {}
                    UnhookWindowsHookEx(hook);
                }
            });
            match rx.recv() {
                Ok(Ok(thread)) => Ok(Monitor {
                    thread,
                    join: Some(join),
                }),
                Ok(Err(why)) => Err(why),
                Err(e) => Err(e.to_string()),
            }
        }
    }

    impl Drop for Monitor {
        fn drop(&mut self) {
            // SAFETY: posts a message to the thread started above.
            unsafe {
                PostThreadMessageW(self.thread, WM_QUIT, 0, 0);
            }
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
            *SINK.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }
}

#[cfg(not(windows))]
mod other {
    use super::OnEscape;

    /// Without a way to see the user's Esc on this platform.
    pub struct Monitor;

    impl Monitor {
        // shortcut: no global Esc hook on macOS and Linux; upgrade when a hook (a CGEvent tap, an
        // X11 key grab) is added for them
        pub fn start(_: OnEscape) -> Result<Monitor, String> {
            Err("the Esc abort is only available on Windows so far".to_string())
        }
    }
}
