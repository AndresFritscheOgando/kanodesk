//! Framing for QUIC streams: a `u32` big-endian length, then the payload.
//!
//! The length is checked against a maximum **before** any buffer is reserved,
//! so a hostile header cannot make the receiver allocate. [`FrameCodec`]
//! handles raw frames; [`ProtoCodec`] adds prost (de)serialisation. Both are
//! tokio-util codecs, usable with `FramedRead`/`FramedWrite` or with
//! [`crate::io`].
//!
//! Unlike RustDesk's variable 1–4 byte header (05 §1), the header here is a
//! fixed 4 bytes: QUIC already frames streams, and a fixed size keeps the
//! decoder trivial.

use std::marker::PhantomData;

use bytes::{Buf, BufMut, BytesMut};
use prost::Message;
use tokio_util::codec::{Decoder, Encoder};

/// Bytes in a frame header.
pub const HEADER_LEN: usize = 4;
/// Default largest payload: 16 MiB, well above a 4K keyframe.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Framing and message errors.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The underlying stream failed.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    /// A frame is larger than the codec's limit.
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    FrameTooLarge {
        /// Announced or attempted length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The stream ended in the middle of a frame.
    #[error("stream ended mid-frame")]
    Truncated,
    /// A frame did not contain a valid protobuf message.
    #[error("protobuf: {0}")]
    Decode(#[from] prost::DecodeError),
}

/// Raw length-prefixed frames.
#[derive(Debug, Clone, Copy)]
pub struct FrameCodec {
    max_len: usize,
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameCodec {
    /// A codec with the default [`MAX_FRAME_LEN`].
    pub fn new() -> Self {
        Self::with_max_len(MAX_FRAME_LEN)
    }

    /// A codec with a custom payload limit (at most `u32::MAX`).
    pub fn with_max_len(max_len: usize) -> Self {
        FrameCodec {
            max_len: max_len.min(u32::MAX as usize),
        }
    }

    /// The payload limit.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    fn check(&self, len: usize) -> Result<(), CodecError> {
        if len > self.max_len {
            return Err(CodecError::FrameTooLarge {
                len,
                max: self.max_len,
            });
        }
        Ok(())
    }
}

impl Decoder for FrameCodec {
    type Item = BytesMut;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<BytesMut>, CodecError> {
        if src.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = u32::from_be_bytes(src[..HEADER_LEN].try_into().expect("4 bytes")) as usize;
        self.check(len)?;
        let total = HEADER_LEN + len;
        if src.len() < total {
            // Safe to reserve: `len` is within the limit.
            src.reserve(total - src.len());
            return Ok(None);
        }
        src.advance(HEADER_LEN);
        Ok(Some(src.split_to(len)))
    }

    fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<BytesMut>, CodecError> {
        match self.decode(src)? {
            Some(frame) => Ok(Some(frame)),
            None if src.is_empty() => Ok(None),
            None => Err(CodecError::Truncated),
        }
    }
}

impl Encoder<&[u8]> for FrameCodec {
    type Error = CodecError;

    fn encode(&mut self, payload: &[u8], dst: &mut BytesMut) -> Result<(), CodecError> {
        self.check(payload.len())?;
        dst.reserve(HEADER_LEN + payload.len());
        dst.put_u32(payload.len() as u32);
        dst.put_slice(payload);
        Ok(())
    }
}

/// Length-prefixed protobuf messages of type `T`.
#[derive(Debug)]
pub struct ProtoCodec<T> {
    frames: FrameCodec,
    _message: PhantomData<fn() -> T>,
}

impl<T> Clone for ProtoCodec<T> {
    fn clone(&self) -> Self {
        Self::with_frames(self.frames)
    }
}

impl<T> Default for ProtoCodec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ProtoCodec<T> {
    /// A codec with the default [`MAX_FRAME_LEN`].
    pub fn new() -> Self {
        Self::with_frames(FrameCodec::new())
    }

    /// A codec over a configured [`FrameCodec`].
    pub fn with_frames(frames: FrameCodec) -> Self {
        ProtoCodec {
            frames,
            _message: PhantomData,
        }
    }

    /// The same framing for another message type.
    pub fn retype<U>(&self) -> ProtoCodec<U> {
        ProtoCodec::with_frames(self.frames)
    }
}

impl<T: Message + Default> Decoder for ProtoCodec<T> {
    type Item = T;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<T>, CodecError> {
        match self.frames.decode(src)? {
            Some(frame) => Ok(Some(T::decode(frame.freeze())?)),
            None => Ok(None),
        }
    }

    fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<T>, CodecError> {
        match self.frames.decode_eof(src)? {
            Some(frame) => Ok(Some(T::decode(frame.freeze())?)),
            None => Ok(None),
        }
    }
}

impl<T: Message> Encoder<&T> for ProtoCodec<T> {
    type Error = CodecError;

    fn encode(&mut self, msg: &T, dst: &mut BytesMut) -> Result<(), CodecError> {
        let len = msg.encoded_len();
        self.frames.check(len)?;
        dst.reserve(HEADER_LEN + len);
        dst.put_u32(len as u32);
        msg.encode(dst).expect("capacity reserved above");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{
        self, ControlMessage, InputMessage, VideoMessage, control_message, input_message,
    };
    use proptest::prelude::*;

    fn encode_frames(payloads: &[Vec<u8>]) -> BytesMut {
        let mut codec = FrameCodec::new();
        let mut out = BytesMut::new();
        for p in payloads {
            codec.encode(p.as_slice(), &mut out).unwrap();
        }
        out
    }

    /// Feeds `bytes` to the decoder in chunks of the given sizes, as a socket would.
    fn decode_chunked(bytes: &[u8], chunks: &[usize]) -> Result<Vec<Vec<u8>>, CodecError> {
        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        let mut out = Vec::new();
        let mut rest = bytes;
        let mut sizes = chunks.iter().cycle();
        while !rest.is_empty() {
            let n = (*sizes.next().unwrap()).clamp(1, rest.len());
            buf.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            while let Some(frame) = codec.decode(&mut buf)? {
                out.push(frame.to_vec());
            }
        }
        if let Some(frame) = codec.decode_eof(&mut buf)? {
            out.push(frame.to_vec());
        }
        Ok(out)
    }

    #[test]
    fn empty_payload_is_a_valid_frame() {
        let bytes = encode_frames(&[vec![]]);
        assert_eq!(&bytes[..], &[0, 0, 0, 0]);
        assert_eq!(
            decode_chunked(&bytes, &[1]).unwrap(),
            vec![Vec::<u8>::new()]
        );
    }

    #[test]
    fn oversized_header_fails_before_allocating() {
        let mut codec = FrameCodec::with_max_len(10);
        let mut buf = BytesMut::from(&[0, 0, 0, 11][..]);
        let before = buf.capacity();
        assert!(matches!(
            codec.decode(&mut buf),
            Err(CodecError::FrameTooLarge { len: 11, max: 10 })
        ));
        assert_eq!(buf.capacity(), before);

        let mut big = BytesMut::from(&u32::MAX.to_be_bytes()[..]);
        assert!(FrameCodec::new().decode(&mut big).is_err());
    }

    #[test]
    fn encoder_enforces_the_limit() {
        let mut codec = FrameCodec::with_max_len(3);
        let mut out = BytesMut::new();
        assert!(codec.encode(&[1, 2, 3][..], &mut out).is_ok());
        assert!(matches!(
            codec.encode(&[1, 2, 3, 4][..], &mut out),
            Err(CodecError::FrameTooLarge { .. })
        ));
        let mut proto = ProtoCodec::<v1::TextInput>::with_frames(FrameCodec::with_max_len(3));
        let text = v1::TextInput {
            text: "too long".into(),
        };
        assert!(proto.encode(&text, &mut out).is_err());
    }

    #[test]
    fn eof_inside_a_frame_is_truncation() {
        let bytes = encode_frames(&[vec![1, 2, 3]]);
        let mut codec = FrameCodec::new();
        let mut header_only = BytesMut::from(&bytes[..2]);
        assert!(matches!(
            codec.decode_eof(&mut header_only),
            Err(CodecError::Truncated)
        ));
        let mut cut = BytesMut::from(&bytes[..5]);
        assert!(matches!(
            codec.decode_eof(&mut cut),
            Err(CodecError::Truncated)
        ));
    }

    #[test]
    fn garbage_payload_is_a_decode_error() {
        let mut codec = ProtoCodec::<ControlMessage>::new();
        // Field 1, wire type 2 (length-delimited), length 200 with no bytes after it.
        let mut buf = encode_frames(&[vec![0x0a, 0xc8, 0x01]]);
        assert!(matches!(codec.decode(&mut buf), Err(CodecError::Decode(_))));
    }

    // ---- property tests ----

    fn arb_hello() -> impl Strategy<Value = v1::Hello> {
        (
            any::<u32>(),
            any::<u32>(),
            prop::collection::vec(0i32..12, 0..8),
            ".{0,20}",
        )
            .prop_map(|(version, min_version, capabilities, agent)| v1::Hello {
                version,
                min_version,
                capabilities,
                agent,
            })
    }

    fn arb_control() -> impl Strategy<Value = ControlMessage> {
        prop_oneof![
            arb_hello().prop_map(ControlMessage::from),
            any::<u32>().prop_map(|display| v1::KeyframeRequest { display }.into()),
            any::<u64>().prop_map(|timestamp_us| v1::Ping { timestamp_us }.into()),
            (0i32..6, ".{0,40}").prop_map(|(reason, detail)| v1::Close { reason, detail }.into()),
            (any::<u32>(), any::<u32>(), any::<u64>()).prop_map(|(display, lost, last_seq)| {
                v1::ReceiverReport {
                    display,
                    lost,
                    jitter_ms: lost as f32 / 7.0,
                    decode_ms: 1.5,
                    rendered_fps: 30.0,
                    last_seq,
                }
                .into()
            }),
            prop::collection::vec(any::<u32>(), 0..8)
                .prop_map(|ids| v1::SelectDisplay { ids }.into()),
            Just(ControlMessage { msg: None }),
        ]
    }

    fn arb_video() -> impl Strategy<Value = VideoMessage> {
        prop_oneof![
            (any::<u32>(), 0i32..4, any::<u32>(), any::<u32>()).prop_map(
                |(display, codec, width, height)| v1::StreamStart {
                    display,
                    codec,
                    width,
                    height
                }
                .into()
            ),
            (
                any::<u64>(),
                any::<i64>(),
                any::<bool>(),
                prop::collection::vec(any::<u8>(), 0..4096),
                any::<u64>(),
                any::<u32>()
            )
                .prop_map(|(seq, pts_ms, key, data, capture_us, host_delay_us)| {
                    v1::VideoFrame {
                        display: 0,
                        seq,
                        pts_ms,
                        key,
                        data,
                        capture_us,
                        host_delay_us,
                    }
                    .into()
                }),
        ]
    }

    fn arb_input() -> impl Strategy<Value = InputMessage> {
        let mouse = (
            0i32..6,
            any::<u32>(),
            any::<i32>(),
            any::<i32>(),
            any::<i32>(),
            any::<i32>(),
        )
            .prop_map(|(kind, buttons, x, y, dx, dy)| {
                input_message::Msg::Mouse(v1::MouseEvent {
                    kind,
                    buttons,
                    x,
                    y,
                    dx,
                    dy,
                    display: 0,
                })
            });
        let key = (any::<u32>(), any::<bool>(), any::<u32>(), 0i32..3).prop_map(
            |(code, down, modifiers, mode)| {
                input_message::Msg::Key(v1::KeyEvent {
                    code,
                    down,
                    modifiers,
                    mode,
                })
            },
        );
        let text = ".{0,64}".prop_map(|text| input_message::Msg::Text(v1::TextInput { text }));
        (any::<u64>(), prop_oneof![mouse, key, text]).prop_map(|(seq, msg)| InputMessage {
            seq,
            msg: Some(msg),
        })
    }

    fn round_trip<T>(msgs: &[T], chunks: &[usize]) -> Vec<T>
    where
        T: Message + Default,
    {
        let mut codec = ProtoCodec::<T>::new();
        let mut wire = BytesMut::new();
        for m in msgs {
            codec.encode(m, &mut wire).unwrap();
        }
        decode_chunked(&wire, chunks)
            .unwrap()
            .into_iter()
            .map(|f| T::decode(f.as_slice()).unwrap())
            .collect()
    }

    proptest! {
        #[test]
        fn frames_survive_any_chunking(
            payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..3000), 0..16),
            chunks in prop::collection::vec(1usize..5000, 1..8),
        ) {
            let wire = encode_frames(&payloads);
            prop_assert_eq!(wire.len(), payloads.iter().map(|p| p.len() + HEADER_LEN).sum::<usize>());
            prop_assert_eq!(decode_chunked(&wire, &chunks).unwrap(), payloads);
        }

        #[test]
        fn control_messages_round_trip(
            msgs in prop::collection::vec(arb_control(), 0..12),
            chunks in prop::collection::vec(1usize..64, 1..4),
        ) {
            prop_assert_eq!(round_trip(&msgs, &chunks), msgs);
        }

        #[test]
        fn video_messages_round_trip(
            msgs in prop::collection::vec(arb_video(), 0..6),
            chunks in prop::collection::vec(1usize..2000, 1..4),
        ) {
            prop_assert_eq!(round_trip(&msgs, &chunks), msgs);
        }

        #[test]
        fn input_messages_round_trip(
            msgs in prop::collection::vec(arb_input(), 0..12),
            chunks in prop::collection::vec(1usize..32, 1..4),
        ) {
            prop_assert_eq!(round_trip(&msgs, &chunks), msgs);
        }

        #[test]
        fn decoder_never_panics_on_random_bytes(
            bytes in prop::collection::vec(any::<u8>(), 0..2048),
        ) {
            crate::fuzz::decode_all(&bytes);
        }

        #[test]
        fn hello_arm_is_preserved(hello in arb_hello()) {
            let msgs = round_trip(&[ControlMessage::from(hello.clone())], &[3]);
            prop_assert_eq!(&msgs[0].msg, &Some(control_message::Msg::Hello(hello)));
        }
    }
}
