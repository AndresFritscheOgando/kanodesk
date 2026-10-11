//! One relay session: a UDP socket that forwards datagrams between the two
//! addresses learned from hellos.

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use kanodesk_proto::v1::RelayRole;
use kanodesk_transport::relay::{SESSION_LEN, parse_hello};
use tokio::net::UdpSocket;

use crate::Shared;
use crate::bucket::ByteBucket;

/// Binds a data socket on `ip`, inside `range` if given (random start, so
/// ports are not predictable).
pub(crate) async fn bind_data_socket(
    ip: IpAddr,
    range: Option<(u16, u16)>,
) -> std::io::Result<UdpSocket> {
    let Some((lo, hi)) = range else {
        return UdpSocket::bind(SocketAddr::new(ip, 0)).await;
    };
    let span = u32::from(hi - lo) + 1;
    let mut start = [0u8; 4];
    getrandom::fill(&mut start).map_err(|e| std::io::Error::other(e.to_string()))?;
    let start = u32::from_le_bytes(start) % span;
    for i in 0..span {
        let port = lo + ((start + i) % span) as u16;
        if let Ok(socket) = UdpSocket::bind(SocketAddr::new(ip, port)).await {
            return Ok(socket);
        }
    }
    Err(std::io::Error::new(
        ErrorKind::AddrInUse,
        "no free port in range",
    ))
}

/// Forwards until the session is idle for the configured timeout.
pub(crate) async fn forward(socket: UdpSocket, token: [u8; SESSION_LEN], shared: Arc<Shared>) {
    let idle = shared.config.idle_timeout;
    let mut bucket = ByteBucket::new(shared.config.session_bandwidth, Instant::now());
    let (mut controller, mut host): (Option<SocketAddr>, Option<SocketAddr>) = (None, None);
    let mut buf = vec![0u8; 65_535];
    let counters = &shared.counters;
    loop {
        let (n, from) = match tokio::time::timeout(idle, socket.recv_from(&mut buf)).await {
            Err(_) => break,
            // Windows reports an earlier send's ICMP "port unreachable" on the
            // next receive; it says nothing about this datagram.
            Ok(Err(e)) if e.kind() == ErrorKind::ConnectionReset => continue,
            Ok(Err(_)) => break,
            Ok(Ok(v)) => v,
        };
        let data = &buf[..n];
        if let Some((role, session)) = parse_hello(data) {
            if session == token {
                match role {
                    RelayRole::Controller => controller = Some(from),
                    RelayRole::Host => host = Some(from),
                    RelayRole::Unspecified => {}
                }
            }
            continue;
        }
        let to = if Some(from) == controller {
            host
        } else if Some(from) == host {
            controller
        } else {
            None
        };
        let Some(to) = to else {
            counters.dropped_unknown.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if !bucket.take(n, Instant::now()) {
            // QUIC congestion control backs off on the loss.
            counters.dropped_over_limit.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if socket.send_to(data, to).await.is_ok() {
            counters
                .bytes_forwarded
                .fetch_add(n as u64, Ordering::Relaxed);
        }
    }
    shared.end_session(&token);
    tracing::info!(
        port = socket.local_addr().map(|a| a.port()).ok(),
        "relay session ended"
    );
}
