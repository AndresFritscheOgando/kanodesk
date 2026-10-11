//! Spike wire messages, big-endian, carried in length-prefixed frames.
//!
//! - Control stream, viewer → host: [`Control`].
//! - Video stream, host → viewer: [`VideoHeader`] followed by the VP9 frame.

/// Malformed message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed {0} message")]
pub struct WireError(&'static str);

const TAG_REQUEST_KEYFRAME: u8 = 1;

/// Control messages from viewer to host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Send a keyframe for this display as soon as possible (on connect and
    /// after a decode error).
    RequestKeyframe {
        /// Display id.
        display: u32,
    },
}

impl Control {
    /// Serialises the message.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Control::RequestKeyframe { display } => {
                let mut out = vec![TAG_REQUEST_KEYFRAME];
                out.extend_from_slice(&display.to_be_bytes());
                out
            }
        }
    }

    /// Parses a message.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        match bytes {
            [TAG_REQUEST_KEYFRAME, rest @ ..] if rest.len() == 4 => Ok(Control::RequestKeyframe {
                display: u32::from_be_bytes(rest.try_into().expect("length checked")),
            }),
            _ => Err(WireError("control")),
        }
    }
}

/// Per-frame metadata sent ahead of each encoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoHeader {
    /// Sequence number, starting at 0 per stream.
    pub frame_no: u64,
    /// When the captured image was presented on the host (µs, QPC clock).
    pub capture_us: u64,
    /// When encoding finished on the host (µs, QPC clock).
    pub encoded_us: u64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Whether this is a keyframe.
    pub key: bool,
}

/// Size of an encoded [`VideoHeader`].
pub const VIDEO_HEADER_LEN: usize = 8 * 3 + 4 * 2 + 1;

/// Serialises a header and frame data into one message.
pub fn encode_video(header: &VideoHeader, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(VIDEO_HEADER_LEN + data.len());
    out.extend_from_slice(&header.frame_no.to_be_bytes());
    out.extend_from_slice(&header.capture_us.to_be_bytes());
    out.extend_from_slice(&header.encoded_us.to_be_bytes());
    out.extend_from_slice(&header.width.to_be_bytes());
    out.extend_from_slice(&header.height.to_be_bytes());
    out.push(header.key as u8);
    out.extend_from_slice(data);
    out
}

/// Splits a video message into header and frame data.
pub fn decode_video(bytes: &[u8]) -> Result<(VideoHeader, &[u8]), WireError> {
    if bytes.len() < VIDEO_HEADER_LEN {
        return Err(WireError("video"));
    }
    let u64_at = |i: usize| u64::from_be_bytes(bytes[i..i + 8].try_into().expect("in bounds"));
    let u32_at = |i: usize| u32::from_be_bytes(bytes[i..i + 4].try_into().expect("in bounds"));
    let key = match bytes[32] {
        0 => false,
        1 => true,
        _ => return Err(WireError("video")),
    };
    Ok((
        VideoHeader {
            frame_no: u64_at(0),
            capture_us: u64_at(8),
            encoded_us: u64_at(16),
            width: u32_at(24),
            height: u32_at(28),
            key,
        },
        &bytes[VIDEO_HEADER_LEN..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_round_trip() {
        let msg = Control::RequestKeyframe { display: 2 };
        assert_eq!(Control::decode(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn control_rejects_garbage() {
        assert!(Control::decode(&[]).is_err());
        assert!(Control::decode(&[1, 0, 0]).is_err());
        assert!(Control::decode(&[9, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn video_round_trip() {
        let header = VideoHeader {
            frame_no: 42,
            capture_us: 1_000_000,
            encoded_us: 1_006_000,
            width: 1920,
            height: 1080,
            key: true,
        };
        let msg = encode_video(&header, b"vp9 bytes");
        assert_eq!(msg.len(), VIDEO_HEADER_LEN + 9);
        let (decoded, data) = decode_video(&msg).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(data, b"vp9 bytes");
    }

    #[test]
    fn video_rejects_short_or_bad_flag() {
        assert!(decode_video(&[0; VIDEO_HEADER_LEN - 1]).is_err());
        let mut msg = encode_video(&VideoHeader::default(), b"");
        msg[32] = 7;
        assert!(decode_video(&msg).is_err());
    }
}
