//! QUIC endpoints: TLS 1.3 with the ring provider, ALPN, and transport tuning.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, IdleTimeout, TransportConfig, VarInt};

use crate::TransportError;
use crate::tls::{Identity, PinVerifier, SERVER_NAME, ServerTrust};

/// ALPN protocol id. Bumped whenever the spike wire format changes.
pub const ALPN: &[u8] = b"kanodesk-spike/0";

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

/// Creates a server endpoint bound to `bind`, presenting `identity`.
/// Must be called inside a tokio runtime.
pub fn server(bind: SocketAddr, identity: &Identity) -> Result<Endpoint, TransportError> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert.clone()], identity.private_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto =
        QuicServerConfig::try_from(tls).map_err(|e| TransportError::Other(e.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(Arc::new(transport_config()?));
    Ok(Endpoint::server(config, bind)?)
}

/// Creates a client endpoint bound to `bind` (use port 0) that trusts servers
/// according to `trust`. Must be called inside a tokio runtime.
pub fn client(bind: SocketAddr, trust: ServerTrust) -> Result<Endpoint, TransportError> {
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
    let mut endpoint = Endpoint::client(bind)?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

/// Connects to a server.
pub async fn connect(endpoint: &Endpoint, addr: SocketAddr) -> Result<Connection, TransportError> {
    Ok(endpoint.connect(addr, SERVER_NAME)?.await?)
}

/// Closes one connection immediately with application code 0.
pub fn close(conn: &Connection, reason: &[u8]) {
    conn.close(VarInt::from_u32(0), reason);
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
    use crate::framing::{read_frame, write_frame};
    use crate::streams::{StreamKind, accept_control, accept_uni, open_control, open_video};
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn localhost(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    #[tokio::test]
    async fn control_and_video_streams_over_loopback() {
        let identity = Identity::self_signed().unwrap();
        let server = server(localhost(0), &identity).unwrap();
        let addr = server.local_addr().unwrap();

        let host = tokio::spawn(async move {
            let conn = accept(&server).await.unwrap().unwrap();
            let (mut ctl_send, mut ctl_recv) = accept_control(&conn).await.unwrap();
            let mut buf = Vec::new();
            assert!(read_frame(&mut ctl_recv, &mut buf).await.unwrap());
            assert_eq!(buf, b"keyframe please");
            write_frame(&mut ctl_send, b"ok").await.unwrap();

            let mut video = open_video(&conn, 3).await.unwrap();
            for i in 0u8..5 {
                write_frame(&mut video, &vec![i; 1000 * i as usize])
                    .await
                    .unwrap();
            }
            video.finish().unwrap();
            // Keep the connection open until the client has read everything.
            conn.closed().await;
        });

        let client = client(localhost(0), ServerTrust::Pinned(identity.fingerprint())).unwrap();
        let conn = connect(&client, addr).await.unwrap();
        let (mut ctl_send, mut ctl_recv) = open_control(&conn).await.unwrap();
        write_frame(&mut ctl_send, b"keyframe please")
            .await
            .unwrap();
        let mut buf = Vec::new();
        assert!(read_frame(&mut ctl_recv, &mut buf).await.unwrap());
        assert_eq!(buf, b"ok");

        let (kind, mut video) = accept_uni(&conn).await.unwrap();
        assert_eq!(kind, StreamKind::Video { display: 3 });
        for i in 0u8..5 {
            assert!(read_frame(&mut video, &mut buf).await.unwrap());
            assert_eq!(buf, vec![i; 1000 * i as usize]);
        }
        assert!(!read_frame(&mut video, &mut buf).await.unwrap());
        shutdown(&client, b"done").await;
        host.await.unwrap();
    }

    #[tokio::test]
    async fn closing_is_seen_by_the_peer_promptly() {
        let identity = Identity::self_signed().unwrap();
        let server = server(localhost(0), &identity).unwrap();
        let addr = server.local_addr().unwrap();
        let host = tokio::spawn(async move {
            let conn = accept(&server).await.unwrap().unwrap();
            // Must end well before the idle timeout.
            tokio::time::timeout(Duration::from_secs(2), conn.closed())
                .await
                .expect("close noticed promptly")
        });
        let client = client(localhost(0), ServerTrust::Pinned(identity.fingerprint())).unwrap();
        let conn = connect(&client, addr).await.unwrap();
        close(&conn, b"bye");
        let reason = host.await.unwrap();
        assert!(matches!(
            reason,
            quinn::ConnectionError::ApplicationClosed(_)
        ));
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
