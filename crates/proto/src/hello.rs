//! The `Hello` exchange and protocol-version negotiation.
//!
//! Each peer sends [`Hello`] first on the control stream: the highest protocol
//! version it speaks (`version`), the lowest it still accepts
//! (`min_version`), and its capabilities. [`negotiate`] picks the highest
//! version in the overlap of both ranges and the capabilities both sides
//! know. It is symmetric, so both peers reach the same result without another
//! round trip. With no overlap the client should close with
//! `CloseReason::IncompatibleVersion`.

use std::collections::BTreeSet;

use crate::v1::{Capability, Hello};

/// Highest protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// Lowest protocol version this build accepts.
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// Builds this build's `Hello`.
pub fn hello(capabilities: &[Capability], agent: &str) -> Hello {
    Hello {
        version: PROTOCOL_VERSION,
        min_version: MIN_PROTOCOL_VERSION,
        capabilities: capabilities.iter().map(|&c| c as i32).collect(),
        agent: agent.to_owned(),
    }
}

/// The outcome of a successful negotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    /// Protocol version both peers will use.
    pub version: u32,
    /// Capabilities both peers support.
    pub capabilities: BTreeSet<Capability>,
}

impl Negotiated {
    /// Whether both peers support `capability`.
    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// Why negotiation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NegotiationError {
    /// A `Hello` had version 0 or `min_version > version`.
    #[error("malformed hello: versions {min}..={max}")]
    Malformed {
        /// Its `min_version`.
        min: u32,
        /// Its `version`.
        max: u32,
    },
    /// The version ranges do not overlap.
    #[error("no common protocol version: local {local:?}, remote {remote:?}")]
    Incompatible {
        /// Local `(min_version, version)`.
        local: (u32, u32),
        /// Remote `(min_version, version)`.
        remote: (u32, u32),
    },
}

fn range(h: &Hello) -> Result<(u32, u32), NegotiationError> {
    if h.version == 0 || h.min_version > h.version {
        return Err(NegotiationError::Malformed {
            min: h.min_version,
            max: h.version,
        });
    }
    Ok((h.min_version, h.version))
}

fn known(h: &Hello) -> BTreeSet<Capability> {
    h.capabilities
        .iter()
        .filter_map(|&c| Capability::try_from(c).ok())
        .filter(|&c| c != Capability::Unspecified)
        .collect()
}

/// Agrees on a version and capability set. `negotiate(a, b) == negotiate(b, a)`.
pub fn negotiate(local: &Hello, remote: &Hello) -> Result<Negotiated, NegotiationError> {
    let (local_min, local_max) = range(local)?;
    let (remote_min, remote_max) = range(remote)?;
    let low = local_min.max(remote_min);
    let high = local_max.min(remote_max);
    if low > high {
        return Err(NegotiationError::Incompatible {
            local: (local_min, local_max),
            remote: (remote_min, remote_max),
        });
    }
    Ok(Negotiated {
        version: high,
        capabilities: known(local).intersection(&known(remote)).copied().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn h(min_version: u32, version: u32, caps: &[i32]) -> Hello {
        Hello {
            version,
            min_version,
            capabilities: caps.to_vec(),
            agent: String::new(),
        }
    }

    #[test]
    fn same_build_agrees_on_everything() {
        let caps = [Capability::VideoVp9, Capability::KeyframeRequest];
        let a = hello(&caps, "a");
        let n = negotiate(&a, &hello(&caps, "b")).unwrap();
        assert_eq!(n.version, PROTOCOL_VERSION);
        assert!(n.has(Capability::VideoVp9) && n.has(Capability::KeyframeRequest));
        assert!(!n.has(Capability::VideoH264));
    }

    #[test]
    fn picks_highest_common_version() {
        let n = negotiate(&h(1, 3, &[]), &h(2, 5, &[])).unwrap();
        assert_eq!(n.version, 3);
    }

    #[test]
    fn disjoint_ranges_are_incompatible() {
        let err = negotiate(&h(1, 2, &[]), &h(3, 4, &[])).unwrap_err();
        assert_eq!(
            err,
            NegotiationError::Incompatible {
                local: (1, 2),
                remote: (3, 4)
            }
        );
    }

    #[test]
    fn rejects_malformed_hellos() {
        assert!(matches!(
            negotiate(&h(0, 0, &[]), &h(1, 1, &[])),
            Err(NegotiationError::Malformed { .. })
        ));
        assert!(matches!(
            negotiate(&h(1, 1, &[]), &h(3, 2, &[])),
            Err(NegotiationError::Malformed { .. })
        ));
    }

    #[test]
    fn unknown_and_unspecified_capabilities_are_ignored() {
        let n = negotiate(&h(1, 1, &[0, 1, 999]), &h(1, 1, &[0, 1, 999])).unwrap();
        assert_eq!(
            n.capabilities.into_iter().collect::<Vec<_>>(),
            vec![Capability::VideoVp9]
        );
    }

    fn arb_hello() -> impl Strategy<Value = Hello> {
        (1u32..20, 0u32..5, prop::collection::vec(-2i32..15, 0..10))
            .prop_map(|(min, span, caps)| h(min, min + span, &caps))
    }

    proptest! {
        #[test]
        fn negotiation_is_symmetric(a in arb_hello(), b in arb_hello()) {
            prop_assert_eq!(negotiate(&a, &b), negotiate(&b, &a).map_err(|e| match e {
                NegotiationError::Incompatible { local, remote } =>
                    NegotiationError::Incompatible { local: remote, remote: local },
                other => other,
            }));
        }

        #[test]
        fn result_fits_both_peers(a in arb_hello(), b in arb_hello()) {
            let overlap = a.min_version.max(b.min_version) <= a.version.min(b.version);
            match negotiate(&a, &b) {
                Ok(n) => {
                    prop_assert!(overlap);
                    prop_assert!(n.version >= a.min_version && n.version <= a.version);
                    prop_assert!(n.version >= b.min_version && n.version <= b.version);
                    // Highest possible.
                    prop_assert_eq!(n.version, a.version.min(b.version));
                    for c in &n.capabilities {
                        prop_assert!(a.capabilities.contains(&(*c as i32)));
                        prop_assert!(b.capabilities.contains(&(*c as i32)));
                    }
                }
                Err(_) => prop_assert!(!overlap),
            }
        }
    }
}
