//! NAT type classification (03 §4), local address discovery, and hole
//! punching.
//!
//! **Detection**: the device asks the ID server's main port and its NAT-test
//! port, from one UDP socket, which address they see. Same public address →
//! the NAT maps endpoint-independently (cone, punchable); different →
//! symmetric; equal to a local address → no NAT. The queries themselves are
//! in `kanodesk-rendezvous`; [`classify`] is the decision.
//!
//! **Punching**: the host sends a few small datagrams to every address of
//! the controller while the controller starts its QUIC handshake toward the
//! host. The host's datagrams open a mapping in the host's NAT, so the
//! controller's retransmitted Initial gets through.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

pub use kanodesk_proto::v1::NatType;

use crate::peer::{PeerSocket, canonical};

/// Prefix of hole-punching datagrams (ignored by QUIC).
pub const PUNCH_MAGIC: &[u8] = b"KDPUNCH1";

/// Classifies the NAT from what two server ports observed for one socket.
pub fn classify(
    local_ips: &[IpAddr],
    local_port: u16,
    observed_main: SocketAddr,
    observed_test: SocketAddr,
) -> NatType {
    let (main, test) = (canonical(observed_main), canonical(observed_test));
    if main != test {
        return NatType::Symmetric;
    }
    let ip = main.ip();
    let is_local = ip.is_loopback() || ip.is_unspecified() || local_ips.contains(&ip);
    if is_local && main.port() == local_port {
        NatType::Open
    } else {
        NatType::Cone
    }
}

/// Private IPv4 (RFC 1918) addresses: worth trying only on the same LAN.
pub fn is_private_v4(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(_) => false,
    }
}

/// Public IPv4: not private, loopback, link-local, CGNAT (100.64/10),
/// documentation, broadcast or unspecified. Such an address on an interface
/// (or from a UPnP mapping) is reachable from anywhere.
pub fn is_public_v4(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let cgnat = o[0] == 100 && (64..128).contains(&o[1]);
            let documentation = matches!(
                (o[0], o[1], o[2]),
                (192, 0, 2) | (198, 51, 100) | (203, 0, 113)
            );
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || cgnat
                || documentation)
        }
        IpAddr::V6(_) => false,
    }
}

/// Global unicast IPv6 (2000::/3): reachable without NAT, firewalls allowing.
pub fn is_global_v6(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => (v6.segments()[0] & 0xe000) == 0x2000,
        _ => false,
    }
}

/// The source address the OS would use toward `probe` (no packet is sent:
/// connecting a UDP socket only picks a route).
fn route_source(bind: IpAddr, probe: SocketAddr) -> Option<IpAddr> {
    let socket = UdpSocket::bind(SocketAddr::new(bind, 0)).ok()?;
    socket.connect(probe).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// The addresses the OS would use toward the internet (no packets sent).
fn primary_ips() -> Vec<IpAddr> {
    let probes = [
        (
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            "192.0.2.1:9".parse().expect("valid"),
        ),
        (
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            "[2001:db8::1]:9".parse().expect("valid"),
        ),
    ];
    probes
        .into_iter()
        .filter_map(|(bind, probe)| route_source(bind, probe))
        .collect()
}

/// Name fragments of host-only adapters that hypervisors and container
/// runtimes create (Hyper-V/WSL, VirtualBox, VMware, Docker, Podman, libvirt, LXC).
/// Their addresses lead only to VMs and containers on this machine, so
/// offering them to peers is useless and can win a LAN race by accident.
/// VPN adapters (WireGuard, Tailscale, ZeroTier, OpenVPN) are kept: peers on
/// the same VPN can use them.
const VIRTUAL_ADAPTERS: &[&str] = &[
    "vethernet",
    "hyper-v",
    "wsl",
    "default switch",
    "virtualbox",
    "vboxnet",
    "vmware",
    "vmnet",
    "docker",
    "veth",
    "virbr",
    "br-",
    "lxcbr",
    "lxdbr",
    "podman",
];

/// Whether an adapter name looks like a hypervisor's or container runtime's
/// host-only adapter (see [`VIRTUAL_ADAPTERS`]).
pub fn is_virtual_adapter(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    VIRTUAL_ADAPTERS.iter().any(|v| name.contains(v))
}

/// Every usable address of every interface that is up (all LANs, VPNs,
/// IPv6), primary ones first. Loopback, link-local and unspecified
/// addresses are left out, and so are hypervisor/container adapters
/// ([`is_virtual_adapter`]) unless they carry the primary route (a
/// Hyper-V external switch, for example, is the real LAN). Falls back to the
/// primary addresses if the interfaces cannot be listed.
pub fn local_ips() -> Vec<IpAddr> {
    let mut ips = primary_ips();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            let ip = iface.ip();
            let usable = !iface.is_loopback()
                && !iface.is_link_local()
                && !ip.is_unspecified()
                && matches!(iface.oper_status, if_addrs::IfOperStatus::Up)
                && !is_virtual_adapter(&iface.name);
            if usable && !ips.contains(&ip) {
                ips.push(ip);
            }
        }
    }
    ips
}

/// [`local_ips`] combined with a port: what a device reports to the ID server.
/// IPv6 addresses are kept only if global and the socket can use them.
pub fn local_addrs(socket: &PeerSocket) -> Vec<SocketAddr> {
    local_ips()
        .into_iter()
        .filter(|ip| ip.is_ipv4() || (socket.is_dual_stack() && is_global_v6(*ip)))
        .map(|ip| SocketAddr::new(ip, socket.port()))
        .collect()
}

/// How many following ports to try toward a symmetric NAT.
pub const PREDICTION_WINDOW: u16 = 16;

/// Port prediction for symmetric NATs: many allocate new public ports
/// sequentially, so the mapping a peer opens toward us is likely just above
/// the one its ID-server connection got. Returns `addr`'s next `window` ports.
pub fn predicted_ports(addr: SocketAddr, window: u16) -> Vec<SocketAddr> {
    (1..=window)
        .filter_map(|i| addr.port().checked_add(i))
        .map(|port| SocketAddr::new(addr.ip(), port))
        .collect()
}

/// Sends `rounds` punch datagrams to each target, `interval` apart.
pub async fn punch(socket: &PeerSocket, targets: &[SocketAddr], rounds: u32, interval: Duration) {
    for round in 0..rounds {
        for &target in targets {
            if socket.can_reach(target) {
                // Best effort: an unreachable target must not stop the others.
                let _ = socket.send_raw(PUNCH_MAGIC, target);
            }
        }
        if round + 1 < rounds {
            tokio::time::sleep(interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn classification() {
        let locals = ["192.168.1.20".parse().unwrap()];
        // No NAT: the server sees our own address and port.
        assert_eq!(
            classify(
                &locals,
                5000,
                addr("192.168.1.20:5000"),
                addr("192.168.1.20:5000")
            ),
            NatType::Open
        );
        // Same public mapping toward both ports: cone.
        assert_eq!(
            classify(
                &locals,
                5000,
                addr("203.0.113.5:61000"),
                addr("203.0.113.5:61000")
            ),
            NatType::Cone
        );
        // Port preserved but behind a different public IP: still a NAT.
        assert_eq!(
            classify(
                &locals,
                5000,
                addr("203.0.113.5:5000"),
                addr("203.0.113.5:5000")
            ),
            NatType::Cone
        );
        // A new mapping per destination: symmetric.
        assert_eq!(
            classify(
                &locals,
                5000,
                addr("203.0.113.5:61000"),
                addr("203.0.113.5:61001")
            ),
            NatType::Symmetric
        );
        // Mapped IPv6 forms compare equal to IPv4.
        assert_eq!(
            classify(
                &locals,
                5000,
                addr("[::ffff:203.0.113.5]:7"),
                addr("203.0.113.5:7")
            ),
            NatType::Cone
        );
    }

    #[test]
    fn predicted_ports_follow_and_stop_at_the_top() {
        let p = predicted_ports(addr("203.0.113.5:61000"), 3);
        assert_eq!(
            p,
            vec![
                addr("203.0.113.5:61001"),
                addr("203.0.113.5:61002"),
                addr("203.0.113.5:61003")
            ]
        );
        assert_eq!(predicted_ports(addr("203.0.113.5:65534"), 5).len(), 1);
    }

    #[test]
    fn public_v4_detection() {
        for public in ["8.8.8.8", "1.1.1.1", "::ffff:8.8.4.4"] {
            assert!(is_public_v4(public.parse().unwrap()), "{public}");
        }
        for not in [
            "10.0.0.1",
            "172.16.5.4",
            "192.168.1.1",
            "127.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "203.0.113.5",
            "0.0.0.0",
            "255.255.255.255",
            "2001:db8::1",
        ] {
            assert!(!is_public_v4(not.parse().unwrap()), "{not}");
        }
    }

    #[test]
    fn address_kinds() {
        assert!(is_private_v4("10.1.2.3".parse().unwrap()));
        assert!(is_private_v4("::ffff:192.168.0.1".parse().unwrap()));
        assert!(!is_private_v4("8.8.8.8".parse().unwrap()));
        assert!(is_global_v6("2001:db8::1".parse().unwrap()));
        assert!(!is_global_v6("fe80::1".parse().unwrap()));
        assert!(!is_global_v6("::1".parse().unwrap()));
        assert!(!is_global_v6("::ffff:8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn virtual_adapters_are_recognised() {
        for name in [
            "vEthernet (WSL (Hyper-V firewall))",
            "vEthernet (Default Switch)",
            "VirtualBox Host-Only Network",
            "VMware Network Adapter VMnet8",
            "docker0",
            "veth1a2b3c",
            "virbr0",
            "br-5f2e0c1d",
            "lxdbr0",
        ] {
            assert!(is_virtual_adapter(name), "{name}");
        }
        for name in [
            "Ethernet",
            "Wi-Fi",
            "eth0",
            "wlan0",
            "en0",
            "Tailscale",
            "wg0",
            "ZeroTier One [abc]",
        ] {
            assert!(!is_virtual_adapter(name), "{name}");
        }
    }

    #[test]
    fn local_ips_finds_this_machine() {
        let ips = local_ips();
        assert!(ips.iter().any(|ip| ip.is_ipv4()), "{ips:?}");
        assert!(ips.iter().all(|ip| !ip.is_unspecified()));
    }
}
