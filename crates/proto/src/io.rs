//! Async message reader and writer over byte streams (QUIC streams in practice).

use bytes::BytesMut;
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::codec::{Decoder, Encoder};

use crate::codec::{CodecError, ProtoCodec};

/// Reads length-prefixed messages of type `T`.
#[derive(Debug)]
pub struct MessageReader<R, T> {
    inner: R,
    buf: BytesMut,
    codec: ProtoCodec<T>,
}

impl<R, T> MessageReader<R, T> {
    /// Wraps a byte stream.
    pub fn new(inner: R) -> Self {
        Self::with_codec(inner, ProtoCodec::new())
    }

    /// Wraps a byte stream with a configured codec.
    pub fn with_codec(inner: R, codec: ProtoCodec<T>) -> Self {
        MessageReader {
            inner,
            buf: BytesMut::new(),
            codec,
        }
    }

    /// Switches to another message type, keeping any bytes already buffered.
    /// Used after a stream's header.
    pub fn into_typed<U>(self) -> MessageReader<R, U> {
        MessageReader {
            codec: self.codec.retype(),
            inner: self.inner,
            buf: self.buf,
        }
    }

    /// The underlying stream.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }
}

impl<R: AsyncRead + Unpin, T: Message + Default> MessageReader<R, T> {
    /// Reads the next message. `Ok(None)` means the stream ended cleanly
    /// between messages; ending inside one is [`CodecError::Truncated`].
    pub async fn next(&mut self) -> Result<Option<T>, CodecError> {
        loop {
            if let Some(msg) = self.codec.decode(&mut self.buf)? {
                return Ok(Some(msg));
            }
            if self.inner.read_buf(&mut self.buf).await? == 0 {
                return self.codec.decode_eof(&mut self.buf);
            }
        }
    }
}

/// Writes length-prefixed messages of type `T`.
#[derive(Debug)]
pub struct MessageWriter<W, T> {
    inner: W,
    buf: BytesMut,
    codec: ProtoCodec<T>,
}

impl<W, T> MessageWriter<W, T> {
    /// Wraps a byte stream.
    pub fn new(inner: W) -> Self {
        Self::with_codec(inner, ProtoCodec::new())
    }

    /// Wraps a byte stream with a configured codec.
    pub fn with_codec(inner: W, codec: ProtoCodec<T>) -> Self {
        MessageWriter {
            inner,
            buf: BytesMut::new(),
            codec,
        }
    }

    /// Switches to another message type.
    pub fn into_typed<U>(self) -> MessageWriter<W, U> {
        MessageWriter {
            codec: self.codec.retype(),
            inner: self.inner,
            buf: self.buf,
        }
    }

    /// The underlying stream.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Unwraps the stream.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: AsyncWrite + Unpin, T: Message> MessageWriter<W, T> {
    /// Writes one message (header and payload in a single write).
    pub async fn send(&mut self, msg: &T) -> Result<(), CodecError> {
        self.buf.clear();
        self.codec.encode(msg, &mut self.buf)?;
        self.inner.write_all(&self.buf).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{self, ControlMessage, StreamHeader, StreamKind};

    #[tokio::test]
    async fn header_then_messages_in_one_read() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut header = MessageWriter::<_, StreamHeader>::new(a);
        header
            .send(&StreamHeader {
                kind: StreamKind::Control as i32,
                display: 0,
            })
            .await
            .unwrap();
        let mut control: MessageWriter<_, ControlMessage> = header.into_typed();
        for display in 0..3 {
            control
                .send(&v1::KeyframeRequest { display }.into())
                .await
                .unwrap();
        }
        drop(control);

        // The reader may pull header and messages in one read; switching type
        // must keep the buffered bytes.
        let mut reader = MessageReader::<_, StreamHeader>::new(b);
        let h = reader.next().await.unwrap().unwrap();
        assert_eq!(h.kind(), StreamKind::Control);
        let mut reader: MessageReader<_, ControlMessage> = reader.into_typed();
        for display in 0..3 {
            let msg = reader.next().await.unwrap().unwrap();
            assert_eq!(msg, v1::KeyframeRequest { display }.into());
        }
        assert!(reader.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn truncated_stream_is_an_error() {
        let mut reader = MessageReader::<_, ControlMessage>::new(&[0u8, 0, 0, 9, 1][..]);
        assert!(matches!(reader.next().await, Err(CodecError::Truncated)));
    }
}
