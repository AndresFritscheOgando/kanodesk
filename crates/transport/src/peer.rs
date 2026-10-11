//! A device's one UDP socket: a QUIC endpoint that both accepts and makes
//! connections, plus a raw handle for hole-punching datagrams.
//!
//! Everything a device does (ID server, incoming and outgoing peer
//! connections, relay) uses this one socket, so the public address the ID
//! server observes is the address peers must reach. The socket is dual-stack
//! (`[::]`, IPv4-mapped) where the OS allows, else IPv4 only.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::sync::Arc;

use quinn::{Endpoint, EndpointConfig, TokioRuntime};
use socket2::{Domain, Protocol, Socket, Type};

use crate::TransportError;
use crate::endpoint::{client_config, server_config};
use crate::tls::{Identity, ServerTrust};

/// The device socket.
pub struct PeerSocket {
    endpoint: Endpoint,
    raw: RawSender,
    local: SocketAddr,
    dual_stack: bool,
}

/// A cheap, cloneable handle for sending raw datagrams from the device
/// socket (for background tasks such as relay keep-alive hellos).
#[derive(Clone)]
pub struct RawSender {
    socket: Arc<UdpSocket>,
    dual_stack: bool,
}

impl RawSender {
    /// Sends one datagram (see [`PeerSocket::send_raw`]).
    pub fn send(&self, data: &[u8], to: SocketAddr) -> std::io::Result<usize> {
        self.socket.send_to(data, for_socket(to, self.dual_stack))
    }
}

fn bind_dual_stack(port: u16) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(false)?;
    socket.bind(&SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0)).into())?;
    Ok(socket.into())
}

/// IPv4-mapped form of `addr` for a dual-stack socket.
fn for_socket(addr: SocketAddr, dual_stack: bool) -> SocketAddr {
    match (addr, dual_stack) {
        (SocketAddr::V4(v4), true) => {
            SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
        }
        _ => addr,
    }
}

/// `addr` with an IPv4-mapped IPv6 address turned back into IPv4.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

impl PeerSocket {
    /// Binds `port` (0 for any) on all interfaces. `identity` is presented
    /// to peers that connect in; `trust` decides which servers this device
    /// accepts when connecting out (peers are authenticated by Noise later,
    /// so `InsecureAcceptAny` is normal here). Needs a tokio runtime.
    pub fn bind(
        port: u16,
        identity: &Identity,
        trust: ServerTrust,
    ) -> Result<Self, TransportError> {
        let (socket, dual_stack) = match bind_dual_stack(port) {
            Ok(s) => (s, true),
            Err(_) => (
                UdpSocket::bind(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::UNSPECIFIED,
                    port,
                )))?,
                false,
            ),
        };
        socket.set_nonblocking(true)?;
        let raw = socket.try_clone()?;
        let local = socket.local_addr()?;
        let mut endpoint = Endpoint::new(
            EndpointConfig::default(),
            Some(server_config(identity)?),
            socket,
            Arc::new(TokioRuntime),
        )?;
        endpoint.set_default_client_config(client_config(trust)?);
        Ok(PeerSocket {
            endpoint,
            raw: RawSender {
                socket: Arc::new(raw),
                dual_stack,
            },
            local,
            dual_stack,
        })
    }

    /// The QUIC endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The local UDP port.
    pub fn port(&self) -> u16 {
        self.local.port()
    }

    /// Whether the socket takes both IPv4 and IPv6.
    pub fn is_dual_stack(&self) -> bool {
        self.dual_stack
    }

    /// Whether this socket can reach `addr` at all (IPv6 needs dual stack).
    pub fn can_reach(&self, addr: SocketAddr) -> bool {
        addr.is_ipv4() || self.dual_stack
    }

    /// Sends one raw datagram from the device socket (hole punching, relay
    /// hellos). QUIC on the other side ignores such datagrams.
    pub fn send_raw(&self, data: &[u8], to: SocketAddr) -> std::io::Result<usize> {
        self.raw.send(data, to)
    }

    /// A handle for raw sends from background tasks.
    pub fn raw_sender(&self) -> RawSender {
        self.raw.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint;
    use std::time::Duration;

    fn peer() -> PeerSocket {
        PeerSocket::bind(
            0,
            &Identity::self_signed().unwrap(),
            ServerTrust::InsecureAcceptAny,
        )
        .unwrap()
    }

    fn v4(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    async fn echo_once(conn: quinn::Connection) {
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let data = recv.read_to_end(1024).await.unwrap();
        send.write_all(&data).await.unwrap();
        send.finish().unwrap();
        conn.closed().await;
    }

    async fn round_trip(conn: &quinn::Connection) -> Vec<u8> {
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(b"ping").await.unwrap();
        send.finish().unwrap();
        recv.read_to_end(1024).await.unwrap()
    }

    #[tokio::test]
    async fn dual_stack_peer_connects_to_ipv4_server_and_back() {
        let a = peer();
        assert!(a.is_dual_stack(), "Windows supports dual-stack sockets");
        // Peer -> plain IPv4 server.
        let server = endpoint::server(v4(0), &Identity::self_signed().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let s =
            tokio::spawn(
                async move { echo_once(server.accept().await.unwrap().await.unwrap()).await },
            );
        let conn = endpoint::connect(a.endpoint(), addr).await.unwrap();
        assert_eq!(canonical(conn.remote_address()), addr);
        assert_eq!(round_trip(&conn).await, b"ping");
        conn.close(0u32.into(), b"");
        s.await.unwrap();
    }

    #[tokio::test]
    async fn two_peers_connect_both_ways_over_ipv4_and_ipv6() {
        let a = peer();
        let b = peer();
        for target in [
            v4(b.port()),
            SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, b.port(), 0, 0)),
        ] {
            let b_ep = b.endpoint().clone();
            let s = tokio::spawn(async move {
                echo_once(b_ep.accept().await.unwrap().await.unwrap()).await
            });
            let conn = endpoint::connect(a.endpoint(), target).await.unwrap();
            assert_eq!(round_trip(&conn).await, b"ping");
            conn.close(0u32.into(), b"");
            tokio::time::timeout(Duration::from_secs(5), s)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn raw_datagrams_do_not_disturb_quic() {
        let a = peer();
        let b = peer();
        // Junk at b's QUIC socket is ignored; a real connection still works.
        for _ in 0..5 {
            a.send_raw(b"KDPUNCH1 not quic", v4(b.port())).unwrap();
        }
        let b_ep = b.endpoint().clone();
        let s =
            tokio::spawn(
                async move { echo_once(b_ep.accept().await.unwrap().await.unwrap()).await },
            );
        let conn = endpoint::connect(a.endpoint(), v4(b.port())).await.unwrap();
        assert_eq!(round_trip(&conn).await, b"ping");
        conn.close(0u32.into(), b"");
        s.await.unwrap();
    }

    #[test]
    fn canonical_unmaps_ipv4() {
        let mapped: SocketAddr = "[::ffff:10.0.0.1]:5".parse().unwrap();
        assert_eq!(canonical(mapped), "10.0.0.1:5".parse().unwrap());
        assert_eq!(for_socket("10.0.0.1:5".parse().unwrap(), true), mapped);
        assert_eq!(
            for_socket("10.0.0.1:5".parse().unwrap(), false),
            "10.0.0.1:5".parse().unwrap()
        );
    }
}
