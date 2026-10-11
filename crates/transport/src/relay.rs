//! Relay client side and the tickets that authorise relay sessions.
//!
//! See `relay.proto` for the protocol. The relay is blind: it forwards whole
//! QUIC packets between the two peers' UDP sockets, so TLS and Noise run end
//! to end through it.
//!
//! **Tickets**: the ID server and the relay share a 32-byte secret. For each
//! connect request the ID server issues a 16-byte session token and a ticket
//! `expires_at (u64 BE) || MAC(secret, session || expires_at)[..16]`. The
//! relay accepts a bind only with a valid, unexpired ticket, so it relays
//! only sessions the ID server created (no open relay).

use std::net::SocketAddr;
use std::time::Duration;

use kanodesk_proto::v1::{RelayBind, RelayMessage, RelayRole, relay_message::Msg};

use crate::peer::{PeerSocket, RawSender};
use crate::streams::open_relay;
use crate::{TransportError, endpoint};
use quinn::Connection;

/// Session token length.
pub const SESSION_LEN: usize = 16;
/// Ticket length: 8-byte expiry + 16-byte MAC.
pub const TICKET_LEN: usize = 24;
/// How long a ticket stays valid for binding.
pub const TICKET_LIFETIME: Duration = Duration::from_secs(120);
/// Prefix of hello datagrams sent to a session's data port.
pub const HELLO_MAGIC: &[u8] = b"KDRELAY1";
/// Hello datagram length: magic + role + session.
pub const HELLO_LEN: usize = 8 + 1 + SESSION_LEN;

/// The 32-byte secret shared by the ID server and the relay. Its `Debug`
/// output is redacted so it cannot leak into logs.
#[derive(Clone, PartialEq, Eq)]
pub struct RelaySecret(pub [u8; 32]);

impl std::fmt::Debug for RelaySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelaySecret(..)")
    }
}

impl RelaySecret {
    /// A new random secret.
    pub fn generate() -> Result<Self, TransportError> {
        let mut s = [0u8; 32];
        getrandom::fill(&mut s).map_err(|e| TransportError::Other(e.to_string()))?;
        Ok(RelaySecret(s))
    }

    /// Parses 64 hex digits (surrounding whitespace ignored).
    pub fn from_hex(text: &str) -> Result<Self, TransportError> {
        let text = text.trim();
        let bad = || TransportError::Other("relay secret must be 64 hex digits".into());
        if text.len() != 64 || !text.is_ascii() {
            return Err(bad());
        }
        let mut out = [0u8; 32];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(|_| bad())?;
        }
        Ok(RelaySecret(out))
    }

    /// Reads a secret file (64 hex digits).
    pub fn read_file(path: &std::path::Path) -> Result<Self, TransportError> {
        Self::from_hex(&std::fs::read_to_string(path)?)
    }

    /// The file contents for this secret.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn mac(secret: &[u8; 32], session: &[u8], expires_at: u64) -> [u8; 16] {
    let key = blake3::derive_key("kanodesk v1 relay ticket", secret);
    let mut h = blake3::Hasher::new_keyed(&key);
    h.update(session);
    h.update(&expires_at.to_be_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}

/// ID server: issues a ticket for `session`, valid until `expires_at` (Unix seconds).
pub fn issue_ticket(secret: &[u8; 32], session: &[u8; SESSION_LEN], expires_at: u64) -> Vec<u8> {
    let mut out = expires_at.to_be_bytes().to_vec();
    out.extend_from_slice(&mac(secret, session, expires_at));
    out
}

/// Relay: checks a ticket at Unix time `now`.
pub fn verify_ticket(secret: &[u8; 32], session: &[u8], ticket: &[u8], now: u64) -> bool {
    if session.len() != SESSION_LEN || ticket.len() != TICKET_LEN {
        return false;
    }
    let expires_at = u64::from_be_bytes(ticket[..8].try_into().expect("length checked"));
    let expected = mac(secret, session, expires_at);
    // Constant-time comparison of the MAC.
    let diff = expected
        .iter()
        .zip(&ticket[8..])
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    diff == 0 && now <= expires_at
}

/// A hello datagram for `session` in `role`.
pub fn hello(role: RelayRole, session: &[u8; SESSION_LEN]) -> Vec<u8> {
    let mut out = HELLO_MAGIC.to_vec();
    out.push(role as i32 as u8);
    out.extend_from_slice(session);
    out
}

/// Parses a hello datagram.
pub fn parse_hello(data: &[u8]) -> Option<(RelayRole, [u8; SESSION_LEN])> {
    if data.len() != HELLO_LEN || &data[..8] != HELLO_MAGIC {
        return None;
    }
    let role = match data[8] {
        1 => RelayRole::Controller,
        2 => RelayRole::Host,
        _ => return None,
    };
    Some((role, data[9..].try_into().expect("length checked")))
}

/// Registers this device for a relay session: binds on the relay's control
/// port and returns the session's data address. Then send hellos with
/// [`send_hellos`].
pub async fn bind(
    socket: &PeerSocket,
    relay: SocketAddr,
    session: &[u8; SESSION_LEN],
    ticket: &[u8],
    role: RelayRole,
) -> Result<SocketAddr, TransportError> {
    let conn = endpoint::connect(socket.endpoint(), relay).await?;
    let (mut send, mut recv) = open_relay(&conn).await?;
    send.send(&RelayMessage {
        msg: Some(Msg::Bind(RelayBind {
            session: session.to_vec(),
            ticket: ticket.to_vec(),
            role: role as i32,
        })),
    })
    .await?;
    let reply = recv.next().await?.and_then(|m| m.msg);
    endpoint::close(&conn, b"bound");
    match reply {
        Some(Msg::Bound(b)) => {
            let port = u16::try_from(b.port)
                .map_err(|_| TransportError::Other("relay sent a bad port".into()))?;
            Ok(SocketAddr::new(relay.ip(), port))
        }
        Some(Msg::Failure(f)) => Err(TransportError::Other(format!(
            "relay refused: {:?}",
            f.error()
        ))),
        _ => Err(TransportError::Other(
            "relay closed without answering".into(),
        )),
    }
}

/// Sends hello datagrams to a session's data address so the relay learns
/// where to forward (works behind any NAT, since it uses the data path).
pub async fn send_hellos(
    socket: &PeerSocket,
    data: SocketAddr,
    session: &[u8; SESSION_LEN],
    role: RelayRole,
    rounds: u32,
) {
    let msg = hello(role, session);
    for round in 0..rounds {
        let _ = socket.send_raw(&msg, data);
        if round + 1 < rounds {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Interval of keep-alive hellos on a relayed connection.
pub const HELLO_INTERVAL: Duration = Duration::from_secs(10);

/// Keeps a relayed session's address fresh while `conn` lives: sends a
/// hello every `interval`. If this device's NAT mapping changes (rebinding),
/// the relay learns the new address from the next hello. Also refreshes the
/// NAT mapping itself on quiet links.
pub fn spawn_hello_keepalive(
    sender: RawSender,
    data: SocketAddr,
    session: [u8; SESSION_LEN],
    role: RelayRole,
    conn: Connection,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let msg = hello(role, &session);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = conn.closed() => break,
                _ = tick.tick() => {
                    let _ = sender.send(&msg, data);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [7u8; 32];
    const SESSION: [u8; 16] = [3u8; 16];

    #[test]
    fn tickets_verify_until_expiry() {
        let t = issue_ticket(&SECRET, &SESSION, 1000);
        assert_eq!(t.len(), TICKET_LEN);
        assert!(verify_ticket(&SECRET, &SESSION, &t, 999));
        assert!(verify_ticket(&SECRET, &SESSION, &t, 1000));
        assert!(!verify_ticket(&SECRET, &SESSION, &t, 1001), "expired");
    }

    #[test]
    fn tickets_are_bound_to_secret_session_and_expiry() {
        let t = issue_ticket(&SECRET, &SESSION, 1000);
        assert!(!verify_ticket(&[8u8; 32], &SESSION, &t, 0), "other secret");
        assert!(!verify_ticket(&SECRET, &[4u8; 16], &t, 0), "other session");
        let mut extended = t.clone();
        extended[..8].copy_from_slice(&5000u64.to_be_bytes());
        assert!(
            !verify_ticket(&SECRET, &SESSION, &extended, 2000),
            "forged expiry"
        );
        let mut flipped = t.clone();
        flipped[20] ^= 1;
        assert!(!verify_ticket(&SECRET, &SESSION, &flipped, 0));
        assert!(!verify_ticket(&SECRET, &SESSION, &t[..20], 0));
        assert!(!verify_ticket(&SECRET, &SESSION[..8], &t, 0));
    }

    #[test]
    fn secrets_parse_and_stay_out_of_debug_output() {
        let s = RelaySecret::generate().unwrap();
        assert_eq!(
            RelaySecret::from_hex(&format!("  {}\n", s.to_hex())).unwrap(),
            s
        );
        assert!(RelaySecret::from_hex("abcd").is_err());
        assert!(RelaySecret::from_hex(&"zz".repeat(32)).is_err());
        assert_eq!(format!("{s:?}"), "RelaySecret(..)");
    }

    #[test]
    fn hellos_round_trip_and_reject_junk() {
        for role in [RelayRole::Controller, RelayRole::Host] {
            assert_eq!(parse_hello(&hello(role, &SESSION)), Some((role, SESSION)));
        }
        assert_eq!(hello(RelayRole::Host, &SESSION).len(), HELLO_LEN);
        assert!(parse_hello(b"KDRELAY1").is_none());
        let mut bad_role = hello(RelayRole::Host, &SESSION);
        bad_role[8] = 9;
        assert!(parse_hello(&bad_role).is_none());
        let mut bad_magic = hello(RelayRole::Host, &SESSION);
        bad_magic[0] = b'X';
        assert!(parse_hello(&bad_magic).is_none());
    }
}
