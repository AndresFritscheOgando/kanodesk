//! Length-prefixed frames: a `u32` big-endian length, then that many bytes.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::TransportError;

/// Largest frame accepted or sent: 16 MiB, well above a 4K keyframe.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Writes one frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> Result<(), TransportError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(TransportError::FrameTooLarge {
            len: payload.len(),
            max: MAX_FRAME_LEN,
        });
    }
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(payload).await?;
    Ok(())
}

/// Reads one frame into `buf` (replacing its contents).
///
/// Returns `Ok(false)` if the stream ended cleanly before a new frame, and
/// [`TransportError::Truncated`] if it ended inside one.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<bool, TransportError> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        let n = reader.read(&mut header[filled..]).await?;
        if n == 0 {
            return if filled == 0 {
                Ok(false)
            } else {
                Err(TransportError::Truncated)
            };
        }
        filled += n;
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_LEN {
        return Err(TransportError::FrameTooLarge {
            len,
            max: MAX_FRAME_LEN,
        });
    }
    buf.resize(len, 0);
    match reader.read_exact(buf).await {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(TransportError::Truncated),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_several_frames() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let frames: [&[u8]; 3] = [b"hello", b"", &[7u8; 600]];
        let writer = tokio::spawn(async move {
            for f in frames {
                write_frame(&mut a, f).await.unwrap();
            }
        });
        let mut buf = Vec::new();
        for f in frames {
            assert!(read_frame(&mut b, &mut buf).await.unwrap());
            assert_eq!(buf, f);
        }
        writer.await.unwrap();
        // Writer dropped: clean end of stream.
        assert!(!read_frame(&mut b, &mut buf).await.unwrap());
    }

    #[tokio::test]
    async fn detects_truncation_in_header_and_body() {
        let mut buf = Vec::new();
        let mut short_header: &[u8] = &[0, 0];
        assert!(matches!(
            read_frame(&mut short_header, &mut buf).await,
            Err(TransportError::Truncated)
        ));
        let mut short_body: &[u8] = &[0, 0, 0, 5, 1, 2];
        assert!(matches!(
            read_frame(&mut short_body, &mut buf).await,
            Err(TransportError::Truncated)
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_frames() {
        let mut buf = Vec::new();
        let header = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes();
        let mut reader: &[u8] = &header;
        assert!(matches!(
            read_frame(&mut reader, &mut buf).await,
            Err(TransportError::FrameTooLarge { .. })
        ));

        let mut sink = tokio::io::sink();
        let big = vec![0u8; MAX_FRAME_LEN + 1];
        assert!(matches!(
            write_frame(&mut sink, &big).await,
            Err(TransportError::FrameTooLarge { .. })
        ));
    }
}
