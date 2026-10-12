//! Keyboard grab for the viewer (07 §2.3): while the viewer window is in
//! front, every key goes to the remote host, including the ones the local
//! system would otherwise act on (Win, Alt+Tab, Alt+Esc, Ctrl+Esc, Alt+F4).
//!
//! On Windows this is a low-level keyboard hook (`WH_KEYBOARD_LL`) installed
//! on the window's thread. While the grab is active and the given window is
//! the foreground window, each key event is turned into a [`KeyCode`] and
//! handed to a callback, and the system never sees it. Exceptions, which
//! pass through untouched:
//!
//! - the lock keys (Caps/Num/Scroll Lock), so the local lock states keep
//!   following the user; the host follows them through lock-state sync;
//! - events a KanoDesk host on this same machine injected (marked with
//!   [`crate::windows::INJECTED_MARKER`]), so a local host can type into the
//!   viewer without a feedback loop;
//! - keys without a HID usage.
//!
//! Ctrl+Alt+Del and Win+L are handled below any hook and stay local.
//!
//! The decision itself is the plain function [`decide`], tested without a hook.

use crate::event::KeyCode;
use crate::scancode::from_pc;

/// One key event taken from the local keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrabbedKey {
    /// The key.
    pub code: KeyCode,
    /// Pressed (also auto-repeat) or released.
    pub down: bool,
}

/// What the hook does with one key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Let the local system have it.
    Pass,
    /// Swallow it without forwarding (keyboard noise such as fake Shifts).
    Drop,
    /// Swallow it and forward it to the host.
    Forward(GrabbedKey),
}

/// One raw keyboard event as the hook sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawKey {
    /// Set-1 scan code.
    pub scan: u16,
    /// `E0`-prefixed.
    pub extended: bool,
    /// Windows virtual key.
    pub vk: u16,
    /// Press (`true`) or release.
    pub down: bool,
    /// Injected by a KanoDesk host on this machine.
    pub own_injection: bool,
}

/// The hook's decision for `key`, given whether the grab is active and the
/// viewer window is in front.
pub fn decide(key: RawKey, grabbing: bool) -> Decision {
    if !grabbing || key.own_injection {
        return Decision::Pass;
    }
    match from_pc(key.scan, key.extended, key.vk) {
        Some(code) if code.lock().is_some() => Decision::Pass,
        Some(code) => Decision::Forward(GrabbedKey {
            code,
            down: key.down,
        }),
        // The fake Shift some keyboards wrap around extended keys.
        None if key.scan == 0x2A && key.extended => Decision::Drop,
        None => Decision::Pass,
    }
}

#[cfg(windows)]
pub use imp::KeyboardGrab;

#[cfg(windows)]
mod imp {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetForegroundWindow, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
        SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
    };

    use super::{Decision, GrabbedKey, RawKey, decide};
    use crate::InputError;
    use crate::windows::INJECTED_MARKER;

    type Callback = Box<dyn Fn(GrabbedKey) + Send>;

    /// One grab per process (a hook procedure has no context argument).
    static CALLBACK: Mutex<Option<Callback>> = Mutex::new(None);
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static WINDOW: AtomicIsize = AtomicIsize::new(0);
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 && ACTIVE.load(Ordering::Relaxed) {
            // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            // SAFETY: plain call.
            let front = unsafe { GetForegroundWindow() };
            let grabbing = front.0 as isize == WINDOW.load(Ordering::Relaxed);
            let raw = RawKey {
                scan: info.scanCode as u16,
                extended: info.flags.contains(LLKHF_EXTENDED),
                vk: info.vkCode as u16,
                down: matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN),
                own_injection: info.dwExtraInfo == INJECTED_MARKER,
            };
            match decide(raw, grabbing) {
                Decision::Pass => {}
                Decision::Drop => return LRESULT(1),
                Decision::Forward(key) => {
                    if let Ok(callback) = CALLBACK.try_lock()
                        && let Some(callback) = callback.as_ref()
                    {
                        callback(key);
                    }
                    return LRESULT(1);
                }
            }
        }
        // SAFETY: passes the event on unchanged.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// An installed keyboard grab; see the module docs. Install and drop it
    /// on the thread that runs the window's message loop.
    pub struct KeyboardGrab {
        hook: HHOOK,
    }

    impl KeyboardGrab {
        /// Installs the hook. Keys go to `on_key` while the grab is active
        /// ([`Self::set_active`]) and `window` (an `HWND`) is in front.
        pub fn install(
            window: isize,
            on_key: impl Fn(GrabbedKey) + Send + 'static,
        ) -> Result<KeyboardGrab, InputError> {
            if INSTALLED.swap(true, Ordering::SeqCst) {
                return Err(InputError::Unsupported("a second keyboard grab"));
            }
            *CALLBACK.lock().expect("not poisoned") = Some(Box::new(on_key));
            WINDOW.store(window, Ordering::Relaxed);
            ACTIVE.store(false, Ordering::Relaxed);
            // SAFETY: a valid hook procedure; the module handle of this
            // process; 0 = all threads (required for low-level hooks).
            let result = unsafe {
                let module = GetModuleHandleW(None).ok();
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module.map(Into::into), 0)
            };
            match result {
                Ok(hook) => Ok(KeyboardGrab { hook }),
                Err(e) => {
                    *CALLBACK.lock().expect("not poisoned") = None;
                    INSTALLED.store(false, Ordering::SeqCst);
                    Err(InputError::Os(format!("SetWindowsHookEx: {e}")))
                }
            }
        }

        /// Starts or stops grabbing (call with the window's focus state).
        pub fn set_active(&self, active: bool) {
            ACTIVE.store(active, Ordering::Relaxed);
        }

        /// Whether the grab is active.
        pub fn is_active(&self) -> bool {
            ACTIVE.load(Ordering::Relaxed)
        }

        /// The window whose being in front enables the grab.
        pub fn window(&self) -> HWND {
            HWND(WINDOW.load(Ordering::Relaxed) as *mut _)
        }
    }

    impl Drop for KeyboardGrab {
        fn drop(&mut self) {
            ACTIVE.store(false, Ordering::Relaxed);
            // SAFETY: the hook this value installed.
            let _ = unsafe { UnhookWindowsHookEx(self.hook) };
            *CALLBACK.lock().expect("not poisoned") = None;
            INSTALLED.store(false, Ordering::SeqCst);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Arc;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
            SendInput, VIRTUAL_KEY,
        };
        use windows::Win32::UI::WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW};

        #[test]
        fn only_one_grab_at_a_time() {
            let first = KeyboardGrab::install(0, |_| {}).unwrap();
            assert!(KeyboardGrab::install(0, |_| {}).is_err());
            drop(first);
            let again = KeyboardGrab::install(0, |_| {}).unwrap();
            assert!(!again.is_active());
        }

        #[test]
        #[ignore = "grabs the real keyboard for a moment and presses F24"]
        fn grabbed_keys_reach_the_callback_and_not_the_system() {
            use crate::KeyCode;
            let got = Arc::new(Mutex::new(Vec::new()));
            let sink = got.clone();
            // SAFETY: plain call.
            let front = unsafe { GetForegroundWindow() };
            let grab = KeyboardGrab::install(front.0 as isize, move |k| {
                sink.lock().unwrap().push(k);
            })
            .unwrap();
            grab.set_active(true);
            // An unmarked F24 (scan 0x76), as a physical keyboard would send.
            let press = |flags| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: 0x76,
                        dwFlags: KEYEVENTF_SCANCODE | flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            // SAFETY: valid inputs.
            unsafe {
                SendInput(
                    &[press(Default::default()), press(KEYEVENTF_KEYUP)],
                    std::mem::size_of::<INPUT>() as i32,
                )
            };
            // Low-level hooks run while this thread pumps messages.
            let until = std::time::Instant::now() + std::time::Duration::from_millis(300);
            let mut msg = MSG::default();
            while std::time::Instant::now() < until && got.lock().unwrap().len() < 2 {
                // SAFETY: valid out pointer.
                unsafe { while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {} }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            grab.set_active(false);
            drop(grab);
            let f24 = KeyCode(0x73);
            assert_eq!(
                *got.lock().unwrap(),
                vec![
                    GrabbedKey {
                        code: f24,
                        down: true
                    },
                    GrabbedKey {
                        code: f24,
                        down: false
                    }
                ]
            );
        }
    }
}

/// Placeholder where no keyboard grab exists: installing always fails, so
/// callers fall back to their window's own key events.
#[cfg(not(windows))]
pub struct KeyboardGrab;

#[cfg(not(windows))]
impl KeyboardGrab {
    /// Always fails on this OS.
    pub fn install(
        _window: isize,
        _on_key: impl Fn(GrabbedKey) + Send + 'static,
    ) -> Result<KeyboardGrab, crate::InputError> {
        Err(crate::InputError::Unsupported("keyboard grab on this OS"))
    }

    /// Does nothing.
    pub fn set_active(&self, _active: bool) {}

    /// Always `false`.
    pub fn is_active(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(scan: u16, extended: bool, vk: u16) -> RawKey {
        RawKey {
            scan,
            extended,
            vk,
            down: true,
            own_injection: false,
        }
    }

    #[test]
    fn system_keys_are_forwarded_while_grabbing() {
        // Left Windows key (E0 5B), Tab, Escape, F4.
        for (scan, ext, usage) in [
            (0x5B, true, 0xE3),
            (0x0F, false, 0x2B),
            (0x01, false, 0x29),
            (0x3E, false, 0x3D),
        ] {
            assert_eq!(
                decide(raw(scan, ext, 0), true),
                Decision::Forward(GrabbedKey {
                    code: KeyCode(usage),
                    down: true
                })
            );
        }
        // Media keys by virtual key.
        assert_eq!(
            decide(raw(0x30, true, 0xAF), true),
            Decision::Forward(GrabbedKey {
                code: KeyCode::consumer(0xE9),
                down: true
            })
        );
    }

    #[test]
    fn pass_through_cases() {
        // Not grabbing (viewer not in front): everything stays local.
        assert_eq!(decide(raw(0x5B, true, 0x5B), false), Decision::Pass);
        // A local host's own injection.
        let own = RawKey {
            own_injection: true,
            ..raw(0x1E, false, 0x41)
        };
        assert_eq!(decide(own, true), Decision::Pass);
        // Lock keys keep working locally (the host follows by lock sync).
        assert_eq!(decide(raw(0x3A, false, 0x14), true), Decision::Pass);
        assert_eq!(decide(raw(0x45, false, 0x90), true), Decision::Pass);
        // Unknown keys.
        assert_eq!(decide(raw(0x00, false, 0xFF), true), Decision::Pass);
        // Fake Shift is swallowed silently.
        assert_eq!(decide(raw(0x2A, true, 0xA0), true), Decision::Drop);
    }
}
