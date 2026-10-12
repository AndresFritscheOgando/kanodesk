//! Windows backend: `SendInput` with scan codes and absolute virtual-desktop
//! coordinates.
//!
//! - Keys are sent as set-1 scan codes ([`crate::scancode`]), so the host's
//!   layout decides the character (Map mode).
//! - Positions are normalised over the whole virtual desktop
//!   (`MOUSEEVENTF_VIRTUALDESK`). The desktop's size is read on a thread
//!   switched to per-monitor DPI awareness, so the numbers are physical
//!   pixels, the same space DXGI capture reports.
//! - Media/browser keys, Pause and the Korean language keys go as virtual
//!   keys; text (Translate mode) as `KEYEVENTF_UNICODE` characters, which
//!   bypass the keyboard layout; relative moves as plain `MOUSEEVENTF_MOVE`
//!   (pointer acceleration applies; games reading raw input see the deltas).
//! - Every injected event carries [`INJECTED_MARKER`] as its extra info. A
//!   viewer running on the same machine checks it ([`is_own_injection`]) and
//!   does not forward the host's own injected input back again.
//! - Lock states are read with `GetKeyState`; "lock screen" is
//!   `LockWorkStation`.
//!
//! `SendInput` cannot reach the secure desktop (Ctrl+Alt+Del, UAC prompts)
//! or windows of a higher integrity level (UIPI) from a normal process, and
//! Ctrl+Alt+Del itself needs `SendSAS` from the service (prompt 11).

use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageExtraInfo, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, KEYEVENTF_UNICODE, VK_CAPITAL, VK_NUMLOCK, VK_SCROLL,
};

use crate::InputError;
use crate::event::{Action, KeyCode, Modifiers, MouseButton};
use crate::injector::InputSink;
use crate::scancode::{PcKey, to_pc};

/// Extra-info value on every event this backend injects ("KDIN").
pub const INJECTED_MARKER: usize = 0x4B44_494E;

/// Whether the window message being handled right now was injected by a
/// KanoDesk host on this machine. Call it from the window procedure (event
/// handlers that run while the message is dispatched).
pub fn is_own_injection() -> bool {
    // SAFETY: no arguments; reads thread-local message state.
    let info = unsafe { GetMessageExtraInfo() };
    info.0 as usize == INJECTED_MARKER
}

/// The virtual desktop in physical pixels: (left, top, width, height).
pub fn virtual_desktop() -> (i32, i32, i32, i32) {
    // SAFETY: plain calls; the previous DPI context is restored before returning.
    unsafe {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let rect = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );
        if !previous.0.is_null() {
            SetThreadDpiAwarenessContext(previous);
        }
        rect
    }
}

/// Maps a pixel to `SendInput`'s 0..=65535 range, where 0 is the first and
/// 65535 the last pixel of `[origin, origin + size)`. Rounded up, so
/// Windows' mapping back (`n × (size − 1) / 65535`) lands on the same pixel
/// whether it rounds down or to nearest (measured: the "÷ 65536" reading of
/// the docs is one pixel short in the middle of the desktop).
fn normalise(pos: i32, origin: i32, size: i32) -> i32 {
    let last = i64::from(size.max(1)) - 1;
    if last == 0 {
        return 0;
    }
    let offset = (i64::from(pos) - i64::from(origin)).clamp(0, last);
    ((offset * 65535 + last - 1) / last) as i32
}

/// Injects input with `SendInput`.
#[derive(Debug, Default)]
pub struct SendInputSink;

impl SendInputSink {
    /// A sink for this session.
    pub fn new() -> Self {
        SendInputSink
    }

    fn send(&self, input: INPUT) -> Result<(), InputError> {
        self.send_all(&[input])
    }

    fn send_all(&self, inputs: &[INPUT]) -> Result<(), InputError> {
        // SAFETY: fully initialised INPUTs; the size argument matches them.
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err(InputError::Os(format!(
                "SendInput: {}",
                windows::core::Error::from_thread()
            )))
        }
    }

    fn mouse(
        &self,
        dx: i32,
        dy: i32,
        data: u32,
        flags: MOUSE_EVENT_FLAGS,
    ) -> Result<(), InputError> {
        self.send(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTED_MARKER,
                },
            },
        })
    }

    fn keyboard(
        &self,
        vk: VIRTUAL_KEY,
        scan: u16,
        flags: KEYBD_EVENT_FLAGS,
    ) -> Result<(), InputError> {
        self.send(keyboard_input(vk, scan, flags))
    }
}

fn keyboard_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_MARKER,
            },
        },
    }
}

/// The inputs that type `text`: a Unicode press and release per UTF-16 unit.
fn text_inputs(text: &str) -> Vec<INPUT> {
    text.encode_utf16()
        .flat_map(|unit| {
            [
                keyboard_input(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE),
                keyboard_input(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
            ]
        })
        .collect()
}

/// Whether a toggle key (Caps/Num/Scroll Lock) is on.
fn toggled(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: plain call; bit 0 is the toggle state.
    unsafe { GetKeyState(vk.0 as i32) & 1 != 0 }
}

/// This machine's Caps/Num/Scroll Lock states, as the calling thread sees
/// them (for a window thread: as of the message being handled).
pub fn lock_state() -> Modifiers {
    let mut m = Modifiers::NONE;
    for (vk, lock) in [
        (VK_CAPITAL, Modifiers::CAPS_LOCK),
        (VK_NUMLOCK, Modifiers::NUM_LOCK),
        (VK_SCROLL, Modifiers::SCROLL_LOCK),
    ] {
        if toggled(vk) {
            m |= lock;
        }
    }
    m
}

impl InputSink for SendInputSink {
    fn move_to(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        let (left, top, width, height) = virtual_desktop();
        self.mouse(
            normalise(x, left, width),
            normalise(y, top, height),
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), InputError> {
        let (flags, data) = match (button, down) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        };
        self.mouse(0, 0, data as u32, flags)
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        if dy != 0 {
            self.mouse(0, 0, dy as u32, MOUSEEVENTF_WHEEL)?;
        }
        if dx != 0 {
            self.mouse(0, 0, dx as u32, MOUSEEVENTF_HWHEEL)?;
        }
        Ok(())
    }

    fn key(&mut self, code: KeyCode, down: bool) -> Result<(), InputError> {
        let up = if down {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        match to_pc(code) {
            Some(PcKey::Scan { code, extended }) => {
                let ext = if extended {
                    KEYEVENTF_EXTENDEDKEY
                } else {
                    KEYBD_EVENT_FLAGS(0)
                };
                self.keyboard(VIRTUAL_KEY(0), code as u16, KEYEVENTF_SCANCODE | ext | up)
            }
            // Virtual keys for media/browser keys are "extended" keys.
            Some(PcKey::Vk(vk)) => {
                let ext = if vk >= 0xA6 {
                    KEYEVENTF_EXTENDEDKEY
                } else {
                    KEYBD_EVENT_FLAGS(0)
                };
                self.keyboard(VIRTUAL_KEY(vk), 0, ext | up)
            }
            None => Err(InputError::UnknownKey(code.0)),
        }
    }

    fn move_relative(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        self.mouse(dx, dy, 0, MOUSEEVENTF_MOVE)
    }

    fn text(&mut self, text: &str) -> Result<(), InputError> {
        let inputs = text_inputs(text);
        if inputs.is_empty() {
            return Ok(());
        }
        self.send_all(&inputs)
    }

    fn action(&mut self, action: Action) -> Result<(), InputError> {
        match action {
            // SAFETY: plain call; locks the interactive session.
            Action::LockScreen => unsafe { LockWorkStation() }
                .map_err(|e| InputError::Os(format!("LockWorkStation: {e}"))),
            Action::CtrlAltDel => Err(InputError::Unsupported(
                "Ctrl+Alt+Del needs the KanoDesk service (SendSAS)",
            )),
        }
    }

    fn lock_state(&mut self) -> Option<Modifiers> {
        Some(lock_state())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalised_coordinates_map_back_to_the_same_pixel() {
        // Windows maps n back to origin + n * (size - 1) / 65535.
        for (origin, size) in [(0, 1920), (-1280, 3200), (0, 7680), (-3840, 11520), (0, 1)] {
            for pixel in [0, 1, size / 3, size / 2, size - 2, size - 1] {
                if pixel < 0 || pixel >= size {
                    continue;
                }
                let n = normalise(origin + pixel, origin, size);
                assert!((0..=65535).contains(&n));
                let exact = n as f64 * (size - 1).max(0) as f64 / 65535.0;
                for back in [exact.floor(), exact.round()] {
                    assert_eq!(
                        back as i32, pixel,
                        "origin {origin} size {size} pixel {pixel} n {n}"
                    );
                }
            }
        }
        // Outside the desktop: clamped to its edges.
        assert_eq!(normalise(-99_999, 0, 1920), 0);
        assert_eq!(normalise(99_999, 0, 1920), 65535);
    }

    #[test]
    fn text_becomes_utf16_unicode_presses() {
        let inputs = text_inputs("a€🙂");
        // a, €, and the emoji's two surrogates: press + release each.
        assert_eq!(inputs.len(), 8);
        let units: Vec<u16> = inputs
            .iter()
            .map(|i| unsafe { i.Anonymous.ki.wScan })
            .collect();
        assert_eq!(
            units,
            [0x61, 0x61, 0x20AC, 0x20AC, 0xD83D, 0xD83D, 0xDE42, 0xDE42]
        );
        let ups = inputs
            .iter()
            .filter(|i| unsafe { i.Anonymous.ki.dwFlags }.contains(KEYEVENTF_KEYUP))
            .count();
        assert_eq!(ups, 4);
        assert!(text_inputs("").is_empty());
    }

    #[test]
    fn ctrl_alt_del_says_it_needs_the_service() {
        assert!(matches!(
            SendInputSink::new().action(Action::CtrlAltDel),
            Err(InputError::Unsupported(_))
        ));
    }

    #[test]
    #[ignore = "toggles Scroll Lock twice on the real keyboard"]
    fn lock_state_follows_injected_lock_keys() {
        let mut sink = SendInputSink::new();
        let before = sink.lock_state().unwrap();
        let press = |sink: &mut SendInputSink| {
            sink.key(KeyCode::SCROLL_LOCK, true).unwrap();
            sink.key(KeyCode::SCROLL_LOCK, false).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        press(&mut sink);
        let toggled = sink.lock_state().unwrap();
        press(&mut sink);
        let restored = sink.lock_state().unwrap();
        assert_ne!(
            toggled.contains(Modifiers::SCROLL_LOCK),
            before.contains(Modifiers::SCROLL_LOCK),
            "GetKeyState saw the toggle"
        );
        assert_eq!(restored, before);
    }

    #[test]
    fn unknown_keys_are_refused_without_injecting() {
        let mut sink = SendInputSink::new();
        assert!(matches!(
            sink.key(KeyCode(0x00), true),
            Err(InputError::UnknownKey(0))
        ));
    }

    #[test]
    fn virtual_desktop_is_not_empty() {
        let (_, _, w, h) = virtual_desktop();
        assert!(w > 0 && h > 0);
    }

    // These inject real input into the desktop of whoever runs them, so they
    // are ignored by default: run `cargo test -p kanodesk-input -- --ignored`
    // on a machine where a moving cursor and an F24 press do no harm.

    #[test]
    #[ignore = "moves the real mouse pointer"]
    fn pointer_lands_on_the_requested_pixel_of_every_display() {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

        fn cursor() -> (i32, i32) {
            let mut p = POINT::default();
            // SAFETY: valid out pointer; switched to physical pixels like the sink.
            unsafe {
                let previous =
                    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                GetCursorPos(&mut p).unwrap();
                SetThreadDpiAwarenessContext(previous);
            }
            (p.x, p.y)
        }

        let start = cursor();
        let mut sink = SendInputSink::new();
        // Every monitor's corners and centre. (Corners of the virtual desktop
        // itself may lie off every monitor; Windows then moves the pointer
        // to the nearest one, so they are no test.)
        let targets = monitor_points().expect("EnumDisplayMonitors");
        assert!(!targets.is_empty());
        let mut misses = Vec::new();
        // A person moving the real mouse during the test shifts the pointer;
        // retry each target a few times and count it if any attempt is exact.
        for &(x, y) in &targets {
            let mut got = (i32::MIN, i32::MIN);
            for _ in 0..5 {
                sink.move_to(x, y).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(5));
                got = cursor();
                if got == (x, y) {
                    break;
                }
            }
            if got != (x, y) {
                misses.push(((x, y), got));
            }
        }
        sink.move_to(start.0, start.1).unwrap();
        assert!(misses.is_empty(), "wanted → got: {misses:?}");
    }

    /// Corners and centre of every monitor, in physical pixels.
    fn monitor_points() -> Result<Vec<(i32, i32)>, ()> {
        use windows::Win32::Foundation::{LPARAM, RECT};
        use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
        use windows::core::BOOL;

        unsafe extern "system" fn collect(
            _: HMONITOR,
            _: HDC,
            rect: *mut RECT,
            out: LPARAM,
        ) -> BOOL {
            // SAFETY: Windows passes a valid rect; `out` is the Vec below.
            unsafe {
                let r = &*rect;
                let v = &mut *(out.0 as *mut Vec<(i32, i32)>);
                v.push((r.left, r.top));
                v.push((r.right - 1, r.bottom - 1));
                v.push(((r.left + r.right) / 2, (r.top + r.bottom) / 2));
            }
            BOOL(1)
        }

        let mut centres: Vec<(i32, i32)> = Vec::new();
        // SAFETY: the callback only runs during this call; the Vec outlives it.
        unsafe {
            let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let ok = EnumDisplayMonitors(
                None,
                None,
                Some(collect),
                LPARAM(&mut centres as *mut _ as isize),
            );
            SetThreadDpiAwarenessContext(previous);
            if !ok.as_bool() {
                return Err(());
            }
        }
        Ok(centres)
    }

    #[test]
    #[ignore = "presses F24 on the real keyboard"]
    fn keys_go_down_and_are_released() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F24};

        let down = || unsafe { GetAsyncKeyState(VK_F24.0 as i32) } as u16 & 0x8000 != 0;
        let mut sink = SendInputSink::new();
        let f24 = KeyCode(0x73);
        sink.key(f24, true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let was_down = down();
        sink.key(f24, false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(was_down, "F24 scan code did not register as VK_F24");
        assert!(!down(), "F24 still down");
    }
}
