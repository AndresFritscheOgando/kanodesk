//! The QUIC stream layout (16 §4), as far as the spike needs it.
//!
//! Every stream starts with one `StreamHeader` message and then carries its
//! envelope type, all framed by `kanodesk_proto::codec`. The client opens the
//! **control** stream (bidirectional, high priority, `ControlMessage`); the
//! server opens one **video** stream (unidirectional, `VideoMessage`) per
//! display.

use kanodesk_proto::v1::{
    ControlMessage, RelayMessage, RendezvousMessage, StreamHeader, StreamKind, VideoMessage,
};
use kanodesk_proto::{FrameCodec, MessageReader, MessageWriter, ProtoCodec};
use quinn::{Connection, RecvStream, SendStream};

use crate::TransportError;

/// Priority for the control stream. Higher is sent first (quinn semantics).
pub const CONTROL_PRIORITY: i32 = 10;
/// Priority for video streams.
pub const VIDEO_PRIORITY: i32 = 0;

/// Sends control messages.
pub type ControlWriter = MessageWriter<SendStream, ControlMessage>;
/// Receives control messages.
pub type ControlReader = MessageReader<RecvStream, ControlMessage>;
/// Sends video messages for one display.
pub type VideoWriter = MessageWriter<SendStream, VideoMessage>;
/// Receives video messages for one display.
pub type VideoReader = MessageReader<RecvStream, VideoMessage>;
/// Sends ID-server messages.
pub type RendezvousWriter = MessageWriter<SendStream, RendezvousMessage>;
/// Receives ID-server messages.
pub type RendezvousReader = MessageReader<RecvStream, RendezvousMessage>;

/// A unidirectional stream opened by the peer, identified by its header.
#[derive(Debug)]
pub enum IncomingStream {
    /// Encoded video for one display.
    Video {
        /// Display id on the server.
        display: u32,
        /// The stream's messages.
        reader: VideoReader,
    },
}

fn set_priority(stream: &SendStream, priority: i32) -> Result<(), TransportError> {
    stream
        .set_priority(priority)
        .map_err(|e| TransportError::Other(e.to_string()))
}

async fn write_header(
    send: SendStream,
    kind: StreamKind,
    display: u32,
) -> Result<MessageWriter<SendStream, StreamHeader>, TransportError> {
    let mut writer = MessageWriter::<_, StreamHeader>::new(send);
    writer
        .send(&StreamHeader {
            kind: kind as i32,
            display,
        })
        .await?;
    Ok(writer)
}

async fn read_header(
    recv: RecvStream,
) -> Result<(StreamHeader, MessageReader<RecvStream, StreamHeader>), TransportError> {
    read_header_with(recv, ProtoCodec::new()).await
}

async fn read_header_with(
    recv: RecvStream,
    codec: ProtoCodec<StreamHeader>,
) -> Result<(StreamHeader, MessageReader<RecvStream, StreamHeader>), TransportError> {
    let mut reader = MessageReader::with_codec(recv, codec);
    match reader.next().await? {
        Some(header) => Ok((header, reader)),
        None => Err(TransportError::StreamClosed),
    }
}

/// Client side: opens the control stream. The header is sent immediately, so
/// the server sees the stream at once.
pub async fn open_control(
    conn: &Connection,
) -> Result<(ControlWriter, ControlReader), TransportError> {
    let (send, recv) = conn.open_bi().await?;
    set_priority(&send, CONTROL_PRIORITY)?;
    let writer = write_header(send, StreamKind::Control, 0).await?;
    Ok((writer.into_typed(), MessageReader::new(recv)))
}

/// Server side: accepts the client's control stream.
pub async fn accept_control(
    conn: &Connection,
) -> Result<(ControlWriter, ControlReader), TransportError> {
    let (send, recv) = conn.accept_bi().await?;
    let (header, reader) = read_header(recv).await?;
    if header.kind() != StreamKind::Control {
        return Err(TransportError::BadStreamHeader(format!(
            "expected a control stream, got {:?}",
            header.kind()
        )));
    }
    set_priority(&send, CONTROL_PRIORITY)?;
    Ok((MessageWriter::new(send), reader.into_typed()))
}

/// Server side: opens the video stream for one display.
pub async fn open_video(conn: &Connection, display: u32) -> Result<VideoWriter, TransportError> {
    let send = conn.open_uni().await?;
    set_priority(&send, VIDEO_PRIORITY)?;
    Ok(write_header(send, StreamKind::Video, display)
        .await?
        .into_typed())
}

/// Client side: accepts the next unidirectional stream and reads its header.
pub async fn accept_uni(conn: &Connection) -> Result<IncomingStream, TransportError> {
    let recv = conn.accept_uni().await?;
    let (header, reader) = read_header(recv).await?;
    match header.kind() {
        StreamKind::Video => Ok(IncomingStream::Video {
            display: header.display,
            reader: reader.into_typed(),
        }),
        other => Err(TransportError::BadStreamHeader(format!(
            "unexpected unidirectional stream {other:?}"
        ))),
    }
}

/// Device side: opens the stream to the ID server.
pub async fn open_rendezvous(
    conn: &Connection,
) -> Result<(RendezvousWriter, RendezvousReader), TransportError> {
    let (send, recv) = conn.open_bi().await?;
    let writer = write_header(send, StreamKind::Rendezvous, 0).await?;
    Ok((writer.into_typed(), MessageReader::new(recv)))
}

/// Largest message accepted on a rendezvous stream. Requests are small, and
/// the ID server talks to untrusted clients, so the limit is tight.
pub const RENDEZVOUS_MAX_FRAME: usize = 64 * 1024;

/// ID-server side: accepts a device's rendezvous stream.
pub async fn accept_rendezvous(
    conn: &Connection,
) -> Result<(RendezvousWriter, RendezvousReader), TransportError> {
    let (send, recv) = conn.accept_bi().await?;
    let codec = ProtoCodec::with_frames(FrameCodec::with_max_len(RENDEZVOUS_MAX_FRAME));
    let (header, reader) = read_header_with(recv, codec).await?;
    if header.kind() != StreamKind::Rendezvous {
        return Err(TransportError::BadStreamHeader(format!(
            "expected a rendezvous stream, got {:?}",
            header.kind()
        )));
    }
    Ok((MessageWriter::new(send), reader.into_typed()))
}

/// Sends relay control messages.
pub type RelayWriter = MessageWriter<SendStream, RelayMessage>;
/// Receives relay control messages.
pub type RelayReader = MessageReader<RecvStream, RelayMessage>;

/// Largest message on a relay control stream.
pub const RELAY_MAX_FRAME: usize = 4 * 1024;

/// Peer side: opens the control stream to a relay.
pub async fn open_relay(conn: &Connection) -> Result<(RelayWriter, RelayReader), TransportError> {
    let (send, recv) = conn.open_bi().await?;
    let writer = write_header(send, StreamKind::Relay, 0).await?;
    Ok((writer.into_typed(), MessageReader::new(recv)))
}

/// Relay side: accepts a peer's control stream.
pub async fn accept_relay(conn: &Connection) -> Result<(RelayWriter, RelayReader), TransportError> {
    let (send, recv) = conn.accept_bi().await?;
    let codec = ProtoCodec::with_frames(FrameCodec::with_max_len(RELAY_MAX_FRAME));
    let (header, reader) = read_header_with(recv, codec).await?;
    if header.kind() != StreamKind::Relay {
        return Err(TransportError::BadStreamHeader(format!(
            "expected a relay stream, got {:?}",
            header.kind()
        )));
    }
    Ok((MessageWriter::new(send), reader.into_typed()))
}
