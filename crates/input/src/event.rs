//! The input event model (independent of protobuf, 07 §6) and its wire form.
//!
//! Keys are identified by their USB HID usage, the physical key position:
//! "Map" mode (07 §2.1), where the host's keyboard layout decides which
//! character a key produces. Keyboard-page usages (0x07) are plain numbers;
//! other pages (media keys on the consumer page 0x0C) carry the page in the
//! upper 16 bits. Text that the viewer's layout already resolved travels as
//! [`InputEvent::Text`] ("Translate" mode). Mouse positions are display
//! pixels relative to the display's top-left corner; the host adds the
//! display's place on its virtual desktop.

use kanodesk_proto::v1::{self, InputMessage, KeyMode, MouseKind, input_message};

use crate::InputError;

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MouseButton {
    /// Primary button.
    Left,
    /// Secondary button.
    Right,
    /// Wheel button.
    Middle,
    /// Side button "back" (X1).
    Back,
    /// Side button "forward" (X2).
    Forward,
}

impl MouseButton {
    /// Every button.
    pub const ALL: [MouseButton; 5] = [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ];

    /// The button's bit in `MouseEvent.buttons`.
    pub fn bit(self) -> u32 {
        let b = match self {
            MouseButton::Left => v1::MouseButton::Left,
            MouseButton::Right => v1::MouseButton::Right,
            MouseButton::Middle => v1::MouseButton::Middle,
            MouseButton::Back => v1::MouseButton::Back,
            MouseButton::Forward => v1::MouseButton::Forward,
        };
        b as u32
    }

    /// The button whose bit is the only one set in `bits`.
    pub fn from_bits(bits: u32) -> Option<Self> {
        MouseButton::ALL.into_iter().find(|b| b.bit() == bits)
    }
}

/// A physical key: its USB HID usage. Keyboard-page (0x07) usages are below
/// 0x10000; any other page is `(page << 16) | usage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyCode(pub u32);

/// HID usage page of consumer controls (media and browser keys).
pub const CONSUMER_PAGE: u32 = 0x0C;

impl KeyCode {
    /// Caps Lock.
    pub const CAPS_LOCK: KeyCode = KeyCode(0x39);
    /// Scroll Lock.
    pub const SCROLL_LOCK: KeyCode = KeyCode(0x47);
    /// Num Lock.
    pub const NUM_LOCK: KeyCode = KeyCode(0x53);
    /// Left Control.
    pub const LEFT_CONTROL: KeyCode = KeyCode(0xE0);
    /// Left Shift.
    pub const LEFT_SHIFT: KeyCode = KeyCode(0xE1);
    /// Left Alt.
    pub const LEFT_ALT: KeyCode = KeyCode(0xE2);
    /// Left Windows/Command key.
    pub const LEFT_META: KeyCode = KeyCode(0xE3);

    /// A key on the keyboard page.
    pub const fn keyboard(usage: u16) -> KeyCode {
        KeyCode(usage as u32)
    }

    /// A key on the consumer page.
    pub const fn consumer(usage: u16) -> KeyCode {
        KeyCode((CONSUMER_PAGE << 16) | usage as u32)
    }

    /// The usage page (0x07 for the keyboard page).
    pub fn page(self) -> u32 {
        match self.0 >> 16 {
            0 => 0x07,
            page => page,
        }
    }

    /// The usage within its page.
    pub fn usage(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    /// Which modifier this key is, if it is one (left and right count the same).
    pub fn modifier(self) -> Option<Modifiers> {
        match self.0 {
            0xE0 | 0xE4 => Some(Modifiers::CONTROL),
            0xE1 | 0xE5 => Some(Modifiers::SHIFT),
            0xE2 | 0xE6 => Some(Modifiers::ALT),
            0xE3 | 0xE7 => Some(Modifiers::META),
            _ => None,
        }
    }

    /// Which lock this key toggles, if it is a lock key.
    pub fn lock(self) -> Option<Modifiers> {
        match self {
            KeyCode::CAPS_LOCK => Some(Modifiers::CAPS_LOCK),
            KeyCode::NUM_LOCK => Some(Modifiers::NUM_LOCK),
            KeyCode::SCROLL_LOCK => Some(Modifiers::SCROLL_LOCK),
            _ => None,
        }
    }
}

/// Modifier keys held, or lock states, as the bit sets of `KeyEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Modifiers(pub u32);

impl Modifiers {
    /// No modifiers.
    pub const NONE: Modifiers = Modifiers(0);
    /// Either Shift.
    pub const SHIFT: Modifiers = Modifiers(v1::Modifier::Shift as u32);
    /// Either Control.
    pub const CONTROL: Modifiers = Modifiers(v1::Modifier::Control as u32);
    /// Either Alt.
    pub const ALT: Modifiers = Modifiers(v1::Modifier::Alt as u32);
    /// Either Windows/Command key.
    pub const META: Modifiers = Modifiers(v1::Modifier::Meta as u32);
    /// Caps Lock is on.
    pub const CAPS_LOCK: Modifiers = Modifiers(v1::Modifier::CapsLock as u32);
    /// Num Lock is on.
    pub const NUM_LOCK: Modifiers = Modifiers(v1::Modifier::NumLock as u32);
    /// Scroll Lock is on.
    pub const SCROLL_LOCK: Modifiers = Modifiers(v1::Modifier::ScrollLock as u32);
    /// The four held-modifier classes, each with the key pressed to restore it.
    pub const HELD: [(Modifiers, KeyCode); 4] = [
        (Modifiers::CONTROL, KeyCode::LEFT_CONTROL),
        (Modifiers::SHIFT, KeyCode::LEFT_SHIFT),
        (Modifiers::ALT, KeyCode::LEFT_ALT),
        (Modifiers::META, KeyCode::LEFT_META),
    ];
    /// The three locks, each with its key.
    pub const LOCKS: [(Modifiers, KeyCode); 3] = [
        (Modifiers::CAPS_LOCK, KeyCode::CAPS_LOCK),
        (Modifiers::NUM_LOCK, KeyCode::NUM_LOCK),
        (Modifiers::SCROLL_LOCK, KeyCode::SCROLL_LOCK),
    ];

    /// Whether every bit of `other` is set.
    pub fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }

    /// Only the lock bits.
    pub fn locks(self) -> Modifiers {
        Modifiers(self.0 & (Self::CAPS_LOCK.0 | Self::NUM_LOCK.0 | Self::SCROLL_LOCK.0))
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Modifiers;
    fn bitor(self, rhs: Modifiers) -> Modifiers {
        Modifiers(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Modifiers) {
        self.0 |= rhs.0;
    }
}

/// Actions a viewer cannot send as plain keys (07 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The secure attention sequence (Ctrl+Alt+Del).
    CtrlAltDel,
    /// Lock the host's session.
    LockScreen,
}

/// One input event, controller to host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Move the pointer to `(x, y)` on `display` (pixels from its top-left corner).
    MouseMove {
        /// Host display id.
        display: u32,
        /// Pixels from the display's left edge.
        x: i32,
        /// Pixels from the display's top edge.
        y: i32,
    },
    /// Move the pointer by `(dx, dy)` (pointer-locked mode, games).
    MouseMoveRelative {
        /// Horizontal motion.
        dx: i32,
        /// Vertical motion.
        dy: i32,
    },
    /// Press or release a button at the current pointer position.
    MouseButton {
        /// Which button.
        button: MouseButton,
        /// Pressed (`true`) or released.
        down: bool,
    },
    /// Turn the wheel, in Windows `WHEEL_DELTA` units (120 = one notch).
    /// Positive `dy` scrolls up (away from the user), positive `dx` right.
    Wheel {
        /// Horizontal amount.
        dx: i32,
        /// Vertical amount.
        dy: i32,
    },
    /// Press or release a physical key.
    Key {
        /// The key.
        code: KeyCode,
        /// Pressed (`true`, also for auto-repeat) or released.
        down: bool,
        /// Modifiers held after this event, on the controller.
        modifiers: Modifiers,
        /// The controller's lock states, if it knows them.
        locks: Option<Modifiers>,
    },
    /// Type these characters (resolved by the controller's layout).
    Text(String),
    /// An action such as Ctrl+Alt+Del.
    Action(Action),
}

/// The keep-alive message (no event), numbered `seq`.
pub fn keep_alive(seq: u64) -> InputMessage {
    InputMessage { seq, msg: None }
}

/// Whether `msg` is a keep-alive rather than an event.
pub fn is_keep_alive(msg: &InputMessage) -> bool {
    msg.msg.is_none()
}

/// Longest text accepted in one `TextInput`, in characters.
pub const MAX_TEXT_CHARS: usize = 4096;

impl InputEvent {
    /// A key event without lock information.
    pub fn key(code: KeyCode, down: bool, modifiers: Modifiers) -> InputEvent {
        InputEvent::Key {
            code,
            down,
            modifiers,
            locks: None,
        }
    }

    /// Whether this is a mouse event (needs the mouse permission); keys,
    /// text and actions need the keyboard permission.
    pub fn is_mouse(&self) -> bool {
        matches!(
            self,
            InputEvent::MouseMove { .. }
                | InputEvent::MouseMoveRelative { .. }
                | InputEvent::MouseButton { .. }
                | InputEvent::Wheel { .. }
        )
    }

    /// The wire message, numbered `seq`.
    pub fn to_proto(&self, seq: u64) -> InputMessage {
        let mouse = |kind: MouseKind, m: v1::MouseEvent| {
            input_message::Msg::Mouse(v1::MouseEvent {
                kind: kind as i32,
                ..m
            })
        };
        let msg = match self {
            &InputEvent::MouseMove { display, x, y } => mouse(
                MouseKind::Move,
                v1::MouseEvent {
                    x,
                    y,
                    display,
                    ..Default::default()
                },
            ),
            &InputEvent::MouseMoveRelative { dx, dy } => mouse(
                MouseKind::MoveRelative,
                v1::MouseEvent {
                    dx,
                    dy,
                    ..Default::default()
                },
            ),
            &InputEvent::MouseButton { button, down } => mouse(
                if down {
                    MouseKind::ButtonDown
                } else {
                    MouseKind::ButtonUp
                },
                v1::MouseEvent {
                    buttons: button.bit(),
                    ..Default::default()
                },
            ),
            &InputEvent::Wheel { dx, dy } => mouse(
                MouseKind::Wheel,
                v1::MouseEvent {
                    dx,
                    dy,
                    ..Default::default()
                },
            ),
            &InputEvent::Key {
                code,
                down,
                modifiers,
                locks,
            } => input_message::Msg::Key(v1::KeyEvent {
                code: code.0,
                down,
                modifiers: modifiers.0,
                mode: KeyMode::Map as i32,
                locks: locks.map(|l| l.locks().0),
            }),
            InputEvent::Text(text) => {
                input_message::Msg::Text(v1::TextInput { text: text.clone() })
            }
            &InputEvent::Action(action) => input_message::Msg::Action(v1::ActionEvent {
                action: match action {
                    Action::CtrlAltDel => v1::InputAction::CtrlAltDel,
                    Action::LockScreen => v1::InputAction::LockScreen,
                } as i32,
            }),
        };
        InputMessage {
            seq,
            msg: Some(msg),
        }
    }

    /// Reads a wire message (not a keep-alive; see [`is_keep_alive`]).
    pub fn from_proto(msg: &InputMessage) -> Result<InputEvent, InputError> {
        match &msg.msg {
            Some(input_message::Msg::Mouse(m)) => match m.kind() {
                MouseKind::Move => Ok(InputEvent::MouseMove {
                    display: m.display,
                    x: m.x,
                    y: m.y,
                }),
                MouseKind::MoveRelative => Ok(InputEvent::MouseMoveRelative { dx: m.dx, dy: m.dy }),
                kind @ (MouseKind::ButtonDown | MouseKind::ButtonUp) => {
                    let button = MouseButton::from_bits(m.buttons).ok_or_else(|| {
                        InputError::Invalid(format!("button bits {:#x}", m.buttons))
                    })?;
                    Ok(InputEvent::MouseButton {
                        button,
                        down: kind == MouseKind::ButtonDown,
                    })
                }
                MouseKind::Wheel => Ok(InputEvent::Wheel { dx: m.dx, dy: m.dy }),
                MouseKind::Unspecified => Err(InputError::Invalid("mouse kind".into())),
            },
            Some(input_message::Msg::Key(k)) => {
                if k.mode() == KeyMode::Translate {
                    // Translate mode sends characters as TextInput instead.
                    return Err(InputError::Unsupported("KeyEvent in Translate mode"));
                }
                Ok(InputEvent::Key {
                    code: KeyCode(k.code),
                    down: k.down,
                    modifiers: Modifiers(k.modifiers),
                    locks: k.locks.map(|l| Modifiers(l).locks()),
                })
            }
            Some(input_message::Msg::Text(t)) => {
                if t.text.chars().count() > MAX_TEXT_CHARS {
                    return Err(InputError::Invalid("text input too long".into()));
                }
                Ok(InputEvent::Text(t.text.clone()))
            }
            Some(input_message::Msg::Action(a)) => match a.action() {
                v1::InputAction::CtrlAltDel => Ok(InputEvent::Action(Action::CtrlAltDel)),
                v1::InputAction::LockScreen => Ok(InputEvent::Action(Action::LockScreen)),
                v1::InputAction::Unspecified => Err(InputError::Invalid("input action".into())),
            },
            None => Err(InputError::Invalid("empty input message".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_the_wire_form() {
        let events = [
            InputEvent::MouseMove {
                display: 1,
                x: -3,
                y: 1079,
            },
            InputEvent::MouseMoveRelative { dx: -7, dy: 12 },
            InputEvent::MouseButton {
                button: MouseButton::Forward,
                down: true,
            },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: false,
            },
            InputEvent::Wheel { dx: -120, dy: 240 },
            InputEvent::key(KeyCode(0x04), true, Modifiers::SHIFT | Modifiers::CONTROL),
            InputEvent::Key {
                code: KeyCode::consumer(0xE9),
                down: false,
                modifiers: Modifiers::NONE,
                locks: Some(Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK),
            },
            InputEvent::Text("Grüße, 世界 🙂".into()),
            InputEvent::Action(Action::CtrlAltDel),
            InputEvent::Action(Action::LockScreen),
        ];
        for (i, e) in events.iter().enumerate() {
            let msg = e.to_proto(i as u64);
            assert_eq!(msg.seq, i as u64);
            assert_eq!(InputEvent::from_proto(&msg).unwrap(), *e);
        }
    }

    #[test]
    fn unsupported_and_invalid_messages_are_refused() {
        let mouse = |m: v1::MouseEvent| InputMessage {
            seq: 0,
            msg: Some(input_message::Msg::Mouse(m)),
        };
        // Two buttons at once is not one button.
        let two = mouse(v1::MouseEvent {
            kind: MouseKind::ButtonDown as i32,
            buttons: 3,
            ..Default::default()
        });
        assert!(matches!(
            InputEvent::from_proto(&two),
            Err(InputError::Invalid(_))
        ));
        let translate = InputMessage {
            seq: 0,
            msg: Some(input_message::Msg::Key(v1::KeyEvent {
                code: 4,
                down: true,
                mode: KeyMode::Translate as i32,
                ..Default::default()
            })),
        };
        assert!(InputEvent::from_proto(&translate).is_err());
        let long = InputMessage {
            seq: 0,
            msg: Some(input_message::Msg::Text(v1::TextInput {
                text: "x".repeat(MAX_TEXT_CHARS + 1),
            })),
        };
        assert!(InputEvent::from_proto(&long).is_err());
        let action = InputMessage {
            seq: 0,
            msg: Some(input_message::Msg::Action(v1::ActionEvent { action: 0 })),
        };
        assert!(InputEvent::from_proto(&action).is_err());
        assert!(InputEvent::from_proto(&InputMessage::default()).is_err());
        // Lock bits keep only locks.
        let locks = InputMessage {
            seq: 0,
            msg: Some(input_message::Msg::Key(v1::KeyEvent {
                code: 4,
                locks: Some(0xFF),
                ..Default::default()
            })),
        };
        let InputEvent::Key { locks, .. } = InputEvent::from_proto(&locks).unwrap() else {
            unreachable!()
        };
        assert_eq!(
            locks,
            Some(Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK | Modifiers::SCROLL_LOCK)
        );
    }

    #[test]
    fn keep_alives_are_messages_without_an_event() {
        let k = keep_alive(9);
        assert!(is_keep_alive(&k));
        assert_eq!(k.seq, 9);
        assert!(!is_keep_alive(
            &InputEvent::Wheel { dx: 0, dy: 1 }.to_proto(1)
        ));
    }

    #[test]
    fn key_codes_know_their_page_modifier_and_lock() {
        assert_eq!(KeyCode(0x04).page(), 0x07);
        assert_eq!(KeyCode::consumer(0xE9).page(), CONSUMER_PAGE);
        assert_eq!(KeyCode::consumer(0xE9).usage(), 0xE9);
        assert_eq!(KeyCode(0xE1).modifier(), Some(Modifiers::SHIFT));
        assert_eq!(KeyCode(0xE5).modifier(), Some(Modifiers::SHIFT));
        assert_eq!(KeyCode(0xE6).modifier(), Some(Modifiers::ALT));
        assert_eq!(KeyCode(0xE7).modifier(), Some(Modifiers::META));
        assert_eq!(KeyCode(0x04).modifier(), None);
        assert_eq!(KeyCode::consumer(0xE1).modifier(), None, "page matters");
        assert_eq!(KeyCode(0x39).lock(), Some(Modifiers::CAPS_LOCK));
        assert_eq!(KeyCode(0x04).lock(), None);
        assert!((Modifiers::SHIFT | Modifiers::ALT).contains(Modifiers::ALT));
        assert!(!Modifiers::SHIFT.contains(Modifiers::ALT));
        assert_eq!(
            (Modifiers::SHIFT | Modifiers::CAPS_LOCK).locks(),
            Modifiers::CAPS_LOCK
        );
    }

    #[test]
    fn mouse_and_keyboard_events_are_told_apart() {
        assert!(InputEvent::MouseMoveRelative { dx: 1, dy: 1 }.is_mouse());
        assert!(!InputEvent::Text("a".into()).is_mouse());
        assert!(!InputEvent::Action(Action::LockScreen).is_mouse());
    }
}
