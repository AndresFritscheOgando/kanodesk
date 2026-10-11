//! The QUIC stream layout (16 §4), as far as the spike needs it.
//!
//! Every stream starts with one header frame: the magic `KD`, a kind byte,
//! and kind-specific fields. The client opens the **control** stream
//! (bidirectional, high priority); the server opens one **video** stream
//! (unidirectional) per display.

use quinn::{Connection, RecvStream, SendStream};

use crate::TransportError;
use crate::framing::{read_frame, write_frame};

const MAGIC: &[u8; 2] = b"KD";
const KIND_CONTROL: u8 = 0;
const KIND_VIDEO: u8 = 1;

/// Priority for the control stream. Higher is sent first (quinn semantics).
pub const CONTROL_PRIORITY: i32 = 10;
/// Priority for video streams.
pub const VIDEO_PRIORITY: i32 = 0;

/// What a stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Control messages, both directions.
    Control,
    /// Encoded video for one display, server to client.
    Video {
        /// Display id on the server.
        display: u32,
    },
}

impl StreamKind {
    /// Header bytes for this kind.
    pub fn encode(self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        match self {
            StreamKind::Control => out.push(KIND_CONTROL),
            StreamKind::Video { display } => {
                out.push(KIND_VIDEO);
                out.extend_from_slice(&display.to_be_bytes());
            }
        }
        out
    }

    /// Parses header bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, TransportError> {
        let bad = |why: &str| TransportError::BadStreamHeader(why.to_owned());
        if bytes.len() < 3 || &bytes[..2] != MAGIC {
            return Err(bad("missing magic"));
        }
        match (bytes[2], &bytes[3..]) {
            (KIND_CONTROL, []) => Ok(StreamKind::Control),
            (KIND_VIDEO, rest) if rest.len() == 4 => Ok(StreamKind::Video {
                display: u32::from_be_bytes(rest.try_into().expect("length checked")),
            }),
            (KIND_CONTROL | KIND_VIDEO, _) => Err(bad("wrong length")),
            (kind, _) => Err(bad(&format!("unknown kind {kind}"))),
        }
    }
}

async fn read_header(recv: &mut RecvStream) -> Result<StreamKind, TransportError> {
    let mut buf = Vec::new();
    if !read_frame(recv, &mut buf).await? {
        return Err(TransportError::StreamClosed);
    }
    StreamKind::decode(&buf)
}

/// Client side: opens the control stream. The header is sent immediately, so
/// the server sees the stream at once.
pub async fn open_control(conn: &Connection) -> Result<(SendStream, RecvStream), TransportError> {
    let (mut send, recv) = conn.open_bi().await?;
    send.set_priority(CONTROL_PRIORITY)
        .map_err(|e| TransportError::Other(e.to_string()))?;
    write_frame(&mut send, &StreamKind::Control.encode()).await?;
    Ok((send, recv))
}

/// Server side: accepts the client's control stream.
pub async fn accept_control(conn: &Connection) -> Result<(SendStream, RecvStream), TransportError> {
    let (send, mut recv) = conn.accept_bi().await?;
    match read_header(&mut recv).await? {
        StreamKind::Control => {
            send.set_priority(CONTROL_PRIORITY)
                .map_err(|e| TransportError::Other(e.to_string()))?;
            Ok((send, recv))
        }
        other => Err(TransportError::BadStreamHeader(format!(
            "expected control stream, got {other:?}"
        ))),
    }
}

/// Server side: opens the video stream for one display.
pub async fn open_video(conn: &Connection, display: u32) -> Result<SendStream, TransportError> {
    let mut send = conn.open_uni().await?;
    send.set_priority(VIDEO_PRIORITY)
        .map_err(|e| TransportError::Other(e.to_string()))?;
    write_frame(&mut send, &StreamKind::Video { display }.encode()).await?;
    Ok(send)
}

/// Client side: accepts the next unidirectional stream and reads its header.
pub async fn accept_uni(conn: &Connection) -> Result<(StreamKind, RecvStream), TransportError> {
    let mut recv = conn.accept_uni().await?;
    let kind = read_header(&mut recv).await?;
    Ok((kind, recv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_round_trip() {
        for kind in [
            StreamKind::Control,
            StreamKind::Video { display: 0 },
            StreamKind::Video { display: 7 },
        ] {
            assert_eq!(StreamKind::decode(&kind.encode()).unwrap(), kind);
        }
    }

    #[test]
    fn rejects_malformed_headers() {
        assert!(StreamKind::decode(b"").is_err());
        assert!(StreamKind::decode(b"XX\x00").is_err());
        assert!(StreamKind::decode(b"KD\x09").is_err());
        assert!(StreamKind::decode(b"KD\x01\x00\x00").is_err());
        assert!(StreamKind::decode(b"KD\x00\x01").is_err());
    }
}
