//! Viewer-side input: window events in, [`InputEvent`]s out (07).
//!
//! - [`ViewMapping`] maps a window position to a pixel on the remote
//!   display, through the same layout the renderer draws (letterboxed, or at
//!   the original size and scrolled; inside an area of the window, e.g.
//!   below a toolbar) and the scale between the video and the display.
//! - [`hid_usage`] turns a winit physical key into its USB HID usage (Map
//!   mode: the key's position, not its character), media keys included.
//! - [`ViewerInput`] tracks what is held and what the host allows, and turns
//!   window events (or keys from `kanodesk_input::grab`) into input events:
//!   Map or Translate mode, absolute or relative mouse, lock states, and the
//!   viewer's own shortcuts ([`ViewerCommand`]). [`ViewerInput::release_all`]
//!   produces the releases for focus loss and closing (07 §6).
//! - [`Coalescer`] passes at most one pointer move per frame interval (the
//!   latest position, or the summed relative motion) and never reorders a
//!   move after a click or key.
//! - [`send_input`] runs the input stream: coalescing, sequence numbers,
//!   and a clean end.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use kanodesk_input::{InputEvent, KeyCode, Modifiers, MouseButton, Permissions};
use kanodesk_transport::{Connection, TransportError, streams};
use tokio::sync::mpsc;
use winit::event::MouseScrollDelta;
use winit::keyboard::{KeyCode as Wk, PhysicalKey};

/// Default pointer-move interval: one move per 60 Hz frame.
pub const MOVE_INTERVAL: Duration = Duration::from_micros(16_667);
/// Keep-alive interval while keys or buttons are held (the host releases
/// them after `kanodesk_host::input::HOLD_TIMEOUT`, 3 s, of silence).
pub const KEEP_ALIVE: Duration = Duration::from_secs(1);

/// How [`send_input`] paces the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendOptions {
    /// At most one pointer move per this interval.
    pub move_interval: Duration,
    /// Keep-alive interval while anything is held.
    pub keep_alive: Duration,
}

impl Default for SendOptions {
    fn default() -> Self {
        SendOptions {
            move_interval: MOVE_INTERVAL,
            keep_alive: KEEP_ALIVE,
        }
    }
}

/// How the video fills its area (15 §2, "view styles").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewStyle {
    /// Scaled to fit the area, keeping its aspect ratio (bars around it).
    #[default]
    Fit,
    /// One video pixel per window pixel; centred when smaller than the area,
    /// scrolled when larger.
    Original,
}

/// Maps window positions to remote display pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewMapping {
    /// The area the video is shown in, physical pixels (the whole client
    /// area, or what a toolbar leaves of it).
    pub window: (u32, u32),
    /// Decoded video size.
    pub video: (u32, u32),
    /// The remote display's size in its own pixels (equal to `video` unless
    /// the host scales the stream).
    pub display: (u32, u32),
    /// Top-left corner of the area in the window.
    pub origin: (f64, f64),
    /// Fit or original size.
    pub style: ViewStyle,
    /// With [`ViewStyle::Original`]: how far the video is scrolled (clamped
    /// to what can be scrolled).
    pub scroll: (f64, f64),
}

impl ViewMapping {
    /// The video fitted into a `window` of this size at the window's corner.
    pub fn new(window: (u32, u32), video: (u32, u32), display: (u32, u32)) -> Self {
        ViewMapping {
            window,
            video,
            display,
            origin: (0.0, 0.0),
            style: ViewStyle::Fit,
            scroll: (0.0, 0.0),
        }
    }

    /// The area's rectangle in the window: (left, top, width, height).
    pub fn area(&self) -> (f64, f64, f64, f64) {
        (
            self.origin.0,
            self.origin.1,
            self.window.0 as f64,
            self.window.1 as f64,
        )
    }

    /// How far the video can scroll on each axis (0 where it fits).
    pub fn max_scroll(&self) -> (f64, f64) {
        match self.style {
            ViewStyle::Fit => (0.0, 0.0),
            ViewStyle::Original => (
                (self.video.0 as f64 - self.window.0 as f64).max(0.0),
                (self.video.1 as f64 - self.window.1 as f64).max(0.0),
            ),
        }
    }

    /// The scroll position that puts the part of the video under `pos`
    /// proportionally under the pointer: at the area's left edge the video's
    /// left edge shows, at its right edge the right edge ("auto scroll").
    pub fn scroll_toward(&self, pos: (f64, f64)) -> (f64, f64) {
        let (max_x, max_y) = self.max_scroll();
        let (left, top, w, h) = self.area();
        let frac = |p: f64, start: f64, len: f64| {
            if len <= 0.0 {
                0.0
            } else {
                ((p - start) / len).clamp(0.0, 1.0)
            }
        };
        (
            (frac(pos.0, left, w) * max_x).round(),
            (frac(pos.1, top, h) * max_y).round(),
        )
    }

    /// The video's rectangle in the window: (left, top, width, height), as
    /// the renderer draws it. With [`ViewStyle::Original`] it may extend
    /// past the area.
    pub fn video_rect(&self) -> Option<(f64, f64, f64, f64)> {
        let (ww, wh) = (self.window.0 as f64, self.window.1 as f64);
        let (vw, vh) = (self.video.0 as f64, self.video.1 as f64);
        if ww <= 0.0 || wh <= 0.0 || vw <= 0.0 || vh <= 0.0 {
            return None;
        }
        let (ox, oy) = self.origin;
        match self.style {
            ViewStyle::Fit => {
                let scale = (ww / vw).min(wh / vh);
                let (w, h) = (vw * scale, vh * scale);
                Some((ox + (ww - w) / 2.0, oy + (wh - h) / 2.0, w, h))
            }
            ViewStyle::Original => {
                let (max_x, max_y) = self.max_scroll();
                let place = |area: f64, video: f64, max: f64, scroll: f64| {
                    if video <= area {
                        (area - video) / 2.0
                    } else {
                        -scroll.clamp(0.0, max)
                    }
                };
                Some((
                    ox + place(ww, vw, max_x, self.scroll.0),
                    oy + place(wh, vh, max_y, self.scroll.1),
                    vw,
                    vh,
                ))
            }
        }
    }

    /// The part of the video that is visible: its rectangle cut to the area.
    fn visible_rect(&self) -> Option<(f64, f64, f64, f64)> {
        let (vl, vt, vw, vh) = self.video_rect()?;
        let (al, at, aw, ah) = self.area();
        let (l, t) = (vl.max(al), vt.max(at));
        let (r, b) = ((vl + vw).min(al + aw), (vt + vh).min(at + ah));
        (r > l && b > t).then_some((l, t, r - l, b - t))
    }

    /// The display pixel under window position `pos`, or `None` outside the
    /// visible video (on the bars, outside the area or off the window).
    pub fn to_display(&self, pos: (f64, f64)) -> Option<(i32, i32)> {
        let (l, t, w, h) = self.visible_rect()?;
        if !(l..l + w).contains(&pos.0) || !(t..t + h).contains(&pos.1) {
            return None;
        }
        let (left, top, w, h) = self.video_rect()?;
        Some(self.pixel((pos.0 - left) / w, (pos.1 - top) / h))
    }

    /// Like [`Self::to_display`], but positions outside the visible video
    /// are moved to its nearest edge (for drags that leave the picture).
    pub fn to_display_clamped(&self, pos: (f64, f64)) -> Option<(i32, i32)> {
        let (l, t, w, h) = self.visible_rect()?;
        // Just inside the far edges: the last visible pixel, not the next.
        let x = pos.0.clamp(l, l + w - 1e-6);
        let y = pos.1.clamp(t, t + h - 1e-6);
        let (left, top, w, h) = self.video_rect()?;
        let u = ((x - left) / w).clamp(0.0, 1.0);
        let v = ((y - top) / h).clamp(0.0, 1.0);
        Some(self.pixel(u, v))
    }

    fn pixel(&self, u: f64, v: f64) -> (i32, i32) {
        let (dw, dh) = (self.display.0.max(1), self.display.1.max(1));
        let x = ((u * dw as f64) as i64).clamp(0, dw as i64 - 1);
        let y = ((v * dh as f64) as i64).clamp(0, dh as i64 - 1);
        (x as i32, y as i32)
    }
}

/// The USB HID usage (keyboard page) of a physical key, if it has one.
pub fn hid_usage(key: PhysicalKey) -> Option<KeyCode> {
    let PhysicalKey::Code(code) = key else {
        return None;
    };
    let usage = match code {
        Wk::KeyA => 0x04,
        Wk::KeyB => 0x05,
        Wk::KeyC => 0x06,
        Wk::KeyD => 0x07,
        Wk::KeyE => 0x08,
        Wk::KeyF => 0x09,
        Wk::KeyG => 0x0A,
        Wk::KeyH => 0x0B,
        Wk::KeyI => 0x0C,
        Wk::KeyJ => 0x0D,
        Wk::KeyK => 0x0E,
        Wk::KeyL => 0x0F,
        Wk::KeyM => 0x10,
        Wk::KeyN => 0x11,
        Wk::KeyO => 0x12,
        Wk::KeyP => 0x13,
        Wk::KeyQ => 0x14,
        Wk::KeyR => 0x15,
        Wk::KeyS => 0x16,
        Wk::KeyT => 0x17,
        Wk::KeyU => 0x18,
        Wk::KeyV => 0x19,
        Wk::KeyW => 0x1A,
        Wk::KeyX => 0x1B,
        Wk::KeyY => 0x1C,
        Wk::KeyZ => 0x1D,
        Wk::Digit1 => 0x1E,
        Wk::Digit2 => 0x1F,
        Wk::Digit3 => 0x20,
        Wk::Digit4 => 0x21,
        Wk::Digit5 => 0x22,
        Wk::Digit6 => 0x23,
        Wk::Digit7 => 0x24,
        Wk::Digit8 => 0x25,
        Wk::Digit9 => 0x26,
        Wk::Digit0 => 0x27,
        Wk::Enter => 0x28,
        Wk::Escape => 0x29,
        Wk::Backspace => 0x2A,
        Wk::Tab => 0x2B,
        Wk::Space => 0x2C,
        Wk::Minus => 0x2D,
        Wk::Equal => 0x2E,
        Wk::BracketLeft => 0x2F,
        Wk::BracketRight => 0x30,
        Wk::Backslash => 0x31,
        Wk::Semicolon => 0x33,
        Wk::Quote => 0x34,
        Wk::Backquote => 0x35,
        Wk::Comma => 0x36,
        Wk::Period => 0x37,
        Wk::Slash => 0x38,
        Wk::CapsLock => 0x39,
        Wk::F1 => 0x3A,
        Wk::F2 => 0x3B,
        Wk::F3 => 0x3C,
        Wk::F4 => 0x3D,
        Wk::F5 => 0x3E,
        Wk::F6 => 0x3F,
        Wk::F7 => 0x40,
        Wk::F8 => 0x41,
        Wk::F9 => 0x42,
        Wk::F10 => 0x43,
        Wk::F11 => 0x44,
        Wk::F12 => 0x45,
        Wk::PrintScreen => 0x46,
        Wk::ScrollLock => 0x47,
        Wk::Pause => 0x48,
        Wk::Insert => 0x49,
        Wk::Home => 0x4A,
        Wk::PageUp => 0x4B,
        Wk::Delete => 0x4C,
        Wk::End => 0x4D,
        Wk::PageDown => 0x4E,
        Wk::ArrowRight => 0x4F,
        Wk::ArrowLeft => 0x50,
        Wk::ArrowDown => 0x51,
        Wk::ArrowUp => 0x52,
        Wk::NumLock => 0x53,
        Wk::NumpadDivide => 0x54,
        Wk::NumpadMultiply => 0x55,
        Wk::NumpadSubtract => 0x56,
        Wk::NumpadAdd => 0x57,
        Wk::NumpadEnter => 0x58,
        Wk::Numpad1 => 0x59,
        Wk::Numpad2 => 0x5A,
        Wk::Numpad3 => 0x5B,
        Wk::Numpad4 => 0x5C,
        Wk::Numpad5 => 0x5D,
        Wk::Numpad6 => 0x5E,
        Wk::Numpad7 => 0x5F,
        Wk::Numpad8 => 0x60,
        Wk::Numpad9 => 0x61,
        Wk::Numpad0 => 0x62,
        Wk::NumpadDecimal => 0x63,
        Wk::IntlBackslash => 0x64,
        Wk::ContextMenu => 0x65,
        Wk::Power => 0x66,
        Wk::NumpadEqual => 0x67,
        Wk::F13 => 0x68,
        Wk::F14 => 0x69,
        Wk::F15 => 0x6A,
        Wk::F16 => 0x6B,
        Wk::F17 => 0x6C,
        Wk::F18 => 0x6D,
        Wk::F19 => 0x6E,
        Wk::F20 => 0x6F,
        Wk::F21 => 0x70,
        Wk::F22 => 0x71,
        Wk::F23 => 0x72,
        Wk::F24 => 0x73,
        Wk::NumpadComma => 0x85,
        Wk::IntlRo => 0x87,
        Wk::KanaMode => 0x88,
        Wk::IntlYen => 0x89,
        Wk::Convert => 0x8A,
        Wk::NonConvert => 0x8B,
        Wk::Lang1 => 0x90,
        Wk::Lang2 => 0x91,
        // Consumer page (0x0C): media and browser keys.
        Wk::AudioVolumeUp => 0x000C_00E9,
        Wk::AudioVolumeDown => 0x000C_00EA,
        Wk::AudioVolumeMute => 0x000C_00E2,
        Wk::MediaPlayPause => 0x000C_00CD,
        Wk::MediaStop => 0x000C_00B7,
        Wk::MediaTrackNext => 0x000C_00B5,
        Wk::MediaTrackPrevious => 0x000C_00B6,
        Wk::BrowserBack => 0x000C_0224,
        Wk::BrowserForward => 0x000C_0225,
        Wk::BrowserRefresh => 0x000C_0227,
        Wk::BrowserStop => 0x000C_0226,
        Wk::BrowserSearch => 0x000C_0221,
        Wk::BrowserFavorites => 0x000C_022A,
        Wk::BrowserHome => 0x000C_0223,
        Wk::LaunchMail => 0x000C_018A,
        Wk::MediaSelect => 0x000C_0183,
        Wk::LaunchApp1 => 0x000C_0194,
        Wk::LaunchApp2 => 0x000C_0192,
        Wk::ControlLeft => 0xE0,
        Wk::ShiftLeft => 0xE1,
        Wk::AltLeft => 0xE2,
        Wk::SuperLeft => 0xE3,
        Wk::ControlRight => 0xE4,
        Wk::ShiftRight => 0xE5,
        Wk::AltRight => 0xE6,
        Wk::SuperRight => 0xE7,
        _ => return None,
    };
    Some(KeyCode(usage))
}

fn button(b: winit::event::MouseButton) -> Option<MouseButton> {
    use winit::event::MouseButton as W;
    match b {
        W::Left => Some(MouseButton::Left),
        W::Right => Some(MouseButton::Right),
        W::Middle => Some(MouseButton::Middle),
        W::Back => Some(MouseButton::Back),
        W::Forward => Some(MouseButton::Forward),
        W::Other(_) => None,
    }
}

/// How keys are sent (07 §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardMode {
    /// Physical key positions; the host's layout decides the characters.
    #[default]
    Map,
    /// Characters as the viewer's layout produces them; shortcuts and keys
    /// without a character still go as positions.
    Translate,
}

/// A viewer shortcut, handled by the viewer instead of being sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerCommand {
    /// Ctrl+Alt+End: send Ctrl+Alt+Del to the host.
    CtrlAltDel,
    /// Ctrl+Alt+Shift+M: switch relative (pointer-locked) mouse on or off.
    ToggleRelativeMouse,
    /// Ctrl+Alt+Shift+K: switch between Map and Translate mode.
    ToggleKeyboardMode,
    /// Ctrl+Alt+Shift+F: enter or leave fullscreen.
    ToggleFullscreen,
}

/// One key from the window or the keyboard grab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput<'a> {
    /// The physical key.
    pub code: KeyCode,
    /// Pressed (also auto-repeat) or released.
    pub down: bool,
    /// The text the viewer's layout produces for this press, if known
    /// (only used in Translate mode).
    pub text: Option<&'a str>,
    /// The viewer's lock states, if known (sent so the host can match them).
    pub locks: Option<Modifiers>,
}

/// What one key produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyOutcome {
    /// Events to send.
    pub events: Vec<InputEvent>,
    /// A viewer shortcut to carry out instead.
    pub command: Option<ViewerCommand>,
}

const END: KeyCode = KeyCode(0x4D);
const KEY_M: KeyCode = KeyCode(0x10);
const KEY_K: KeyCode = KeyCode(0x0E);
const KEY_F: KeyCode = KeyCode(0x09);

/// Turns window events into input events; see the module docs.
#[derive(Debug)]
pub struct ViewerInput {
    mapping: Option<ViewMapping>,
    display: u32,
    allowed: Permissions,
    mode: KeyboardMode,
    relative: bool,
    /// Keys sent to the host as pressed.
    keys: BTreeSet<KeyCode>,
    /// Keys physically held here (for shortcuts and Translate decisions).
    local_keys: BTreeSet<KeyCode>,
    /// Keys whose press went out as text; their release is not sent.
    text_keys: BTreeSet<KeyCode>,
    buttons: BTreeSet<MouseButton>,
    cursor: Option<(f64, f64)>,
}

impl Default for ViewerInput {
    fn default() -> Self {
        Self::new()
    }
}

fn classes(keys: &BTreeSet<KeyCode>) -> Modifiers {
    keys.iter()
        .filter_map(|k| k.modifier())
        .fold(Modifiers::NONE, |a, b| a | b)
}

impl ViewerInput {
    /// Nothing held, nothing allowed until the host says so; Map mode.
    pub fn new() -> Self {
        ViewerInput {
            mapping: None,
            display: 0,
            allowed: Permissions::NONE,
            mode: KeyboardMode::Map,
            relative: false,
            keys: BTreeSet::new(),
            local_keys: BTreeSet::new(),
            text_keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            cursor: None,
        }
    }

    /// Sets the geometry and which host display the video shows.
    pub fn set_view(&mut self, display: u32, mapping: ViewMapping) {
        self.display = display;
        self.mapping = Some(mapping);
    }

    /// What the host allows.
    pub fn permissions(&self) -> Permissions {
        self.allowed
    }

    /// The host changed what it allows. Whatever is no longer allowed is
    /// forgotten here (the host releases it on its side).
    pub fn set_permissions(&mut self, allowed: Permissions) {
        self.allowed = allowed;
        if !allowed.keyboard {
            self.keys.clear();
            self.text_keys.clear();
        }
        if !allowed.mouse {
            self.buttons.clear();
        }
    }

    /// The keyboard mode.
    pub fn keyboard_mode(&self) -> KeyboardMode {
        self.mode
    }

    /// Switches the keyboard mode.
    pub fn set_keyboard_mode(&mut self, mode: KeyboardMode) {
        self.mode = mode;
    }

    /// Whether relative (pointer-locked) mouse mode is on.
    pub fn relative_mouse(&self) -> bool {
        self.relative
    }

    /// Switches relative mouse mode. The window should confine and hide its
    /// pointer while it is on and feed [`Self::mouse_motion`].
    pub fn set_relative_mouse(&mut self, on: bool) {
        self.relative = on;
    }

    /// The pointer moved to `pos` (window physical pixels). While a button is
    /// held, positions outside the video are clamped to its edge (drags).
    /// Ignored in relative mode.
    pub fn cursor_moved(&mut self, pos: (f64, f64)) -> Option<InputEvent> {
        self.cursor = Some(pos);
        if !self.allowed.mouse || self.relative {
            return None;
        }
        let mapping = self.mapping?;
        let (x, y) = if self.buttons.is_empty() {
            mapping.to_display(pos)?
        } else {
            mapping.to_display_clamped(pos)?
        };
        Some(InputEvent::MouseMove {
            display: self.display,
            x,
            y,
        })
    }

    /// Raw mouse motion (device counts); sent only in relative mode.
    pub fn mouse_motion(&mut self, dx: f64, dy: f64) -> Option<InputEvent> {
        if !self.allowed.mouse || !self.relative {
            return None;
        }
        let (dx, dy) = (dx.round() as i32, dy.round() as i32);
        (dx != 0 || dy != 0).then_some(InputEvent::MouseMoveRelative { dx, dy })
    }

    /// The pointer left the window.
    pub fn cursor_left(&mut self) {
        self.cursor = None;
    }

    fn over_video(&self) -> bool {
        if self.relative {
            return true;
        }
        match (self.cursor, self.mapping) {
            (Some(pos), Some(m)) => m.to_display(pos).is_some(),
            _ => false,
        }
    }

    /// A button changed. Presses count only over the video (always in
    /// relative mode); a release counts only if the press was sent.
    pub fn mouse_button(
        &mut self,
        b: winit::event::MouseButton,
        pressed: bool,
    ) -> Option<InputEvent> {
        let button = button(b)?;
        if !self.allowed.mouse {
            return None;
        }
        if pressed {
            if !self.over_video() || !self.buttons.insert(button) {
                return None;
            }
        } else if !self.buttons.remove(&button) {
            return None;
        }
        Some(InputEvent::MouseButton {
            button,
            down: pressed,
        })
    }

    /// The wheel turned (only over the video).
    pub fn wheel(&mut self, delta: MouseScrollDelta) -> Option<InputEvent> {
        if !self.allowed.mouse || !self.over_video() {
            return None;
        }
        // winit: +y = away from the user (scroll up), +x = content moves
        // right, i.e. the inverse of a right tilt. The wire uses Windows'
        // signs: +dy up, +dx right, 120 per notch.
        let (dx, dy) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (-x as f64 * 120.0, y as f64 * 120.0),
            MouseScrollDelta::PixelDelta(p) => (-p.x, p.y),
        };
        let (dx, dy) = (dx.round() as i32, dy.round() as i32);
        (dx != 0 || dy != 0).then_some(InputEvent::Wheel { dx, dy })
    }

    /// A winit key event (when no keyboard grab delivers keys).
    pub fn key(
        &mut self,
        key: PhysicalKey,
        pressed: bool,
        text: Option<&str>,
        locks: Option<Modifiers>,
    ) -> KeyOutcome {
        match hid_usage(key) {
            Some(code) => self.key_input(KeyInput {
                code,
                down: pressed,
                text,
                locks,
            }),
            None => KeyOutcome::default(),
        }
    }

    /// One key, from the window or the keyboard grab.
    ///
    /// - Lock keys are not sent: they act locally, and every key carries the
    ///   lock states so the host follows.
    /// - Viewer shortcuts are recognised on press and not sent.
    /// - In Translate mode a press that produces printable text (without
    ///   Ctrl, Alt or Win, or with AltGr) goes as text; its release is not sent.
    /// - Everything else goes as a key; repeats as further presses (the host
    ///   OS does not auto-repeat injected keys), releases only for sent presses.
    pub fn key_input(&mut self, k: KeyInput<'_>) -> KeyOutcome {
        if k.code.lock().is_some() {
            return KeyOutcome::default();
        }
        if k.down {
            self.local_keys.insert(k.code);
            if let Some(command) = self.shortcut(k.code) {
                return KeyOutcome {
                    events: Vec::new(),
                    command: Some(command),
                };
            }
        } else {
            self.local_keys.remove(&k.code);
        }
        if !self.allowed.keyboard {
            return KeyOutcome::default();
        }
        if !k.down && self.text_keys.remove(&k.code) {
            return KeyOutcome::default();
        }
        if k.down
            && self.mode == KeyboardMode::Translate
            && k.code.modifier().is_none()
            && let Some(text) = k.text.filter(|t| printable(t))
            && text_allowed(classes(&self.local_keys))
        {
            self.text_keys.insert(k.code);
            return KeyOutcome {
                events: vec![InputEvent::Text(text.to_owned())],
                command: None,
            };
        }
        if k.down {
            self.keys.insert(k.code);
        } else if !self.keys.remove(&k.code) {
            return KeyOutcome::default();
        }
        KeyOutcome {
            events: vec![InputEvent::Key {
                code: k.code,
                down: k.down,
                modifiers: classes(&self.keys),
                locks: k.locks,
            }],
            command: None,
        }
    }

    fn shortcut(&self, code: KeyCode) -> Option<ViewerCommand> {
        let held = classes(&self.local_keys);
        if !held.contains(Modifiers::CONTROL | Modifiers::ALT) || held.contains(Modifiers::META) {
            return None;
        }
        let shift = held.contains(Modifiers::SHIFT);
        match code {
            END if !shift => Some(ViewerCommand::CtrlAltDel),
            KEY_M if shift => Some(ViewerCommand::ToggleRelativeMouse),
            KEY_K if shift => Some(ViewerCommand::ToggleKeyboardMode),
            KEY_F if shift => Some(ViewerCommand::ToggleFullscreen),
            _ => None,
        }
    }

    /// Releases for everything held (focus lost, window closing), keys
    /// before buttons and modifiers last.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        self.local_keys.clear();
        self.text_keys.clear();
        let mut keys: Vec<KeyCode> = std::mem::take(&mut self.keys).into_iter().collect();
        keys.sort_by_key(|k| k.modifier().is_some());
        let mut still: BTreeSet<KeyCode> = keys.iter().copied().collect();
        let mut out = Vec::new();
        for code in keys {
            still.remove(&code);
            // Each release reports what is still held after it.
            out.push(InputEvent::key(code, false, classes(&still)));
        }
        for button in std::mem::take(&mut self.buttons) {
            out.push(InputEvent::MouseButton {
                button,
                down: false,
            });
        }
        out
    }

    /// Whether anything is held.
    pub fn anything_held(&self) -> bool {
        !self.keys.is_empty() || !self.buttons.is_empty()
    }
}

/// Text worth typing: non-empty, no control characters.
fn printable(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(char::is_control)
}

/// Whether a character typed with these modifiers is text rather than a
/// shortcut: no Ctrl/Alt/Win, or Ctrl+Alt together (AltGr on Windows).
fn text_allowed(held: Modifiers) -> bool {
    if held.contains(Modifiers::META) {
        return false;
    }
    let ctrl = held.contains(Modifiers::CONTROL);
    let alt = held.contains(Modifiers::ALT);
    ctrl == alt
}

/// Lets through at most one pointer move per interval: the latest absolute
/// position, or the sum of relative motion. Any other event first flushes a
/// pending move, so order is kept.
#[derive(Debug)]
pub struct Coalescer {
    interval: Duration,
    pending: Option<InputEvent>,
    last_move: Option<Instant>,
}

fn is_move(e: &InputEvent) -> bool {
    matches!(
        e,
        InputEvent::MouseMove { .. } | InputEvent::MouseMoveRelative { .. }
    )
}

impl Coalescer {
    /// A coalescer for one move per `interval`.
    pub fn new(interval: Duration) -> Self {
        Coalescer {
            interval,
            pending: None,
            last_move: None,
        }
    }

    /// Takes an event at `now`; returns what to send now.
    pub fn push(&mut self, event: InputEvent, now: Instant) -> Vec<InputEvent> {
        let mut out = Vec::new();
        if !is_move(&event) {
            if let Some(pending) = self.pending.take() {
                self.last_move = Some(now);
                out.push(pending);
            }
            out.push(event);
            return out;
        }
        let merged = match (self.pending.take(), event) {
            (
                Some(InputEvent::MouseMoveRelative { dx: a, dy: b }),
                InputEvent::MouseMoveRelative { dx: c, dy: d },
            ) => InputEvent::MouseMoveRelative {
                dx: a.saturating_add(c),
                dy: b.saturating_add(d),
            },
            (Some(InputEvent::MouseMove { .. }), e @ InputEvent::MouseMove { .. }) => e,
            // A switch between absolute and relative: keep both, in order.
            (Some(previous), e) => {
                out.push(previous);
                e
            }
            (None, e) => e,
        };
        let due = self
            .last_move
            .is_none_or(|t| now.saturating_duration_since(t) >= self.interval);
        if due {
            self.last_move = Some(now);
            out.push(merged);
        } else {
            self.pending = Some(merged);
        }
        out
    }

    /// When the pending move is due, if there is one.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref()?;
        Some(self.last_move? + self.interval)
    }

    /// The pending move, if it is due at `now`.
    pub fn poll(&mut self, now: Instant) -> Option<InputEvent> {
        if self.deadline().is_some_and(|d| now >= d) {
            self.last_move = Some(now);
            return self.pending.take();
        }
        None
    }

    /// The pending move regardless of timing (when the stream ends).
    pub fn flush(&mut self) -> Option<InputEvent> {
        self.pending.take()
    }
}

/// Keys and buttons held according to the events sent.
#[derive(Debug, Default)]
struct Held {
    keys: BTreeSet<KeyCode>,
    buttons: BTreeSet<MouseButton>,
}

impl Held {
    fn track(&mut self, e: &InputEvent) {
        match *e {
            InputEvent::Key { code, down, .. } => {
                if down {
                    self.keys.insert(code);
                } else {
                    self.keys.remove(&code);
                }
            }
            InputEvent::MouseButton { button, down } => {
                if down {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            _ => {}
        }
    }

    fn any(&self) -> bool {
        !self.keys.is_empty() || !self.buttons.is_empty()
    }
}

/// Opens the input stream on `conn` and sends `events` until the channel
/// closes: moves coalesced per [`SendOptions::move_interval`], a keep-alive
/// every [`SendOptions::keep_alive`] while anything is held. Then finishes
/// the stream and returns how many messages were sent.
pub async fn send_input(
    conn: &Connection,
    mut events: mpsc::UnboundedReceiver<InputEvent>,
    options: SendOptions,
) -> Result<u64, TransportError> {
    let mut writer = streams::open_input(conn).await?;
    let mut coalescer = Coalescer::new(options.move_interval);
    let mut held = Held::default();
    let mut seq = 0u64;
    let mut last_sent = Instant::now();
    loop {
        let deadline = coalescer.deadline();
        let keep_alive_at = held.any().then(|| last_sent + options.keep_alive);
        let batch = tokio::select! {
            event = events.recv() => match event {
                Some(e) => coalescer.push(e, Instant::now()),
                None => break,
            },
            _ = sleep_until(deadline), if deadline.is_some() => {
                coalescer.poll(Instant::now()).into_iter().collect()
            }
            _ = sleep_until(keep_alive_at), if keep_alive_at.is_some() => {
                writer.send(&kanodesk_input::event::keep_alive(seq)).await?;
                seq += 1;
                last_sent = Instant::now();
                continue;
            }
        };
        for e in batch {
            held.track(&e);
            writer.send(&e.to_proto(seq)).await?;
            seq += 1;
            last_sent = Instant::now();
        }
    }
    if let Some(e) = coalescer.flush() {
        writer.send(&e.to_proto(seq)).await?;
        seq += 1;
    }
    let _ = writer.into_inner().finish();
    Ok(seq)
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;
    use winit::event::MouseButton as W;

    fn mapping(window: (u32, u32), video: (u32, u32)) -> ViewMapping {
        ViewMapping::new(window, video, video)
    }

    #[test]
    fn same_size_window_maps_one_to_one() {
        let m = mapping((1920, 1080), (1920, 1080));
        assert_eq!(m.to_display((0.0, 0.0)), Some((0, 0)));
        assert_eq!(m.to_display((1919.5, 1079.5)), Some((1919, 1079)));
        assert_eq!(m.to_display((960.0, 540.0)), Some((960, 540)));
        assert_eq!(m.to_display((1920.0, 5.0)), None);
    }

    #[test]
    fn letterbox_and_pillarbox_bars_are_outside() {
        // 16:9 video in a 4:3 window: bars above and below.
        let m = mapping((1200, 900), (1920, 1080));
        let (left, top, w, h) = m.video_rect().unwrap();
        assert_eq!((left, w), (0.0, 1200.0));
        assert!((top - 112.5).abs() < 1e-9 && (h - 675.0).abs() < 1e-9);
        assert_eq!(m.to_display((600.0, 50.0)), None, "in the top bar");
        assert_eq!(m.to_display((600.0, 450.0)), Some((960, 540)));
        assert_eq!(m.to_display((0.0, 112.5)), Some((0, 0)));
        // Each window pixel covers 1.6 display pixels.
        assert_eq!(m.to_display((1199.9, 787.4)), Some((1919, 1079)));
        // Wide window: bars left and right.
        let m = mapping((3000, 1080), (1920, 1080));
        assert_eq!(m.to_display((500.0, 10.0)), None);
        assert_eq!(m.to_display((540.0, 0.0)), Some((0, 0)));
        // Clamped: off to the left and below → nearest edge.
        assert_eq!(m.to_display_clamped((-50.0, 5000.0)), Some((0, 1079)));
    }

    #[test]
    fn video_scaled_down_by_the_host_maps_to_full_display_pixels() {
        let m = ViewMapping::new((1280, 720), (1280, 720), (2560, 1440));
        assert_eq!(m.to_display((640.0, 360.0)), Some((1280, 720)));
        assert_eq!(m.to_display((1279.9, 719.9)), Some((2559, 1439)));
        assert_eq!(mapping((0, 0), (10, 10)).to_display((0.0, 0.0)), None);
    }

    #[test]
    fn an_area_below_a_toolbar_shifts_the_video() {
        // 1000x540 area starting 40 px down; 16:9 video fills it exactly
        // with bars left and right.
        let mut m = mapping((1000, 540), (1920, 1080));
        m.origin = (0.0, 40.0);
        let (left, top, w, h) = m.video_rect().unwrap();
        assert_eq!((left, top, w, h), (20.0, 40.0, 960.0, 540.0));
        assert_eq!(m.to_display((20.0, 39.0)), None, "on the toolbar");
        assert_eq!(m.to_display((20.0, 40.0)), Some((0, 0)));
        assert_eq!(m.to_display((500.0, 310.0)), Some((960, 540)));
        // A drag up onto the toolbar stays on the video's top edge.
        assert_eq!(m.to_display_clamped((500.0, 0.0)), Some((960, 0)));
    }

    #[test]
    fn original_size_centres_small_video_and_scrolls_large_video() {
        // Small video: centred, 1:1, no scrolling.
        let mut m = mapping((1000, 800), (800, 600));
        m.style = ViewStyle::Original;
        assert_eq!(m.max_scroll(), (0.0, 0.0));
        assert_eq!(m.video_rect(), Some((100.0, 100.0, 800.0, 600.0)));
        assert_eq!(m.to_display((100.0, 100.0)), Some((0, 0)));
        assert_eq!(m.to_display((99.0, 100.0)), None);
        assert_eq!(m.to_display((899.5, 699.5)), Some((799, 599)));

        // Large video in a smaller area below a toolbar.
        let mut m = mapping((1000, 500), (1920, 1080));
        m.style = ViewStyle::Original;
        m.origin = (0.0, 30.0);
        assert_eq!(m.max_scroll(), (920.0, 580.0));
        assert_eq!(m.video_rect(), Some((0.0, 30.0, 1920.0, 1080.0)));
        assert_eq!(m.to_display((10.0, 40.0)), Some((10, 10)));
        assert_eq!(m.to_display((1500.0, 40.0)), None, "past the area");
        m.scroll = (920.0, 580.0);
        assert_eq!(m.to_display((999.5, 529.5)), Some((1919, 1079)));
        // Scrolling is clamped.
        m.scroll = (5000.0, -10.0);
        assert_eq!(m.video_rect(), Some((-920.0, 30.0, 1920.0, 1080.0)));
        // Auto scroll follows the pointer across the area.
        assert_eq!(m.scroll_toward((0.0, 30.0)), (0.0, 0.0));
        assert_eq!(m.scroll_toward((500.0, 280.0)), (460.0, 290.0));
        assert_eq!(m.scroll_toward((2000.0, 900.0)), (920.0, 580.0));
        // Drags clamp to the visible part, not the whole video.
        m.scroll = (0.0, 0.0);
        assert_eq!(m.to_display_clamped((3000.0, 3000.0)), Some((999, 499)));
        // Fit never scrolls.
        let fit = mapping((1000, 500), (1920, 1080));
        assert_eq!(fit.max_scroll(), (0.0, 0.0));
        assert_eq!(fit.scroll_toward((900.0, 400.0)), (0.0, 0.0));
    }

    #[test]
    fn keys_map_to_hid_usages() {
        let k = |c| hid_usage(PhysicalKey::Code(c)).map(|k| k.0);
        assert_eq!(k(Wk::KeyA), Some(0x04));
        assert_eq!(k(Wk::KeyZ), Some(0x1D));
        assert_eq!(k(Wk::Digit0), Some(0x27));
        assert_eq!(k(Wk::F24), Some(0x73));
        assert_eq!(k(Wk::ArrowUp), Some(0x52));
        assert_eq!(k(Wk::NumpadEnter), Some(0x58));
        assert_eq!(k(Wk::SuperRight), Some(0xE7));
        assert_eq!(k(Wk::AudioVolumeUp), Some(0x000C_00E9));
        assert_eq!(k(Wk::MediaPlayPause), Some(0x000C_00CD));
        assert_eq!(k(Wk::BrowserBack), Some(0x000C_0224));
        assert_eq!(k(Wk::Lang1), Some(0x90));
        assert_eq!(k(Wk::Sleep), None, "deliberately not mapped");
        assert_eq!(
            hid_usage(PhysicalKey::Unidentified(
                winit::keyboard::NativeKeyCode::Unidentified
            )),
            None
        );
        // Every mapped key can be pressed on a Windows host.
        for code in [
            Wk::AudioVolumeUp,
            Wk::AudioVolumeDown,
            Wk::AudioVolumeMute,
            Wk::MediaPlayPause,
            Wk::MediaStop,
            Wk::MediaTrackNext,
            Wk::MediaTrackPrevious,
            Wk::BrowserBack,
            Wk::BrowserForward,
            Wk::BrowserRefresh,
            Wk::BrowserStop,
            Wk::BrowserSearch,
            Wk::BrowserFavorites,
            Wk::BrowserHome,
            Wk::LaunchMail,
            Wk::MediaSelect,
            Wk::LaunchApp1,
            Wk::LaunchApp2,
            Wk::Lang1,
            Wk::Lang2,
        ] {
            let key = hid_usage(PhysicalKey::Code(code)).unwrap();
            assert!(kanodesk_input::scancode::to_pc(key).is_some(), "{code:?}");
        }
    }

    fn viewer() -> ViewerInput {
        let mut v = ViewerInput::new();
        v.set_permissions(Permissions::ALL);
        v.set_view(2, mapping((1920, 1080), (1920, 1080)));
        v
    }

    fn press(v: &mut ViewerInput, code: u32, down: bool) -> KeyOutcome {
        v.key_input(KeyInput {
            code: KeyCode(code),
            down,
            text: None,
            locks: None,
        })
    }

    fn typed(v: &mut ViewerInput, code: u32, text: &str) -> KeyOutcome {
        v.key_input(KeyInput {
            code: KeyCode(code),
            down: true,
            text: Some(text),
            locks: None,
        })
    }

    #[test]
    fn nothing_is_sent_until_the_host_allows_it() {
        let mut v = ViewerInput::new();
        v.set_view(0, mapping((100, 100), (100, 100)));
        assert_eq!(v.cursor_moved((10.0, 10.0)), None);
        assert!(press(&mut v, 0x04, true).events.is_empty());
        v.set_permissions(Permissions {
            keyboard: true,
            mouse: false,
        });
        assert_eq!(v.cursor_moved((10.0, 10.0)), None);
        assert_eq!(press(&mut v, 0x04, true).events.len(), 1);
        // Revoking forgets what was held: no releases to send later.
        v.set_permissions(Permissions::NONE);
        assert!(!v.anything_held());
        assert!(v.release_all().is_empty());
    }

    #[test]
    fn clicks_count_over_the_video_and_drags_are_clamped() {
        let mut v = ViewerInput::new();
        v.set_permissions(Permissions::ALL);
        v.set_view(1, mapping((1200, 900), (1920, 1080)));
        // In the letterbox bar: no move, no click.
        assert_eq!(v.cursor_moved((600.0, 20.0)), None);
        assert_eq!(v.mouse_button(W::Left, true), None);
        assert_eq!(
            v.mouse_button(W::Left, false),
            None,
            "release of an unsent press"
        );
        // Over the video: move and press.
        assert_eq!(
            v.cursor_moved((600.0, 450.0)),
            Some(InputEvent::MouseMove {
                display: 1,
                x: 960,
                y: 540
            })
        );
        assert!(v.mouse_button(W::Left, true).is_some());
        // Dragging into the bar keeps sending, clamped to the top edge.
        assert_eq!(
            v.cursor_moved((600.0, 20.0)),
            Some(InputEvent::MouseMove {
                display: 1,
                x: 960,
                y: 0
            })
        );
        // The release counts even outside the video.
        assert_eq!(
            v.mouse_button(W::Left, false),
            Some(InputEvent::MouseButton {
                button: MouseButton::Left,
                down: false
            })
        );
        assert_eq!(v.mouse_button(W::Other(9), true), None);
    }

    #[test]
    fn wheel_uses_windows_units_and_signs() {
        let mut v = viewer();
        v.cursor_moved((10.0, 10.0));
        assert_eq!(
            v.wheel(MouseScrollDelta::LineDelta(0.0, 1.0)),
            Some(InputEvent::Wheel { dx: 0, dy: 120 })
        );
        // winit's horizontal sign is inverted relative to a right tilt.
        assert_eq!(
            v.wheel(MouseScrollDelta::LineDelta(-0.5, 0.0)),
            Some(InputEvent::Wheel { dx: 60, dy: 0 })
        );
        assert_eq!(
            v.wheel(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                0.0, -40.0
            ))),
            Some(InputEvent::Wheel { dx: 0, dy: -40 })
        );
        assert_eq!(v.wheel(MouseScrollDelta::LineDelta(0.0, 0.001)), None);
        v.cursor_left();
        assert_eq!(v.wheel(MouseScrollDelta::LineDelta(0.0, 1.0)), None);
    }

    fn mods(o: &KeyOutcome) -> Modifiers {
        match o.events[0] {
            InputEvent::Key { modifiers, .. } => modifiers,
            _ => unreachable!(),
        }
    }

    #[test]
    fn key_events_carry_the_modifiers_held_after_them_and_the_locks() {
        let mut v = viewer();
        assert_eq!(mods(&press(&mut v, 0xE0, true)), Modifiers::CONTROL);
        assert_eq!(
            mods(&press(&mut v, 0xE5, true)),
            Modifiers::CONTROL | Modifiers::SHIFT
        );
        assert_eq!(
            mods(&press(&mut v, 0x06, true)),
            Modifiers::CONTROL | Modifiers::SHIFT
        );
        // Auto-repeat is another press.
        assert!(matches!(
            press(&mut v, 0x06, true).events[0],
            InputEvent::Key { down: true, .. }
        ));
        assert_eq!(mods(&press(&mut v, 0xE0, false)), Modifiers::SHIFT);
        assert!(press(&mut v, 0xE0, false).events.is_empty());
        // Lock states ride along; lock keys themselves are not sent.
        let o = v.key_input(KeyInput {
            code: KeyCode(0x04),
            down: true,
            text: None,
            locks: Some(Modifiers::CAPS_LOCK),
        });
        assert!(matches!(
            o.events[0],
            InputEvent::Key {
                locks: Some(Modifiers::CAPS_LOCK),
                ..
            }
        ));
        assert!(press(&mut v, 0x39, true).events.is_empty());
        assert!(press(&mut v, 0x39, false).events.is_empty());
    }

    #[test]
    fn winit_keys_go_through_the_same_path() {
        let mut v = viewer();
        let o = v.key(PhysicalKey::Code(Wk::KeyA), true, None, None);
        assert_eq!(
            o.events,
            vec![InputEvent::key(KeyCode(0x04), true, Modifiers::NONE)]
        );
        assert!(v.key(PhysicalKey::Code(Wk::Sleep), true, None, None) == KeyOutcome::default());
    }

    #[test]
    fn translate_mode_types_text_and_keeps_shortcuts_as_keys() {
        let mut v = viewer();
        v.set_keyboard_mode(KeyboardMode::Translate);
        // A plain character: text, and its release is not sent.
        assert_eq!(
            typed(&mut v, 0x14, "q").events,
            vec![InputEvent::Text("q".into())]
        );
        assert!(press(&mut v, 0x14, false).events.is_empty());
        // Shift is a key; Shift+2 on this layout is text.
        assert_eq!(press(&mut v, 0xE1, true).events.len(), 1);
        assert_eq!(
            typed(&mut v, 0x1F, "\"").events,
            vec![InputEvent::Text("\"".into())]
        );
        press(&mut v, 0x1F, false);
        press(&mut v, 0xE1, false);
        // Ctrl+C: a shortcut, sent as keys (winit's text is a control char anyway).
        press(&mut v, 0xE0, true);
        let o = typed(&mut v, 0x06, "c");
        assert!(matches!(
            o.events[0],
            InputEvent::Key {
                code: KeyCode(0x06),
                down: true,
                ..
            }
        ));
        press(&mut v, 0x06, false);
        press(&mut v, 0xE0, false);
        // AltGr (Ctrl+Alt) + Q on a German layout: '@' as text.
        press(&mut v, 0xE0, true);
        press(&mut v, 0xE6, true);
        assert_eq!(
            typed(&mut v, 0x14, "@").events,
            vec![InputEvent::Text("@".into())]
        );
        // Keys without text (arrows) are keys.
        let o = v.key_input(KeyInput {
            code: KeyCode(0x52),
            down: true,
            text: None,
            locks: None,
        });
        assert!(matches!(o.events[0], InputEvent::Key { .. }));
        // Map mode ignores text.
        let mut v = viewer();
        assert!(matches!(
            typed(&mut v, 0x14, "q").events[0],
            InputEvent::Key { .. }
        ));
    }

    #[test]
    fn shortcuts_are_recognised_and_not_sent() {
        let mut v = viewer();
        press(&mut v, 0xE0, true);
        press(&mut v, 0xE2, true);
        let o = press(&mut v, 0x4D, true);
        assert_eq!(o.command, Some(ViewerCommand::CtrlAltDel));
        assert!(o.events.is_empty());
        assert!(
            press(&mut v, 0x4D, false).events.is_empty(),
            "End was never sent"
        );
        press(&mut v, 0xE1, true);
        assert_eq!(
            press(&mut v, 0x10, true).command,
            Some(ViewerCommand::ToggleRelativeMouse)
        );
        assert_eq!(
            press(&mut v, 0x0E, true).command,
            Some(ViewerCommand::ToggleKeyboardMode)
        );
        let o = press(&mut v, 0x09, true);
        assert_eq!(o.command, Some(ViewerCommand::ToggleFullscreen));
        assert!(o.events.is_empty(), "the chord's F is not sent");
        // Without Ctrl+Alt these are ordinary keys.
        let mut v = viewer();
        assert_eq!(press(&mut v, 0x4D, true).command, None);
        // Shortcuts work in view-only mode too (they are the viewer's own).
        let mut v = ViewerInput::new();
        press(&mut v, 0xE0, true);
        press(&mut v, 0xE2, true);
        assert_eq!(
            press(&mut v, 0x4D, true).command,
            Some(ViewerCommand::CtrlAltDel)
        );
    }

    #[test]
    fn relative_mode_sends_motion_and_clicks_anywhere() {
        let mut v = viewer();
        assert_eq!(v.mouse_motion(3.0, -2.0), None, "absolute mode");
        v.set_relative_mouse(true);
        assert_eq!(v.cursor_moved((10.0, 10.0)), None);
        assert_eq!(
            v.mouse_motion(3.4, -2.0),
            Some(InputEvent::MouseMoveRelative { dx: 3, dy: -2 })
        );
        assert_eq!(v.mouse_motion(0.2, 0.0), None);
        v.cursor_left();
        assert!(v.mouse_button(W::Left, true).is_some());
    }

    #[test]
    fn focus_loss_releases_everything_modifiers_last() {
        let mut v = viewer();
        press(&mut v, 0xE0, true);
        press(&mut v, 0xE2, true);
        press(&mut v, 0x2B, true);
        v.cursor_moved((5.0, 5.0));
        v.mouse_button(W::Right, true);
        let released = v.release_all();
        assert_eq!(
            released,
            vec![
                InputEvent::key(KeyCode(0x2B), false, Modifiers::CONTROL | Modifiers::ALT),
                InputEvent::key(KeyCode(0xE0), false, Modifiers::ALT),
                InputEvent::key(KeyCode(0xE2), false, Modifiers::NONE),
                InputEvent::MouseButton {
                    button: MouseButton::Right,
                    down: false
                },
            ]
        );
        assert!(!v.anything_held());
        assert!(v.release_all().is_empty());
        // After a focus change, shortcuts need fresh presses.
        assert_eq!(press(&mut v, 0x4D, true).command, None);
    }

    fn mv(x: i32) -> InputEvent {
        InputEvent::MouseMove {
            display: 0,
            x,
            y: 0,
        }
    }

    #[test]
    fn coalescer_sends_one_move_per_interval_the_latest() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut c = Coalescer::new(Duration::from_millis(16));
        assert_eq!(c.push(mv(1), t0), vec![mv(1)], "first move goes at once");
        assert!(c.push(mv(2), ms(4)).is_empty());
        assert!(c.push(mv(3), ms(8)).is_empty());
        assert_eq!(c.deadline(), Some(ms(16)));
        assert_eq!(c.poll(ms(15)), None);
        assert_eq!(c.poll(ms(16)), Some(mv(3)), "only the latest");
        assert_eq!(c.deadline(), None);
        // After a quiet spell a move goes at once again.
        assert_eq!(c.push(mv(4), ms(100)), vec![mv(4)]);
    }

    #[test]
    fn coalescer_sums_relative_motion() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let rel = |dx, dy| InputEvent::MouseMoveRelative { dx, dy };
        let mut c = Coalescer::new(Duration::from_millis(16));
        assert_eq!(c.push(rel(1, 1), t0), vec![rel(1, 1)]);
        assert!(c.push(rel(2, -1), ms(2)).is_empty());
        assert!(c.push(rel(3, 0), ms(4)).is_empty());
        assert_eq!(c.poll(ms(16)), Some(rel(5, -1)), "no motion is lost");
        // Switching kinds keeps both, in order.
        assert_eq!(c.push(rel(1, 0), ms(40)), vec![rel(1, 0)]);
        assert!(c.push(rel(1, 0), ms(41)).is_empty());
        assert_eq!(c.push(mv(7), ms(42)), vec![rel(1, 0)]);
        assert_eq!(c.flush(), Some(mv(7)));
    }

    #[test]
    fn coalescer_never_reorders_a_move_after_a_click() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(16));
        c.push(mv(1), t0);
        assert!(c.push(mv(2), t0 + Duration::from_millis(1)).is_empty());
        let click = InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
        };
        assert_eq!(
            c.push(click.clone(), t0 + Duration::from_millis(2)),
            vec![mv(2), click]
        );
        assert_eq!(c.flush(), None);
        assert!(c.push(mv(3), t0 + Duration::from_millis(3)).is_empty());
        assert_eq!(c.flush(), Some(mv(3)));
    }
}
