//! Shared body of the `decode_frames` fuzz target (`crates/proto/fuzz`).
//!
//! It lives here so the stable test suite runs it too (see the
//! `decoder_never_panics_on_random_bytes` proptest), not only `cargo fuzz`.

use bytes::BytesMut;
use prost::Message;
use tokio_util::codec::Decoder;

use crate::codec::{FrameCodec, ProtoCodec};
use crate::v1::{ControlMessage, InputMessage, StreamHeader, VideoMessage};

/// Payload limit used while fuzzing, small enough to reach the size check often.
pub const FUZZ_MAX_LEN: usize = 64 * 1024;

/// Treats `data` as a stream arriving in chunks (the first byte picks the
/// chunk size) and decodes every frame as each message type.
///
/// Invariants checked: the decoder never panics; it never returns a frame
/// over the limit; any message it decodes re-encodes to bytes that decode to
/// the same encoding again (so nothing is lost or invented).
pub fn decode_all(data: &[u8]) {
    let Some((&first, mut rest)) = data.split_first() else {
        return;
    };
    let chunk = first as usize % 64 + 1;
    let mut frames = FrameCodec::with_max_len(FUZZ_MAX_LEN);
    let mut buf = BytesMut::new();
    while !rest.is_empty() {
        let n = chunk.min(rest.len());
        buf.extend_from_slice(&rest[..n]);
        rest = &rest[n..];
        loop {
            match frames.decode(&mut buf) {
                Ok(Some(frame)) => {
                    assert!(frame.len() <= FUZZ_MAX_LEN);
                    check::<StreamHeader>(&frame);
                    check::<ControlMessage>(&frame);
                    check::<VideoMessage>(&frame);
                    check::<InputMessage>(&frame);
                }
                Ok(None) => break,
                // A real reader closes the stream on error.
                Err(_) => return,
            }
        }
    }
    let _ = frames.decode_eof(&mut buf);

    // Also run the typed codec over the whole input at once (no chunking).
    let mut typed =
        ProtoCodec::<ControlMessage>::with_frames(FrameCodec::with_max_len(FUZZ_MAX_LEN));
    let mut whole = BytesMut::from(&data[1..]);
    while let Ok(Some(_)) = typed.decode(&mut whole) {}
}

fn check<T: Message + Default>(frame: &[u8]) {
    if let Ok(msg) = T::decode(frame) {
        let once = msg.encode_to_vec();
        let again = T::decode(once.as_slice())
            .expect("re-encoded message decodes")
            .encode_to_vec();
        assert_eq!(once, again, "encoding is not stable");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_edge_inputs() {
        decode_all(&[]);
        decode_all(&[0]);
        decode_all(&[0, 0, 0, 0, 0]);
        decode_all(&[7, 0xff, 0xff, 0xff, 0xff]);
        // A valid frame holding a valid ControlMessage.
        decode_all(&[3, 0, 0, 0, 2, 0x4a, 0x00]);
    }
}
