//! UPnP IGD port mapping (03 §5, "upnp_port").
//!
//! If the router supports UPnP, the host asks it to forward a public UDP
//! port to its device socket. The mapped public address is then reported to
//! the ID server like a local address, so controllers can reach the host
//! directly even when hole punching would fail (for example behind a
//! symmetric NAT). Best effort: many routers have UPnP off, and nothing
//! depends on it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use igd_next::aio::Gateway;
use igd_next::aio::tokio::{Tokio, search_gateway};
use igd_next::{GatewayIpVersion, PortMappingProtocol, SearchOptions};

use crate::TransportError;

/// Description shown in the router's mapping table.
const DESCRIPTION: &str = "KanoDesk";

/// A live port mapping on the router.
pub struct UpnpMapping {
    gateway: Gateway<Tokio>,
    external: SocketAddr,
    local: SocketAddr,
    lease: Duration,
}

fn err(context: &str, e: impl std::fmt::Display) -> TransportError {
    TransportError::Other(format!("UPnP {context}: {e}"))
}

/// The local IPv4 address used to reach `gateway` (no packet is sent).
fn local_ipv4_toward(gateway: SocketAddr) -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(gateway).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Finds the router and maps a public UDP port to `local_port` for `lease`
/// (renew before it ends with [`UpnpMapping::renew`]). Tries the same public
/// port first, then any port the router picks.
pub async fn map_udp(
    local_port: u16,
    lease: Duration,
    search_timeout: Duration,
) -> Result<UpnpMapping, TransportError> {
    let mut options = SearchOptions::default();
    options.timeout = Some(search_timeout);
    options.single_search_timeout = Some(search_timeout);
    options.gateway_ip_version = GatewayIpVersion::V4;
    let gateway = search_gateway(options)
        .await
        .map_err(|e| err("search", e))?;
    let local_ip = local_ipv4_toward(gateway.addr)
        .ok_or_else(|| TransportError::Other("UPnP: no IPv4 route to the router".into()))?;
    let local = SocketAddr::new(local_ip.into(), local_port);
    let external_ip = gateway
        .get_external_ip()
        .await
        .map_err(|e| err("external IP", e))?;
    let lease_secs = lease.as_secs().min(u32::MAX as u64) as u32;
    let external_port = match gateway
        .add_port(
            PortMappingProtocol::UDP,
            local_port,
            local,
            lease_secs,
            DESCRIPTION,
        )
        .await
    {
        Ok(()) => local_port,
        Err(_) => gateway
            .add_any_port(PortMappingProtocol::UDP, local, lease_secs, DESCRIPTION)
            .await
            .map_err(|e| err("add mapping", e))?,
    };
    Ok(UpnpMapping {
        gateway,
        external: SocketAddr::new(external_ip, external_port),
        local,
        lease,
    })
}

impl UpnpMapping {
    /// The public address peers can use.
    pub fn external(&self) -> SocketAddr {
        self.external
    }

    /// Extends the lease.
    pub async fn renew(&self) -> Result<(), TransportError> {
        self.gateway
            .add_port(
                PortMappingProtocol::UDP,
                self.external.port(),
                self.local,
                self.lease.as_secs().min(u32::MAX as u64) as u32,
                DESCRIPTION,
            )
            .await
            .map_err(|e| err("renew", e))
    }

    /// Removes the mapping from the router.
    pub async fn remove(self) -> Result<(), TransportError> {
        self.gateway
            .remove_port(PortMappingProtocol::UDP, self.external.port())
            .await
            .map_err(|e| err("remove", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_without_a_router_fails_cleanly_and_quickly() {
        // On CI and on networks without UPnP the search must end with an
        // error within the timeout, not hang. (With a UPnP router it maps.)
        let start = std::time::Instant::now();
        let result = map_udp(45999, Duration::from_secs(60), Duration::from_millis(500)).await;
        assert!(start.elapsed() < Duration::from_secs(5));
        if let Ok(mapping) = result {
            assert_ne!(mapping.external().port(), 0);
            let _ = mapping.remove().await;
        }
    }

    #[test]
    fn finds_a_local_route() {
        assert!(local_ipv4_toward("192.0.2.1:1900".parse().unwrap()).is_some());
    }
}
