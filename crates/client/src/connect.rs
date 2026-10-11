//! Connection strategy (03 §5–6, 16 §4): LAN first, then a direct punch
//! (public IPv4 and global IPv6 in parallel), then the relay.
//!
//! The host is told about the attempt by the ID server and, at the same
//! time, punches toward this device and binds at the relay
//! (`kanodesk_host::incoming`), so whichever path works is ready.
//! Each phase races all its candidate addresses and stops at the first
//! QUIC handshake that completes; the winner is logged.

use std::net::SocketAddr;
use std::time::Duration;

use kanodesk_proto::v1::{NatType, RelayRole};
use kanodesk_rendezvous::ConnectInfo;
use kanodesk_transport::nat::{
    PREDICTION_WINDOW, is_global_v6, is_private_v4, is_public_v4, predicted_ports,
};
use kanodesk_transport::peer::PeerSocket;
use kanodesk_transport::{Connection, endpoint, relay};
use tokio::task::JoinSet;

/// Which path a connection took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Private address on the same network.
    Lan,
    /// Global IPv6 address (no NAT, firewall punched).
    Ipv6,
    /// Hole-punched public IPv4 address.
    Direct,
    /// Through the relay server.
    Relay,
}

/// Where to reach the relay for this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayPlan {
    /// The relay's control address.
    pub addr: SocketAddr,
    /// Session token from the ID server.
    pub session: [u8; 16],
    /// Ticket from the ID server.
    pub ticket: Vec<u8>,
}

/// Candidate addresses per phase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectPlan {
    /// Private addresses (same network only).
    pub lan: Vec<SocketAddr>,
    /// Global IPv6 and public IPv4 addresses.
    pub direct: Vec<SocketAddr>,
    /// The relay, if the ID server assigned one.
    pub relay: Option<RelayPlan>,
}

impl ConnectPlan {
    /// Builds the plan from the broker's answer. When both sides are behind
    /// symmetric NATs a direct punch cannot work, so it is skipped.
    pub async fn from_info(info: &ConnectInfo, my_nat: NatType) -> Self {
        let lan = info
            .host_local_addrs
            .iter()
            .copied()
            .filter(|a| is_private_v4(a.ip()))
            .collect();
        let both_symmetric = my_nat == NatType::Symmetric && info.host_nat == NatType::Symmetric;
        let direct = if both_symmetric {
            Vec::new()
        } else {
            // Global IPv6 and public IPv4 (e.g. a UPnP mapping) first, then
            // the address the ID server saw, then, toward a symmetric NAT,
            // the ports it will probably allocate next.
            let mut direct: Vec<SocketAddr> = info
                .host_local_addrs
                .iter()
                .copied()
                .filter(|a| is_global_v6(a.ip()) || is_public_v4(a.ip()))
                .collect();
            if !direct.contains(&info.host_addr) {
                direct.push(info.host_addr);
            }
            if info.host_nat == NatType::Symmetric {
                direct.extend(predicted_ports(info.host_addr, PREDICTION_WINDOW));
            }
            direct
        };
        let relay = match (&info.relay, info.relay_session) {
            (Some(name), Some(session)) => tokio::net::lookup_host(name.as_str())
                .await
                .ok()
                .and_then(|mut addrs| addrs.next())
                .map(|addr| RelayPlan {
                    addr,
                    session,
                    ticket: info.relay_ticket.clone(),
                }),
            _ => None,
        };
        ConnectPlan { lan, direct, relay }
    }
}

/// Timeouts and switches for each phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectOptions {
    /// Try LAN addresses.
    pub try_lan: bool,
    /// Try a direct punch.
    pub try_direct: bool,
    /// Fall back to the relay.
    pub try_relay: bool,
    /// LAN phase limit (a LAN handshake takes milliseconds).
    pub lan_timeout: Duration,
    /// Direct phase limit (the host's punches need to arrive).
    pub direct_timeout: Duration,
    /// Relay phase limit.
    pub relay_timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions {
            try_lan: true,
            try_direct: true,
            try_relay: true,
            lan_timeout: Duration::from_millis(1500),
            direct_timeout: Duration::from_secs(5),
            relay_timeout: Duration::from_secs(10),
        }
    }
}

/// A connection and how it was made.
#[derive(Debug)]
pub struct Established {
    /// The QUIC connection (authenticate it with Noise next).
    pub conn: Connection,
    /// The path that won.
    pub path: PathKind,
    /// The address connected to (the relay's data port for relayed sessions).
    pub remote: SocketAddr,
}

/// Every path failed.
#[derive(Debug, thiserror::Error)]
#[error("no path to the host: {}", attempts.iter().map(|(p, e)| format!("{p:?}: {e}")).collect::<Vec<_>>().join("; "))]
pub struct ConnectError {
    /// What was tried, in order, and why it failed.
    pub attempts: Vec<(PathKind, String)>,
}

/// Connects to any of `addrs` within `limit`; the first handshake wins.
async fn race(
    socket: &PeerSocket,
    addrs: &[SocketAddr],
    limit: Duration,
) -> Result<(Connection, SocketAddr), String> {
    let mut set = JoinSet::new();
    for &addr in addrs.iter().filter(|a| socket.can_reach(**a)) {
        let ep = socket.endpoint().clone();
        set.spawn(async move { (addr, endpoint::connect(&ep, addr).await) });
    }
    if set.is_empty() {
        return Err("no usable address".into());
    }
    let mut errors = Vec::new();
    let outcome = tokio::time::timeout(limit, async {
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((addr, Ok(conn))) => return Some((conn, addr)),
                Ok((addr, Err(e))) => errors.push(format!("{addr}: {e}")),
                Err(e) => errors.push(e.to_string()),
            }
        }
        None
    })
    .await;
    set.abort_all();
    match outcome {
        Ok(Some(won)) => Ok(won),
        Ok(None) => Err(errors.join(", ")),
        Err(_) => Err(format!("timed out after {limit:?}")),
    }
}

/// Connects to the host along the first path that works.
pub async fn connect(
    socket: &PeerSocket,
    plan: &ConnectPlan,
    options: &ConnectOptions,
) -> Result<Established, ConnectError> {
    let mut attempts = Vec::new();
    let done = |conn: Connection, path: PathKind, remote: SocketAddr| {
        tracing::info!(?path, %remote, "connected to host");
        Established { conn, path, remote }
    };

    if options.try_lan && !plan.lan.is_empty() {
        match race(socket, &plan.lan, options.lan_timeout).await {
            Ok((conn, addr)) => return Ok(done(conn, PathKind::Lan, addr)),
            Err(e) => attempts.push((PathKind::Lan, e)),
        }
    }
    if options.try_direct && !plan.direct.is_empty() {
        match race(socket, &plan.direct, options.direct_timeout).await {
            Ok((conn, addr)) => {
                let path = if addr.is_ipv6() && is_global_v6(addr.ip()) {
                    PathKind::Ipv6
                } else {
                    PathKind::Direct
                };
                return Ok(done(conn, path, addr));
            }
            Err(e) => attempts.push((PathKind::Direct, e)),
        }
    }
    if options.try_relay {
        match &plan.relay {
            Some(r) => match via_relay(socket, r, options.relay_timeout).await {
                Ok((conn, addr)) => {
                    // Keep the relay's view of our address fresh (NAT rebinding).
                    relay::spawn_hello_keepalive(
                        socket.raw_sender(),
                        addr,
                        r.session,
                        RelayRole::Controller,
                        conn.clone(),
                        relay::HELLO_INTERVAL,
                    );
                    return Ok(done(conn, PathKind::Relay, addr));
                }
                Err(e) => attempts.push((PathKind::Relay, e)),
            },
            None => attempts.push((PathKind::Relay, "no relay assigned".into())),
        }
    }
    tracing::warn!(?attempts, "no path to the host");
    Err(ConnectError { attempts })
}

async fn via_relay(
    socket: &PeerSocket,
    plan: &RelayPlan,
    limit: Duration,
) -> Result<(Connection, SocketAddr), String> {
    tokio::time::timeout(limit, async {
        let data = relay::bind(
            socket,
            plan.addr,
            &plan.session,
            &plan.ticket,
            RelayRole::Controller,
        )
        .await
        .map_err(|e| e.to_string())?;
        relay::send_hellos(socket, data, &plan.session, RelayRole::Controller, 3).await;
        let conn = endpoint::connect(socket.endpoint(), data)
            .await
            .map_err(|e| e.to_string())?;
        Ok((conn, data))
    })
    .await
    .map_err(|_| format!("timed out after {limit:?}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanodesk_crypto::DeviceIdentity;
    use kanodesk_rendezvous::DeviceId;
    use kanodesk_rendezvous::VerifiedRecord;

    fn info(host_nat: NatType, locals: &[&str]) -> ConnectInfo {
        ConnectInfo {
            host: VerifiedRecord {
                id: "123456789".parse::<DeviceId>().unwrap(),
                key: DeviceIdentity::generate().unwrap().public_key(),
                issued_at: 0,
            },
            host_addr: "203.0.113.7:40000".parse().unwrap(),
            controller_addr: "198.51.100.2:50000".parse().unwrap(),
            relay: Some("127.0.0.1:21117".into()),
            relay_session: Some([1u8; 16]),
            relay_ticket: vec![2u8; 24],
            host_local_addrs: locals.iter().map(|a| a.parse().unwrap()).collect(),
            host_nat,
        }
    }

    #[tokio::test]
    async fn plan_sorts_candidates_by_phase() {
        let plan = ConnectPlan::from_info(
            &info(NatType::Cone, &["192.168.1.5:40000", "[2001:db8::5]:40000"]),
            NatType::Cone,
        )
        .await;
        assert_eq!(plan.lan, vec!["192.168.1.5:40000".parse().unwrap()]);
        assert_eq!(
            plan.direct,
            vec![
                "[2001:db8::5]:40000".parse().unwrap(),
                "203.0.113.7:40000".parse().unwrap()
            ]
        );
        let relay = plan.relay.unwrap();
        assert_eq!(relay.addr, "127.0.0.1:21117".parse().unwrap());
        assert_eq!((relay.session, relay.ticket.len()), ([1u8; 16], 24));
    }

    #[tokio::test]
    async fn symmetric_host_gets_predicted_ports_and_public_locals_come_first() {
        let plan = ConnectPlan::from_info(
            &info(NatType::Symmetric, &["8.8.4.4:41000", "10.0.0.2:41000"]),
            NatType::Cone,
        )
        .await;
        assert_eq!(
            plan.direct[0],
            "8.8.4.4:41000".parse().unwrap(),
            "public (UPnP) address first"
        );
        assert_eq!(plan.direct[1], "203.0.113.7:40000".parse().unwrap());
        assert_eq!(plan.direct.len(), 2 + PREDICTION_WINDOW as usize);
        assert_eq!(plan.direct[2], "203.0.113.7:40001".parse().unwrap());
        // A cone host gets no predictions.
        let plan = ConnectPlan::from_info(&info(NatType::Cone, &[]), NatType::Cone).await;
        assert_eq!(plan.direct, vec!["203.0.113.7:40000".parse().unwrap()]);
    }

    #[tokio::test]
    async fn both_symmetric_skips_the_direct_punch() {
        let plan = ConnectPlan::from_info(&info(NatType::Symmetric, &[]), NatType::Symmetric).await;
        assert!(plan.direct.is_empty());
        let plan = ConnectPlan::from_info(&info(NatType::Symmetric, &[]), NatType::Cone).await;
        assert_eq!(
            plan.direct.len(),
            1 + PREDICTION_WINDOW as usize,
            "one cone side can still punch, with predicted ports"
        );
    }

    #[tokio::test]
    async fn nothing_to_try_is_a_clear_error() {
        let socket = PeerSocket::bind(
            0,
            &kanodesk_transport::tls::Identity::self_signed().unwrap(),
            kanodesk_transport::tls::ServerTrust::InsecureAcceptAny,
        )
        .unwrap();
        let err = connect(&socket, &ConnectPlan::default(), &ConnectOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err.attempts.len(), 1);
        assert_eq!(err.attempts[0].0, PathKind::Relay);
    }
}
