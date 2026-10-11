//! IP networks in CIDR notation, for `--trusted-network`.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

/// An IPv4 or IPv6 network, e.g. `10.0.0.0/8` or `2001:db8::/32`. A bare
/// address is a single-host network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Whether `ip` is inside this network. IPv4-mapped IPv6 addresses
    /// (`::ffff:a.b.c.d`) count as their IPv4 address.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix as u32).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix as u32).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = match s.trim().split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s.trim(), None),
        };
        let addr: IpAddr = addr.parse().map_err(|e| format!("{s:?}: {e}"))?;
        let addr = addr.to_canonical();
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p.parse::<u8>().map_err(|e| format!("{s:?}: prefix: {e}"))?,
            None => max,
        };
        if prefix > max {
            return Err(format!("{s:?}: prefix longer than {max}"));
        }
        Ok(Cidr { addr, prefix })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn ipv4_networks() {
        let net: Cidr = "10.1.0.0/16".parse().unwrap();
        assert!(net.contains(ip("10.1.200.3")));
        assert!(!net.contains(ip("10.2.0.1")));
        assert!(net.contains(ip("::ffff:10.1.2.3")), "mapped IPv4 counts");
        assert!(!net.contains(ip("2001:db8::1")));
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(ip("203.0.113.9")));
        let host: Cidr = "192.0.2.7".parse().unwrap();
        assert!(host.contains(ip("192.0.2.7")) && !host.contains(ip("192.0.2.8")));
        assert_eq!(host.to_string(), "192.0.2.7/32");
    }

    #[test]
    fn ipv6_networks() {
        let net: Cidr = "2001:db8::/32".parse().unwrap();
        assert!(net.contains(ip("2001:db8:ffff::1")));
        assert!(!net.contains(ip("2001:db9::1")));
        assert!(!net.contains(ip("10.0.0.1")));
        let all: Cidr = "::/0".parse().unwrap();
        assert!(all.contains(ip("::1")));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "",
            "10.0.0.0/33",
            "::/129",
            "10.0.0/8",
            "x/1",
            "10.0.0.0/-1",
        ] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad:?}");
        }
    }
}
