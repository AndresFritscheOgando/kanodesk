//! Applies input events on the host: permission checks, display offsets,
//! and bookkeeping of what is held, so everything can be released.
//!
//! [`InputSink`] is the OS backend (it only knows virtual-desktop pixels and
//! raw key/button presses). [`Injector`] sits in front of it:
//!
//! - drops mouse events while the mouse permission is off and keyboard
//!   events (keys, text, actions) while the keyboard permission is off (07 §5);
//! - maps display-relative positions onto the virtual desktop (multi-monitor
//!   offsets), clamped to the display;
//! - remembers which keys and buttons are down and ignores releases of things
//!   that are not;
//! - keeps modifiers in step with the controller: a modifier it no longer
//!   holds is released, one it holds but the host lost (e.g. after a quiet
//!   release during a network stall) is pressed again;
//! - toggles the host's Caps/Num/Scroll Lock to match the controller's before
//!   a key, when the controller reports them;
//! - releases everything held when asked, when a permission is revoked, and
//!   when it is dropped (07 §6), so no key stays stuck on the host.

use std::collections::BTreeSet;

use crate::InputError;
use crate::event::{Action, InputEvent, KeyCode, Modifiers, MouseButton};

/// An OS input backend.
pub trait InputSink: Send {
    /// Moves the pointer to `(x, y)` in virtual-desktop pixels.
    fn move_to(&mut self, x: i32, y: i32) -> Result<(), InputError>;
    /// Presses or releases a button.
    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), InputError>;
    /// Turns the wheel (`WHEEL_DELTA` units; positive `dy` = up, `dx` = right).
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), InputError>;
    /// Presses or releases a key.
    fn key(&mut self, code: KeyCode, down: bool) -> Result<(), InputError>;
    /// Moves the pointer by `(dx, dy)`.
    fn move_relative(&mut self, _dx: i32, _dy: i32) -> Result<(), InputError> {
        Err(InputError::Unsupported("relative mouse"))
    }
    /// Types `text` as characters, independent of the keyboard layout.
    fn text(&mut self, _text: &str) -> Result<(), InputError> {
        Err(InputError::Unsupported("text input"))
    }
    /// Performs an action such as locking the screen.
    fn action(&mut self, _action: Action) -> Result<(), InputError> {
        Err(InputError::Unsupported("input actions"))
    }
    /// The host's Caps/Num/Scroll Lock states, if they can be read.
    fn lock_state(&mut self) -> Option<Modifiers> {
        None
    }
}

impl<S: InputSink + ?Sized> InputSink for Box<S> {
    fn move_to(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        (**self).move_to(x, y)
    }
    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), InputError> {
        (**self).button(button, down)
    }
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        (**self).wheel(dx, dy)
    }
    fn key(&mut self, code: KeyCode, down: bool) -> Result<(), InputError> {
        (**self).key(code, down)
    }
    fn move_relative(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        (**self).move_relative(dx, dy)
    }
    fn text(&mut self, text: &str) -> Result<(), InputError> {
        (**self).text(text)
    }
    fn action(&mut self, action: Action) -> Result<(), InputError> {
        (**self).action(action)
    }
    fn lock_state(&mut self) -> Option<Modifiers> {
        (**self).lock_state()
    }
}

/// Where a display sits on the host's virtual desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayRect {
    /// Display id, as in `MouseEvent.display`.
    pub id: u32,
    /// Left edge in virtual-desktop pixels.
    pub x: i32,
    /// Top edge in virtual-desktop pixels.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// What the controller may do (host-side "keyboard/mouse allowed").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// Keyboard events are applied.
    pub keyboard: bool,
    /// Mouse events are applied.
    pub mouse: bool,
}

impl Permissions {
    /// Keyboard and mouse allowed.
    pub const ALL: Permissions = Permissions {
        keyboard: true,
        mouse: true,
    };
    /// View only.
    pub const NONE: Permissions = Permissions {
        keyboard: false,
        mouse: false,
    };

    /// The `Permission` messages that tell a viewer about these permissions.
    pub fn to_proto(self) -> [kanodesk_proto::v1::Permission; 2] {
        use kanodesk_proto::v1::{Permission, PermissionKind};
        [
            Permission {
                kind: PermissionKind::Keyboard as i32,
                enabled: self.keyboard,
            },
            Permission {
                kind: PermissionKind::Mouse as i32,
                enabled: self.mouse,
            },
        ]
    }

    /// These permissions updated by one `Permission` message (other kinds
    /// leave them unchanged).
    pub fn with(self, message: &kanodesk_proto::v1::Permission) -> Permissions {
        use kanodesk_proto::v1::PermissionKind;
        match message.kind() {
            PermissionKind::Keyboard => Permissions {
                keyboard: message.enabled,
                ..self
            },
            PermissionKind::Mouse => Permissions {
                mouse: message.enabled,
                ..self
            },
            _ => self,
        }
    }
}

/// What [`Injector::apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Passed to the OS.
    Injected,
    /// Dropped: the permission is off.
    NotAllowed,
    /// Dropped: no display with that id.
    UnknownDisplay,
    /// Dropped: a release of something that is not held.
    NotHeld,
}

/// Applies events to an [`InputSink`]; see the module docs.
pub struct Injector<S: InputSink> {
    sink: S,
    displays: Vec<DisplayRect>,
    allowed: Permissions,
    keys: BTreeSet<KeyCode>,
    buttons: BTreeSet<MouseButton>,
}

impl<S: InputSink> Injector<S> {
    /// An injector for `displays` with the given permissions.
    pub fn new(sink: S, displays: Vec<DisplayRect>, allowed: Permissions) -> Self {
        Injector {
            sink,
            displays,
            allowed,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
        }
    }

    /// Replaces the display layout (after a display change).
    pub fn set_displays(&mut self, displays: Vec<DisplayRect>) {
        self.displays = displays;
    }

    /// The current permissions.
    pub fn permissions(&self) -> Permissions {
        self.allowed
    }

    /// Changes the permissions; whatever is no longer allowed is released.
    pub fn set_permissions(&mut self, allowed: Permissions) -> Result<(), InputError> {
        self.allowed = allowed;
        let mut result = Ok(());
        if !allowed.keyboard {
            result = result.and(self.release_keys());
        }
        if !allowed.mouse {
            result = result.and(self.release_buttons());
        }
        result
    }

    /// Keys currently held down by the controller.
    pub fn held_keys(&self) -> impl Iterator<Item = KeyCode> + '_ {
        self.keys.iter().copied()
    }

    /// Buttons currently held down by the controller.
    pub fn held_buttons(&self) -> impl Iterator<Item = MouseButton> + '_ {
        self.buttons.iter().copied()
    }

    /// Whether any key or button is held.
    pub fn anything_held(&self) -> bool {
        !self.keys.is_empty() || !self.buttons.is_empty()
    }

    /// The sink (for tests and diagnostics).
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Applies one event.
    pub fn apply(&mut self, event: &InputEvent) -> Result<Applied, InputError> {
        let allowed = if event.is_mouse() {
            self.allowed.mouse
        } else {
            self.allowed.keyboard
        };
        if !allowed {
            return Ok(Applied::NotAllowed);
        }
        match *event {
            InputEvent::MouseMove { display, x, y } => {
                let Some(d) = self.displays.iter().find(|d| d.id == display) else {
                    return Ok(Applied::UnknownDisplay);
                };
                let x = x.clamp(0, d.width.saturating_sub(1) as i32);
                let y = y.clamp(0, d.height.saturating_sub(1) as i32);
                self.sink.move_to(d.x + x, d.y + y)?;
            }
            InputEvent::MouseMoveRelative { dx, dy } => self.sink.move_relative(dx, dy)?,
            InputEvent::MouseButton { button, down } => {
                if down {
                    self.buttons.insert(button);
                } else if !self.buttons.remove(&button) {
                    return Ok(Applied::NotHeld);
                }
                self.sink.button(button, down)?;
            }
            InputEvent::Wheel { dx, dy } => self.sink.wheel(dx, dy)?,
            InputEvent::Key {
                code,
                down,
                modifiers,
                locks,
            } => {
                if down && let Some(wanted) = locks {
                    self.sync_locks(code, wanted)?;
                }
                self.sync_modifiers(code, down, modifiers)?;
                if down {
                    // A repeat of a held key is sent again: the OS does not
                    // auto-repeat injected keys, the controller's repeats do it.
                    self.keys.insert(code);
                } else if !self.keys.remove(&code) {
                    return Ok(Applied::NotHeld);
                }
                if let Err(e) = self.sink.key(code, down) {
                    if down {
                        self.keys.remove(&code);
                    }
                    return Err(e);
                }
            }
            InputEvent::Text(ref text) => self.sink.text(text)?,
            InputEvent::Action(action) => self.sink.action(action)?,
        }
        Ok(Applied::Injected)
    }

    /// Brings the host's modifiers in line with the controller's `modifiers`
    /// (what it holds after this event). Releases held modifiers it no
    /// longer holds; presses (the left key of) a class it holds that the host
    /// has none of. `code` itself is left to the event.
    fn sync_modifiers(
        &mut self,
        code: KeyCode,
        down: bool,
        modifiers: Modifiers,
    ) -> Result<(), InputError> {
        let stale: Vec<KeyCode> = self
            .keys
            .iter()
            .copied()
            .filter(|&k| k != code)
            .filter(|k| k.modifier().is_some_and(|m| !modifiers.contains(m)))
            .collect();
        for k in stale {
            self.keys.remove(&k);
            self.sink.key(k, false)?;
        }
        for (class, key) in Modifiers::HELD {
            let wanted = modifiers.contains(class);
            // This event itself provides the class.
            let provided = down && code.modifier() == Some(class);
            let held = self.keys.iter().any(|k| k.modifier() == Some(class));
            if wanted && !provided && !held {
                self.keys.insert(key);
                self.sink.key(key, true)?;
            }
        }
        Ok(())
    }

    /// Toggles host locks that differ from the controller's `wanted` states,
    /// unless this key is itself a lock key (it toggles on its own).
    fn sync_locks(&mut self, code: KeyCode, wanted: Modifiers) -> Result<(), InputError> {
        if code.lock().is_some() {
            return Ok(());
        }
        let Some(have) = self.sink.lock_state() else {
            return Ok(());
        };
        for (lock, key) in Modifiers::LOCKS {
            if have.contains(lock) != wanted.contains(lock) {
                self.sink.key(key, true)?;
                self.sink.key(key, false)?;
            }
        }
        Ok(())
    }

    fn release_keys(&mut self) -> Result<(), InputError> {
        let mut result = Ok(());
        // Non-modifiers first, so e.g. Ctrl+C never turns into a bare C.
        let mut keys: Vec<KeyCode> = std::mem::take(&mut self.keys).into_iter().collect();
        keys.sort_by_key(|k| k.modifier().is_some());
        for k in keys {
            result = result.and(self.sink.key(k, false));
        }
        result
    }

    fn release_buttons(&mut self) -> Result<(), InputError> {
        let mut result = Ok(());
        for b in std::mem::take(&mut self.buttons) {
            result = result.and(self.sink.button(b, false));
        }
        result
    }

    /// Releases every held key and button. Tries all of them even if one fails.
    pub fn release_all(&mut self) -> Result<(), InputError> {
        let keys = self.release_keys();
        let buttons = self.release_buttons();
        keys.and(buttons)
    }
}

impl<S: InputSink> Drop for Injector<S> {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

/// One call an [`InputSink`] received (see [`RecordingSink`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkCall {
    /// `move_to(x, y)`.
    Move(i32, i32),
    /// `move_relative(dx, dy)`.
    MoveRelative(i32, i32),
    /// `button(button, down)`.
    Button(MouseButton, bool),
    /// `wheel(dx, dy)`.
    Wheel(i32, i32),
    /// `key(code, down)`.
    Key(KeyCode, bool),
    /// `text(text)`.
    Text(String),
    /// `action(action)`.
    Action(Action),
}

/// An [`InputSink`] that records calls instead of injecting them (tests,
/// dry runs). It tracks lock states from the lock keys it receives. Clones
/// share the record.
#[derive(Debug, Clone, Default)]
pub struct RecordingSink {
    calls: std::sync::Arc<std::sync::Mutex<Vec<SinkCall>>>,
    locks: std::sync::Arc<std::sync::Mutex<Option<Modifiers>>>,
}

impl RecordingSink {
    /// A sink whose host reports these lock states.
    pub fn with_locks(locks: Modifiers) -> Self {
        let sink = RecordingSink::default();
        *sink.locks.lock().expect("not poisoned") = Some(locks);
        sink
    }

    /// Everything recorded so far.
    pub fn calls(&self) -> Vec<SinkCall> {
        self.calls.lock().expect("not poisoned").clone()
    }

    /// Clears the record.
    pub fn clear(&self) {
        self.calls.lock().expect("not poisoned").clear();
    }

    /// The lock states as tracked, if lock reporting was enabled.
    pub fn locks(&self) -> Option<Modifiers> {
        *self.locks.lock().expect("not poisoned")
    }

    fn push(&self, call: SinkCall) -> Result<(), InputError> {
        self.calls.lock().expect("not poisoned").push(call);
        Ok(())
    }
}

impl InputSink for RecordingSink {
    fn move_to(&mut self, x: i32, y: i32) -> Result<(), InputError> {
        self.push(SinkCall::Move(x, y))
    }
    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), InputError> {
        self.push(SinkCall::Button(button, down))
    }
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        self.push(SinkCall::Wheel(dx, dy))
    }
    fn key(&mut self, code: KeyCode, down: bool) -> Result<(), InputError> {
        if down
            && let Some(lock) = code.lock()
            && let Some(state) = self.locks.lock().expect("not poisoned").as_mut()
        {
            state.0 ^= lock.0;
        }
        self.push(SinkCall::Key(code, down))
    }
    fn move_relative(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        self.push(SinkCall::MoveRelative(dx, dy))
    }
    fn text(&mut self, text: &str) -> Result<(), InputError> {
        self.push(SinkCall::Text(text.to_owned()))
    }
    fn action(&mut self, action: Action) -> Result<(), InputError> {
        self.push(SinkCall::Action(action))
    }
    fn lock_state(&mut self) -> Option<Modifiers> {
        self.locks()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: KeyCode = KeyCode(0x04);
    const C: KeyCode = KeyCode(0x06);
    const LCTRL: KeyCode = KeyCode::LEFT_CONTROL;
    const LSHIFT: KeyCode = KeyCode::LEFT_SHIFT;
    const RSHIFT: KeyCode = KeyCode(0xE5);

    fn displays() -> Vec<DisplayRect> {
        vec![
            DisplayRect {
                id: 0,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            // A second monitor left of and above the primary.
            DisplayRect {
                id: 1,
                x: -1280,
                y: -200,
                width: 1280,
                height: 1024,
            },
        ]
    }

    fn injector(allowed: Permissions) -> (Injector<RecordingSink>, RecordingSink) {
        let sink = RecordingSink::default();
        (Injector::new(sink.clone(), displays(), allowed), sink)
    }

    fn key(code: KeyCode, down: bool, modifiers: Modifiers) -> InputEvent {
        InputEvent::key(code, down, modifiers)
    }

    #[test]
    fn permissions_travel_as_messages() {
        for p in [
            Permissions::ALL,
            Permissions::NONE,
            Permissions {
                keyboard: true,
                mouse: false,
            },
        ] {
            let [a, b] = p.to_proto();
            assert_eq!(Permissions::NONE.with(&a).with(&b), p);
        }
        let clipboard = kanodesk_proto::v1::Permission {
            kind: kanodesk_proto::v1::PermissionKind::Clipboard as i32,
            enabled: false,
        };
        assert_eq!(Permissions::ALL.with(&clipboard), Permissions::ALL);
    }

    #[test]
    fn positions_get_the_display_offset_and_are_clamped() {
        let (mut inj, sink) = injector(Permissions::ALL);
        let mv = |display, x, y| InputEvent::MouseMove { display, x, y };
        assert_eq!(inj.apply(&mv(0, 100, 50)).unwrap(), Applied::Injected);
        inj.apply(&mv(1, 10, 20)).unwrap();
        inj.apply(&mv(1, 5000, -5)).unwrap();
        assert_eq!(inj.apply(&mv(7, 1, 1)).unwrap(), Applied::UnknownDisplay);
        inj.apply(&InputEvent::MouseMoveRelative { dx: -3, dy: 4 })
            .unwrap();
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Move(100, 50),
                SinkCall::Move(-1270, -180),
                SinkCall::Move(-1, -200),
                SinkCall::MoveRelative(-3, 4),
            ]
        );
    }

    #[test]
    fn permissions_filter_events_and_revoking_releases() {
        let (mut inj, sink) = injector(Permissions {
            keyboard: false,
            mouse: true,
        });
        for e in [
            key(A, true, Modifiers::NONE),
            InputEvent::Text("x".into()),
            InputEvent::Action(Action::LockScreen),
        ] {
            assert_eq!(inj.apply(&e).unwrap(), Applied::NotAllowed);
        }
        let down = InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
        };
        assert_eq!(inj.apply(&down).unwrap(), Applied::Injected);
        assert_eq!(inj.held_buttons().collect::<Vec<_>>(), [MouseButton::Left]);
        // Revoking the mouse permission releases the held button.
        inj.set_permissions(Permissions::NONE).unwrap();
        assert_eq!(inj.held_buttons().count(), 0);
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Button(MouseButton::Left, true),
                SinkCall::Button(MouseButton::Left, false),
            ]
        );
        assert_eq!(
            inj.apply(&InputEvent::Wheel { dx: 0, dy: 120 }).unwrap(),
            Applied::NotAllowed
        );
        assert_eq!(
            inj.apply(&InputEvent::MouseMoveRelative { dx: 1, dy: 1 })
                .unwrap(),
            Applied::NotAllowed
        );
    }

    #[test]
    fn text_and_actions_reach_the_sink() {
        let (mut inj, sink) = injector(Permissions::ALL);
        inj.apply(&InputEvent::Text("ä€".into())).unwrap();
        inj.apply(&InputEvent::Action(Action::CtrlAltDel)).unwrap();
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Text("ä€".into()),
                SinkCall::Action(Action::CtrlAltDel)
            ]
        );
    }

    #[test]
    fn releases_of_unheld_keys_are_dropped_and_repeats_pass() {
        let (mut inj, sink) = injector(Permissions::ALL);
        assert_eq!(
            inj.apply(&key(A, false, Modifiers::NONE)).unwrap(),
            Applied::NotHeld
        );
        inj.apply(&key(A, true, Modifiers::NONE)).unwrap();
        inj.apply(&key(A, true, Modifiers::NONE)).unwrap(); // auto-repeat
        inj.apply(&key(A, false, Modifiers::NONE)).unwrap();
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Key(A, true),
                SinkCall::Key(A, true),
                SinkCall::Key(A, false)
            ]
        );
    }

    #[test]
    fn stale_modifiers_are_released() {
        let (mut inj, sink) = injector(Permissions::ALL);
        inj.apply(&key(LCTRL, true, Modifiers::CONTROL)).unwrap();
        inj.apply(&key(LSHIFT, true, Modifiers::CONTROL | Modifiers::SHIFT))
            .unwrap();
        sink.clear();
        // The controller says only Shift is held now: Ctrl's release was lost.
        inj.apply(&key(C, true, Modifiers::SHIFT)).unwrap();
        assert_eq!(
            sink.calls(),
            vec![SinkCall::Key(LCTRL, false), SinkCall::Key(C, true)]
        );
        // A modifier's own event does not release or re-press itself.
        sink.clear();
        inj.apply(&key(LSHIFT, true, Modifiers::SHIFT)).unwrap();
        assert_eq!(sink.calls(), vec![SinkCall::Key(LSHIFT, true)]);
        sink.clear();
        inj.apply(&key(LSHIFT, false, Modifiers::NONE)).unwrap();
        assert_eq!(sink.calls(), vec![SinkCall::Key(LSHIFT, false)]);
    }

    #[test]
    fn modifiers_lost_on_the_host_are_pressed_again() {
        // Shift held through a network stall: the host released it (quiet
        // release); the next key says Shift is still held.
        let (mut inj, sink) = injector(Permissions::ALL);
        inj.apply(&key(RSHIFT, true, Modifiers::SHIFT)).unwrap();
        inj.release_all().unwrap();
        sink.clear();
        inj.apply(&key(A, true, Modifiers::SHIFT)).unwrap();
        assert_eq!(
            sink.calls(),
            vec![SinkCall::Key(LSHIFT, true), SinkCall::Key(A, true)]
        );
        // The controller's eventual Right Shift release clears the class.
        sink.clear();
        inj.apply(&key(A, false, Modifiers::SHIFT)).unwrap();
        inj.apply(&key(RSHIFT, false, Modifiers::NONE)).unwrap();
        assert_eq!(
            sink.calls(),
            vec![SinkCall::Key(A, false), SinkCall::Key(LSHIFT, false)]
        );
        assert_eq!(inj.held_keys().count(), 0);
    }

    #[test]
    fn host_locks_follow_the_controller() {
        let sink = RecordingSink::with_locks(Modifiers::NUM_LOCK);
        let mut inj = Injector::new(sink.clone(), displays(), Permissions::ALL);
        let with_locks = |code, locks| InputEvent::Key {
            code,
            down: true,
            modifiers: Modifiers::NONE,
            locks: Some(locks),
        };
        // Controller: Caps on, Num on. Host: Num on → toggle Caps first.
        inj.apply(&with_locks(A, Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK))
            .unwrap();
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Key(KeyCode::CAPS_LOCK, true),
                SinkCall::Key(KeyCode::CAPS_LOCK, false),
                SinkCall::Key(A, true),
            ]
        );
        assert_eq!(
            sink.locks(),
            Some(Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK)
        );
        // In step: nothing extra. A lock key itself is never pre-synced.
        sink.clear();
        inj.apply(&with_locks(C, Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK))
            .unwrap();
        inj.apply(&with_locks(KeyCode::NUM_LOCK, Modifiers::NONE))
            .unwrap();
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Key(C, true),
                SinkCall::Key(KeyCode::NUM_LOCK, true)
            ]
        );
        // Without lock information nothing is toggled.
        let (mut inj, sink) = injector(Permissions::ALL);
        inj.apply(&with_locks(A, Modifiers::CAPS_LOCK)).unwrap();
        assert_eq!(sink.calls(), vec![SinkCall::Key(A, true)]);
    }

    #[test]
    fn release_all_and_drop_release_everything_modifiers_last() {
        let (mut inj, sink) = injector(Permissions::ALL);
        inj.apply(&key(LCTRL, true, Modifiers::CONTROL)).unwrap();
        inj.apply(&key(C, true, Modifiers::CONTROL)).unwrap();
        inj.apply(&InputEvent::MouseButton {
            button: MouseButton::Right,
            down: true,
        })
        .unwrap();
        sink.clear();
        inj.release_all().unwrap();
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Key(C, false),
                SinkCall::Key(LCTRL, false),
                SinkCall::Button(MouseButton::Right, false),
            ]
        );
        // Nothing left to release.
        sink.clear();
        inj.release_all().unwrap();
        assert!(sink.calls().is_empty());

        inj.apply(&key(A, true, Modifiers::NONE)).unwrap();
        sink.clear();
        drop(inj);
        assert_eq!(sink.calls(), vec![SinkCall::Key(A, false)]);
    }

    struct FailingSink;
    impl InputSink for FailingSink {
        fn move_to(&mut self, _: i32, _: i32) -> Result<(), InputError> {
            Err(InputError::Os("nope".into()))
        }
        fn button(&mut self, _: MouseButton, _: bool) -> Result<(), InputError> {
            Err(InputError::Os("nope".into()))
        }
        fn wheel(&mut self, _: i32, _: i32) -> Result<(), InputError> {
            Err(InputError::Os("nope".into()))
        }
        fn key(&mut self, code: KeyCode, _: bool) -> Result<(), InputError> {
            Err(InputError::UnknownKey(code.0))
        }
    }

    #[test]
    fn a_key_the_sink_refuses_is_not_counted_as_held() {
        let mut inj = Injector::new(FailingSink, displays(), Permissions::ALL);
        assert!(
            inj.apply(&key(KeyCode(0x99), true, Modifiers::NONE))
                .is_err()
        );
        assert_eq!(inj.held_keys().count(), 0);
        // Defaults: a sink without text, relative mouse or actions refuses them.
        assert!(matches!(
            inj.apply(&InputEvent::Text("x".into())),
            Err(InputError::Unsupported(_))
        ));
    }
}
