//! QUIC endpoints: TLS 1.3 with the ring provider, ALPN, and transport tuning.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, IdleTimeout, TransportConfig, VarInt};

use crate::TransportError;
use crate::tls::{Identity, PinVerifier, SERVER_NAME, ServerTrust};

/// ALPN protocol id. Not versioned: versions are negotiated by `Hello` (kanodesk_proto::hello).
pub const ALPN: &[u8] = b"kanodesk";

/// How long a silent connection survives.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Keep-alive interval; well under [`IDLE_TIMEOUT`].
pub const KEEP_ALIVE: Duration = Duration::from_secs(5);

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport_config() -> Result<TransportConfig, TransportError> {
    let mut config = TransportConfig::default();
    config
        .max_idle_timeout(Some(
            IdleTimeout::try_from(IDLE_TIMEOUT)
                .map_err(|e| TransportError::Other(e.to_string()))?,
        ))
        .keep_alive_interval(Some(KEEP_ALIVE))
        // Room for a few 4K keyframes in flight per stream.
        .stream_receive_window(VarInt::from_u32(8 * 1024 * 1024))
        .receive_window(VarInt::from_u32(32 * 1024 * 1024))
        .send_window(32 * 1024 * 1024);
    Ok(config)
}

/// QUIC server settings presenting `identity`.
pub(crate) fn server_config(identity: &Identity) -> Result<quinn::ServerConfig, TransportError> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert.clone()], identity.private_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto =
        QuicServerConfig::try_from(tls).map_err(|e| TransportError::Other(e.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(Arc::new(transport_config()?));
    Ok(config)
}

/// QUIC client settings trusting servers according to `trust`.
pub(crate) fn client_config(trust: ServerTrust) -> Result<quinn::ClientConfig, TransportError> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifier::new(trust, provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto =
        QuicClientConfig::try_from(tls).map_err(|e| TransportError::Other(e.to_string()))?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(Arc::new(transport_config()?));
    Ok(config)
}

/// Creates a server endpoint bound to `bind`, presenting `identity`.
/// Must be called inside a tokio runtime.
pub fn server(bind: SocketAddr, identity: &Identity) -> Result<Endpoint, TransportError> {
    Ok(Endpoint::server(server_config(identity)?, bind)?)
}

/// Creates a client endpoint bound to `bind` (use port 0) that trusts servers
/// according to `trust`. Must be called inside a tokio runtime.
pub fn client(bind: SocketAddr, trust: ServerTrust) -> Result<Endpoint, TransportError> {
    let mut endpoint = Endpoint::client(bind)?;
    endpoint.set_default_client_config(client_config(trust)?);
    Ok(endpoint)
}

/// Connects to a server.
pub async fn connect(endpoint: &Endpoint, addr: SocketAddr) -> Result<Connection, TransportError> {
    Ok(endpoint.connect(addr, SERVER_NAME)?.await?)
}

/// TLS exporter label for the Noise channel binding (RFC 5705 / RFC 8446 §7.5).
pub const CHANNEL_BINDING_LABEL: &[u8] = b"EXPORTER-kanodesk-noise-binding-v1";

/// 32 bytes unique to this QUIC/TLS session and identical on both ends. Fed
/// into the Noise prologue, it ties end-to-end authentication to this exact
/// transport session: a man-in-the-middle with two TLS sessions gets two
/// different values and the Noise handshake fails.
pub fn channel_binding(conn: &Connection) -> Result<[u8; 32], TransportError> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, CHANNEL_BINDING_LABEL, b"")
        .map_err(|e| TransportError::Other(format!("TLS exporter: {e:?}")))?;
    Ok(out)
}

/// Closes one connection immediately with application code 0.
pub fn close(conn: &Connection, reason: &[u8]) {
    conn.close(VarInt::from_u32(0), reason);
}

/// Whether the peer closed `conn` deliberately (rather than the connection
/// failing, timing out or being closed here).
pub fn closed_by_peer(conn: &Connection) -> bool {
    matches!(
        conn.close_reason(),
        Some(quinn::ConnectionError::ApplicationClosed(_))
    )
}

/// The reason the peer gave when it closed `conn`, if it closed it
/// deliberately.
pub fn peer_close_message(conn: &Connection) -> Option<String> {
    match conn.close_reason() {
        Some(quinn::ConnectionError::ApplicationClosed(c)) => {
            Some(String::from_utf8_lossy(&c.reason).into_owned())
        }
        _ => None,
    }
}

/// Closes every connection on `endpoint` and waits (up to 500 ms) for the
/// close frames to go out, so peers notice at once instead of at the idle timeout.
pub async fn shutdown(endpoint: &Endpoint, reason: &[u8]) {
    endpoint.close(VarInt::from_u32(0), reason);
    let _ = tokio::time::timeout(Duration::from_millis(500), endpoint.wait_idle()).await;
}

/// Waits for the next incoming connection. `None` when the endpoint is closed.
pub async fn accept(endpoint: &Endpoint) -> Option<Result<Connection, TransportError>> {
    let incoming = endpoint.accept().await?;
    Some(incoming.await.map_err(TransportError::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::{IncomingStream, accept_control, accept_uni, open_control, open_video};
    use kanodesk_proto::v1::{self, ControlMessage, VideoMessage};
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn localhost(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    fn frame(seq: u64) -> VideoMessage {
        v1::VideoFrame {
            display: 3,
            seq,
            data: vec![seq as u8; 1000 * seq as usize],
            ..Default::default()
        }
        .into()
    }

    #[tokio::test]
    async fn control_and_video_streams_over_loopback() {
        let identity = Identity::self_signed().unwrap();
        let server = server(localhost(0), &identity).unwrap();
        let addr = server.local_addr().unwrap();

        let host = tokio::spawn(async move {
            let conn = accept(&server).await.unwrap().unwrap();
            let (mut ctl_send, mut ctl_recv) = accept_control(&conn).await.unwrap();
            let request = ctl_recv.next().await.unwrap().unwrap();
            assert_eq!(request, v1::KeyframeRequest { display: 3 }.into());
            ctl_send
                .send(&v1::Pong { timestamp_us: 7 }.into())
                .await
                .unwrap();

            let mut video = open_video(&conn, 3).await.unwrap();
            for seq in 0..5 {
                video.send(&frame(seq)).await.unwrap();
            }
            video.into_inner().finish().unwrap();
            // Keep the connection open until the client has read everything.
            conn.closed().await;
        });

        let client = client(localhost(0), ServerTrust::Pinned(identity.fingerprint())).unwrap();
        let conn = connect(&client, addr).await.unwrap();
        let (mut ctl_send, mut ctl_recv) = open_control(&conn).await.unwrap();
        ctl_send
            .send(&v1::KeyframeRequest { display: 3 }.into())
            .await
            .unwrap();
        let reply: ControlMessage = ctl_recv.next().await.unwrap().unwrap();
        assert_eq!(reply, v1::Pong { timestamp_us: 7 }.into());

        let IncomingStream::Video {
            display,
            mut reader,
        } = accept_uni(&conn).await.unwrap();
        assert_eq!(display, 3);
        for seq in 0..5 {
            assert_eq!(reader.next().await.unwrap().unwrap(), frame(seq));
        }
        assert!(reader.next().await.unwrap().is_none());
        shutdown(&client, b"done").await;
        host.await.unwrap();
    }

    #[tokio::test]
    async fn input_stream_from_client_to_host() {
        let identity = Identity::self_signed().unwrap();
        let server = server(localhost(0), &identity).unwrap();
        let addr = server.local_addr().unwrap();
        let host = tokio::spawn(async move {
            let conn = accept(&server).await.unwrap().unwrap();
            let mut input = crate::streams::accept_input(&conn).await.unwrap();
            let mut got = Vec::new();
            while let Some(msg) = input.next().await.unwrap() {
                got.push(msg.seq);
            }
            got
        });
        let client = client(localhost(0), ServerTrust::Pinned(identity.fingerprint())).unwrap();
        let conn = connect(&client, addr).await.unwrap();
        let mut input = crate::streams::open_input(&conn).await.unwrap();
        for seq in 0..3 {
            input
                .send(&v1::InputMessage { seq, msg: None })
                .await
                .unwrap();
        }
        input.into_inner().finish().unwrap();
        assert_eq!(host.await.unwrap(), vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn channel_binding_matches_per_connection() {
        let identity = Identity::self_signed().unwrap();
        let server = server(localhost(0), &identity).unwrap();
        let addr = server.local_addr().unwrap();
        let host = tokio::spawn(async move {
            let mut bindings = Vec::new();
            for _ in 0..2 {
                let conn = accept(&server).await.unwrap().unwrap();
                bindings.push(channel_binding(&conn).unwrap());
            }
            bindings
        });
        let client = client(localhost(0), ServerTrust::InsecureAcceptAny).unwrap();
        let a = connect(&client, addr).await.unwrap();
        let b = connect(&client, addr).await.unwrap();
        let (ba, bb) = (channel_binding(&a).unwrap(), channel_binding(&b).unwrap());
        let host_side = host.await.unwrap();
        assert_eq!(host_side, vec![ba, bb], "both ends agree");
        assert_ne!(ba, bb, "each connection is distinct");
    }

    #[tokio::test]
    async fn closing_is_seen_by_the_peer_promptly() {
        let identity = Identity::self_signed().unwrap();
        let server = server(localhost(0), &identity).unwrap();
        let addr = server.local_addr().unwrap();
        let host = tokio::spawn(async move {
            let conn = accept(&server).await.unwrap().unwrap();
            // Must end well before the idle timeout.
            let reason = tokio::time::timeout(Duration::from_secs(2), conn.closed())
                .await
                .expect("close noticed promptly");
            (reason, conn)
        });
        let client = client(localhost(0), ServerTrust::Pinned(identity.fingerprint())).unwrap();
        let conn = connect(&client, addr).await.unwrap();
        close(&conn, b"bye");
        let (reason, host_conn) = host.await.unwrap();
        assert!(matches!(
            reason,
            quinn::ConnectionError::ApplicationClosed(_)
        ));
        assert!(closed_by_peer(&host_conn));
        assert_eq!(peer_close_message(&host_conn).as_deref(), Some("bye"));
        assert_eq!(peer_close_message(&conn), None, "closed here");
        assert!(!closed_by_peer(&conn), "closed locally, not by the peer");
    }

    #[tokio::test]
    async fn wrong_pin_is_refused() {
        let identity = Identity::self_signed().unwrap();
        let other = Identity::self_signed().unwrap();
        let server = server(localhost(0), &identity).unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            // The handshake fails on the client side; just drive the server.
            let _ = accept(&server).await;
        });
        let client = client(localhost(0), ServerTrust::Pinned(other.fingerprint())).unwrap();
        assert!(connect(&client, addr).await.is_err());
    }
}
