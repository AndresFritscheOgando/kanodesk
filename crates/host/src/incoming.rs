//! Host side of connection setup (03 §5–6).
//!
//! When the ID server forwards a controller's request, the host:
//!
//! 1. punches: sends small datagrams from its device socket to every address
//!    of the controller (public, LAN, IPv6), opening its own NAT so the
//!    controller's QUIC handshake gets in;
//! 2. binds at the relay with the session token and says hello there, so a
//!    relayed attempt can start at any time.
//!
//! Connections themselves arrive on the device endpoint's accept loop, by
//! whichever path won; the caller authenticates them (Noise).

use std::net::SocketAddr;
use std::time::Duration;

use kanodesk_proto::v1::{NatType, RelayRole};
use kanodesk_rendezvous::IncomingRequest;
use kanodesk_transport::peer::PeerSocket;
use kanodesk_transport::{nat, relay};

/// Punch and relay settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostOptions {
    /// Punch datagrams per target.
    pub punch_rounds: u32,
    /// Gap between punch rounds.
    pub punch_interval: Duration,
    /// Register at the relay too.
    pub use_relay: bool,
}

impl Default for HostOptions {
    fn default() -> Self {
        // 15 rounds over ~1.5 s covers the controller's first retransmissions.
        HostOptions {
            punch_rounds: 15,
            punch_interval: Duration::from_millis(100),
            use_relay: true,
        }
    }
}

/// What preparation achieved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prepared {
    /// Addresses punched toward.
    pub punched: Vec<SocketAddr>,
    /// The relay data address, if binding there worked.
    pub relay_data: Option<SocketAddr>,
    /// Why relay binding failed, if it did.
    pub relay_error: Option<String>,
    /// The relay session token, if one was assigned.
    pub relay_session: Option<[u8; 16]>,
}

/// If `conn` arrived through this request's relay session, keeps sending
/// hellos while it lives, so the relay follows NAT rebinding on this side.
/// Returns whether it did.
pub fn keep_relay_alive(
    socket: &PeerSocket,
    prepared: &Prepared,
    conn: &kanodesk_transport::Connection,
) -> bool {
    let remote = kanodesk_transport::peer::canonical(conn.remote_address());
    if let (Some(data), Some(session)) = (prepared.relay_data, prepared.relay_session)
        && data == remote
    {
        relay::spawn_hello_keepalive(
            socket.raw_sender(),
            data,
            session,
            RelayRole::Host,
            conn.clone(),
            relay::HELLO_INTERVAL,
        );
        return true;
    }
    false
}

/// Punches toward the controller and binds at the relay, concurrently.
pub async fn prepare(
    socket: &PeerSocket,
    request: &IncomingRequest,
    options: &HostOptions,
) -> Prepared {
    let mut targets: Vec<SocketAddr> = vec![request.controller_addr];
    targets.extend(request.controller_local_addrs.iter().copied());
    // Toward a symmetric NAT, also the ports it will probably open next.
    if request.controller_nat == NatType::Symmetric {
        targets.extend(nat::predicted_ports(
            request.controller_addr,
            nat::PREDICTION_WINDOW,
        ));
    }
    targets.retain(|a| socket.can_reach(*a));
    let mut seen = Vec::new();
    targets.retain(|a| {
        let new = !seen.contains(a);
        seen.push(*a);
        new
    });

    let punching = nat::punch(
        socket,
        &targets,
        options.punch_rounds,
        options.punch_interval,
    );
    let binding = async {
        if !options.use_relay {
            return (None, None);
        }
        let (Some(name), Some(session)) = (&request.relay, request.relay_session) else {
            return (None, Some("no relay assigned".to_owned()));
        };
        let addr = match tokio::net::lookup_host(name.as_str())
            .await
            .ok()
            .and_then(|mut a| a.next())
        {
            Some(a) => a,
            None => return (None, Some(format!("cannot resolve relay {name}"))),
        };
        match relay::bind(
            socket,
            addr,
            &session,
            &request.relay_ticket,
            RelayRole::Host,
        )
        .await
        {
            Ok(data) => {
                relay::send_hellos(socket, data, &session, RelayRole::Host, 3).await;
                (Some(data), None)
            }
            Err(e) => (None, Some(e.to_string())),
        }
    };
    let ((), (relay_data, relay_error)) = tokio::join!(punching, binding);
    tracing::info!(
        controller = %request.controller.id,
        punched = targets.len(),
        relay = ?relay_data,
        "prepared for incoming connection"
    );
    Prepared {
        punched: targets,
        relay_data,
        relay_error,
        relay_session: request.relay_session,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanodesk_crypto::DeviceIdentity;
    use kanodesk_rendezvous::VerifiedRecord;
    use kanodesk_transport::tls::{Identity, ServerTrust};

    fn request(relay: Option<&str>, locals: &[&str]) -> IncomingRequest {
        IncomingRequest {
            controller: VerifiedRecord {
                id: "123456789".parse().unwrap(),
                key: DeviceIdentity::generate().unwrap().public_key(),
                issued_at: 0,
            },
            controller_addr: "127.0.0.1:9".parse().unwrap(),
            host_addr: "127.0.0.1:10".parse().unwrap(),
            relay: relay.map(str::to_owned),
            relay_session: relay.map(|_| [1u8; 16]),
            relay_ticket: Vec::new(),
            controller_local_addrs: locals.iter().map(|a| a.parse().unwrap()).collect(),
            controller_nat: NatType::Cone,
        }
    }

    fn socket() -> PeerSocket {
        PeerSocket::bind(
            0,
            &Identity::self_signed().unwrap(),
            ServerTrust::InsecureAcceptAny,
        )
        .unwrap()
    }

    fn quick() -> HostOptions {
        HostOptions {
            punch_rounds: 2,
            punch_interval: Duration::from_millis(1),
            use_relay: true,
        }
    }

    #[tokio::test]
    async fn punches_every_controller_address_once() {
        let s = socket();
        let p = prepare(
            &s,
            &request(None, &["192.168.1.9:9", "127.0.0.1:9"]),
            &quick(),
        )
        .await;
        // The public address first; the duplicate local one is dropped.
        let expected: Vec<SocketAddr> = vec![
            "127.0.0.1:9".parse().unwrap(),
            "192.168.1.9:9".parse().unwrap(),
        ];
        assert_eq!(p.punched, expected);
        assert_eq!(p.relay_error.as_deref(), Some("no relay assigned"));
        assert!(p.relay_data.is_none());
    }

    #[tokio::test]
    async fn symmetric_controller_gets_predicted_ports() {
        let s = socket();
        let mut r = request(None, &[]);
        r.controller_nat = NatType::Symmetric;
        let p = prepare(&s, &r, &quick()).await;
        assert_eq!(p.punched.len(), 1 + nat::PREDICTION_WINDOW as usize);
        assert_eq!(p.punched[1], "127.0.0.1:10".parse().unwrap());
    }

    #[tokio::test]
    async fn relay_problems_are_reported_not_fatal() {
        let s = socket();
        let p = prepare(&s, &request(Some("not a host name:x"), &[]), &quick()).await;
        assert!(p.relay_error.unwrap().contains("cannot resolve"));
        let p = prepare(
            &s,
            &request(Some("127.0.0.1:9"), &[]),
            &HostOptions {
                use_relay: false,
                ..quick()
            },
        )
        .await;
        assert_eq!((p.relay_data, p.relay_error), (None, None));
    }
}
