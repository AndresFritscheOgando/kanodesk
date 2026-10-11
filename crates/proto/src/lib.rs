//! # kanodesk-proto
//!
//! Wire message types for the KanoDesk protocol (option B in
//! `docs/16-clone-blueprint.md` §1): control, video, input, stats and display
//! messages carried over QUIC streams and datagrams, plus the length-prefixed
//! framing codec. Not wire-compatible with RustDesk; `docs/proto/message.proto`
//! is a field-level reference only. The MVP message set is in 16 §8.
//!
//! - [`v1`]: prost types generated from `proto/kanodesk/v1/*.proto`. Each
//!   stream type has an envelope: [`v1::ControlMessage`], [`v1::VideoMessage`],
//!   [`v1::InputMessage`]. Every stream starts with a [`v1::StreamHeader`].
//! - [`hello`]: the `Hello` exchange and protocol-version negotiation.
//! - [`codec`]: `u32` big-endian length-prefixed frames with a size cap, as
//!   tokio-util codecs.
//! - [`io`]: async [`io::MessageReader`] / [`io::MessageWriter`] over any
//!   `AsyncRead`/`AsyncWrite`, such as QUIC streams.
//!
//! Specified by:
//! - `docs/05-wire-protocol.md`
//! - `docs/16-clone-blueprint.md`
//!
//! Status: MVP message set defined; auth messages are placeholders until phase 1.

pub mod codec;
#[doc(hidden)]
pub mod fuzz;
pub mod hello;
pub mod io;

pub use codec::{CodecError, FrameCodec, MAX_FRAME_LEN, ProtoCodec};
pub use io::{MessageReader, MessageWriter};

/// Generated protocol types, package `kanodesk.v1`.
#[allow(missing_docs, clippy::all, clippy::pedantic)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/kanodesk.v1.rs"));
}

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/05-wire-protocol.md", "docs/16-clone-blueprint.md"];

/// Implements `From<Arm> for Envelope` for each oneof arm.
macro_rules! envelope_from {
    ($envelope:ident, $oneof:ident, $($arm:ident => $ty:ident),+ $(,)?) => {
        $(
            impl From<v1::$ty> for v1::$envelope {
                fn from(m: v1::$ty) -> Self {
                    v1::$envelope { msg: Some(v1::$oneof::Msg::$arm(m)) }
                }
            }
        )+
    };
}

envelope_from!(ControlMessage, control_message,
    Hello => Hello,
    AuthStart => AuthStart,
    AuthFinish => AuthFinish,
    LoginResult => LoginResult,
    Permission => Permission,
    Close => Close,
    Ping => Ping,
    Pong => Pong,
    KeyframeRequest => KeyframeRequest,
    ReceiverReport => ReceiverReport,
    Displays => Displays,
    SelectDisplay => SelectDisplay,
    SetResolution => SetResolution,
);

envelope_from!(VideoMessage, video_message,
    StreamStart => StreamStart,
    Frame => VideoFrame,
);

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
    fn envelope_conversions_pick_the_right_arm() {
        let msg: v1::ControlMessage = v1::KeyframeRequest { display: 2 }.into();
        assert_eq!(
            msg.msg,
            Some(v1::control_message::Msg::KeyframeRequest(
                v1::KeyframeRequest { display: 2 }
            ))
        );
        let msg: v1::VideoMessage = v1::VideoFrame::default().into();
        assert!(matches!(msg.msg, Some(v1::video_message::Msg::Frame(_))));
    }

    #[test]
    fn enum_values_are_stable() {
        // Wire values must never change once shipped.
        assert_eq!(v1::Codec::Vp9 as i32, 1);
        assert_eq!(v1::StreamKind::Control as i32, 1);
        assert_eq!(v1::StreamKind::Video as i32, 2);
        assert_eq!(v1::Capability::KeyframeRequest as i32, 4);
        assert_eq!(v1::CloseReason::IncompatibleVersion as i32, 2);
        assert_eq!(v1::MouseButton::Forward as i32, 16);
    }
}
