//! # kanodesk-input
//!
//! Platform-neutral input events (mouse, keyboard in Map mode) and per-OS
//! injection backends behind a trait. Windows comes first.
//!
//! Specified by:
//! - `docs/07-input-control.md`
//!
//! - [`event`]: the event model ([`InputEvent`]) and its wire form
//!   (`kanodesk.v1.InputMessage`). Keys are USB HID usages (physical
//!   positions, Map mode); positions are pixels on a host display.
//! - [`grab`]: the viewer's keyboard grab (system keys such as Win and
//!   Alt+Tab go to the host while the viewer is in front).
//! - [`injector`]: [`InputSink`] (the OS backend) and [`Injector`]
//!   (permissions, display offsets, held-key bookkeeping, release on
//!   disconnect), plus [`RecordingSink`] for tests.
//! - [`scancode`]: HID usage to PC key (set-1 scan code or virtual key) and back.
//! - `windows` (Windows only): [`SendInputSink`].
//!
//! Status on Windows: Map-mode keys (keyboard and consumer pages), text
//! (Translate mode), absolute and relative mouse, buttons, wheel, lock-state
//! sync, lock screen. Ctrl+Alt+Del needs the service; other OSes come later.

pub mod event;
pub mod grab;
pub mod injector;
pub mod scancode;
#[cfg(windows)]
pub mod windows;

pub use event::{Action, InputEvent, KeyCode, Modifiers, MouseButton};
pub use injector::{
    Applied, DisplayRect, Injector, InputSink, Permissions, RecordingSink, SinkCall,
};
#[cfg(windows)]
pub use windows::SendInputSink;

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/07-input-control.md"];

/// Why an input event could not be read or applied.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// Malformed message.
    #[error("invalid input message: {0}")]
    Invalid(String),
    /// A valid message this host does not handle yet.
    #[error("unsupported input: {0}")]
    Unsupported(&'static str),
    /// No scan code for this HID usage.
    #[error("no key for HID usage {0:#04x}")]
    UnknownKey(u32),
    /// The OS refused the injection.
    #[error("{0}")]
    Os(String),
}

/// The input backend for this OS, if there is one.
pub fn platform_sink() -> Result<Box<dyn InputSink>, InputError> {
    #[cfg(windows)]
    {
        Ok(Box::new(SendInputSink::new()))
    }
    #[cfg(not(windows))]
    {
        Err(InputError::Unsupported("input injection on this OS"))
    }
}

/// This machine's Caps/Num/Scroll Lock states as the calling thread sees
/// them, where they can be read.
pub fn local_lock_state() -> Option<Modifiers> {
    #[cfg(windows)]
    {
        Some(windows::lock_state())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Whether the window message being handled was injected by a KanoDesk host
/// on this same machine (so a viewer must not forward it again). Always
/// `false` where there is no injection backend.
pub fn is_own_injection() -> bool {
    #[cfg(windows)]
    {
        windows::is_own_injection()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn spec_docs_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for doc in SPEC_DOCS {
            assert!(root.join(doc).is_file(), "missing spec doc {doc}");
        }
    }

    #[test]
    fn this_platform_has_a_sink() {
        assert_eq!(platform_sink().is_ok(), cfg!(windows));
        // The test thread is not handling an injected message.
        assert!(!is_own_injection());
    }
}
