//! Reaching the ID server from a device socket, the steps every device
//! takes before registering or asking for a connection (03 §2): read the
//! configured address (port 21116 unless one is given), resolve it, connect
//! and check the server's key, then detect the NAT type through the
//! NAT-test port.

use std::net::SocketAddr;

use kanodesk_crypto::DevicePublicKey;
use kanodesk_proto::v1::NatType;
use kanodesk_transport::Endpoint;

use crate::{ClientError, DEFAULT_PORT, RendezvousClient, RendezvousConfig};

/// `server` with the default port added when it has none (`host`,
/// `1.2.3.4`, `[::1]` → `…:21116`).
pub fn with_default_port(server: &str) -> String {
    let server = server.trim();
    let has_port = if let Some(rest) = server.strip_prefix('[') {
        // [v6] or [v6]:port
        rest.split_once(']')
            .is_some_and(|(_, after)| after.starts_with(':'))
    } else {
        // A bare IPv6 address has several colons and no port.
        server.matches(':').count() == 1
    };
    if has_port || server.is_empty() {
        server.to_owned()
    } else if server.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{server}]:{DEFAULT_PORT}")
    } else {
        format!("{server}:{DEFAULT_PORT}")
    }
}

/// Resolves `host:port`, preferring IPv4 (the device socket handles both).
pub async fn resolve(name: &str) -> Result<SocketAddr, ClientError> {
    let all: Vec<SocketAddr> = tokio::net::lookup_host(name)
        .await
        .map_err(|e| ClientError::Resolve(format!("{name}: {e}")))?
        .collect();
    all.iter()
        .find(|a| a.is_ipv4())
        .or(all.first())
        .copied()
        .ok_or_else(|| ClientError::Resolve(format!("{name}: no addresses")))
}

/// A verified ID-server connection and this device's NAT type.
pub struct Link {
    /// The connection.
    pub client: RendezvousClient,
    /// What NAT detection found (`Unknown` if it failed).
    pub nat: NatType,
}

/// Connects to the ID server `server` (`host[:port]`) from `endpoint`
/// (bound on `local_port`), checks it holds `server_key`, and detects the
/// NAT type through `nat_test` (default: the server's port minus one).
pub async fn open(
    endpoint: &Endpoint,
    local_port: u16,
    server: &str,
    server_key: DevicePublicKey,
    nat_test: Option<&str>,
) -> Result<Link, ClientError> {
    let server_addr = resolve(&with_default_port(server)).await?;
    let client = RendezvousClient::connect(
        endpoint,
        RendezvousConfig {
            server_addr,
            server_key,
        },
    )
    .await?;
    let nat_test = match nat_test {
        Some(name) => resolve(&with_default_port(name)).await?,
        None => SocketAddr::new(server_addr.ip(), server_addr.port().saturating_sub(1)),
    };
    let nat = match client.detect_nat(endpoint, nat_test, local_port).await {
        Ok(t) => t,
        Err(e) => {
            tracing::info!("NAT detection failed ({e}); continuing as unknown");
            NatType::Unknown
        }
    };
    Ok(Link { client, nat })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port_is_added_only_when_missing() {
        assert_eq!(
            with_default_port("ids.example.com"),
            "ids.example.com:21116"
        );
        assert_eq!(with_default_port(" 10.0.0.5 "), "10.0.0.5:21116");
        assert_eq!(with_default_port("10.0.0.5:4000"), "10.0.0.5:4000");
        assert_eq!(with_default_port("::1"), "[::1]:21116");
        assert_eq!(with_default_port("[::1]"), "[::1]:21116");
        assert_eq!(with_default_port("[::1]:4000"), "[::1]:4000");
        assert_eq!(with_default_port(""), "");
    }

    #[tokio::test]
    async fn resolution_prefers_ipv4_and_reports_failures() {
        assert_eq!(
            resolve("127.0.0.1:5").await.unwrap(),
            "127.0.0.1:5".parse().unwrap()
        );
        assert!(matches!(
            resolve("no-such-host.invalid:1").await,
            Err(ClientError::Resolve(_))
        ));
    }
}
