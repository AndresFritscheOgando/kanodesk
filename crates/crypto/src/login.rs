//! Password login with SPAKE2, bound to the Noise session (16 §5 item 4).
//!
//! The host may have two passwords: a temporary one and an optional
//! permanent one. The user just types "the password", so the client runs one
//! SPAKE2 exchange per **slot** with the same typed password, and the host
//! answers each slot with its own password (a random never-matching one if
//! that slot is unset, so the reply does not reveal which passwords exist).
//!
//! ```text
//! client                                             host
//!   AuthStart   [A_temp, A_perm]           ─►  SPAKE2 B side per slot
//!                                          ◄─  AuthFinish  [B_temp, B_perm]
//!   keys k_i; AuthConfirm [mac_c_temp, mac_c_perm] ─►  any mac_c_i valid?
//!                                          ◄─  LoginResult ok + mac_h_i  /  WrongPassword
//!   check mac_h_i
//! ```
//!
//! - **Binding**: the SPAKE2 identities and both MACs include the Noise
//!   handshake hash, so messages from one session are useless in another
//!   (replay) and a relay cannot splice two sessions together.
//! - **Order**: the client proves first. A client that stops after
//!   `AuthFinish` learns nothing about its guess, so only completed attempts
//!   matter, and the host counts each one ([`crate::throttle`]).
//! - **Guesses**: an attacker gets one guess per slot per attempt; there is
//!   no offline dictionary attack on the transcript (SPAKE2's property).
//! - Temporary passwords are compared case-insensitively
//!   ([`crate::password::normalize_temporary`]); permanent ones exactly.

use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use crate::password::normalize_temporary;
use crate::{CryptoError, random_bytes};

/// Number of password slots.
pub const SLOTS: usize = 2;
/// Size of each key-confirmation MAC.
pub const MAC_LEN: usize = 32;

/// Which host password a login matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordKind {
    /// The temporary (one-time) password.
    Temporary,
    /// The permanent password.
    Permanent,
}

impl PasswordKind {
    fn slot(self) -> usize {
        match self {
            PasswordKind::Temporary => 0,
            PasswordKind::Permanent => 1,
        }
    }

    fn from_slot(slot: usize) -> Self {
        if slot == 0 {
            PasswordKind::Temporary
        } else {
            PasswordKind::Permanent
        }
    }
}

/// The SPAKE2 input for a slot: temporary passwords are normalised.
fn slot_password(slot: usize, password: &str) -> Zeroizing<Vec<u8>> {
    match PasswordKind::from_slot(slot) {
        PasswordKind::Temporary => Zeroizing::new(normalize_temporary(password).into_bytes()),
        PasswordKind::Permanent => Zeroizing::new(password.as_bytes().to_vec()),
    }
}

fn identities(slot: usize, handshake_hash: &[u8]) -> (Identity, Identity) {
    let id = |role: &[u8]| {
        let mut v = b"kanodesk/v1 login ".to_vec();
        v.extend_from_slice(role);
        v.push(slot as u8);
        v.extend_from_slice(handshake_hash);
        Identity::new(&v)
    };
    (id(b"client"), id(b"host"))
}

/// Confirmation MAC: keyed BLAKE3 over role, slot, handshake hash and both
/// SPAKE2 messages of the slot, under a key derived from the SPAKE2 key.
pub(crate) fn confirmation_mac(
    key: &[u8],
    role: &[u8],
    slot: usize,
    handshake_hash: &[u8],
    msg_a: &[u8],
    msg_b: &[u8],
) -> [u8; MAC_LEN] {
    let mac_key = Zeroizing::new(blake3::derive_key("kanodesk v1 login confirmation", key));
    let mut h = blake3::Hasher::new_keyed(&mac_key);
    for part in [role, &[slot as u8], handshake_hash, msg_a, msg_b] {
        h.update(&(part.len() as u32).to_be_bytes());
        h.update(part);
    }
    *h.finalize().as_bytes()
}

/// Constant-time equality for MACs.
fn mac_eq(a: &[u8], b: &[u8; MAC_LEN]) -> bool {
    a.len() == MAC_LEN
        && blake3::Hash::from(*b) == blake3::Hash::from_slice(a).expect("length checked")
}

struct SlotKeys {
    key: Zeroizing<Vec<u8>>,
    msg_a: Vec<u8>,
    msg_b: Vec<u8>,
}

/// Client side, after sending `AuthStart`.
pub struct ClientLogin {
    states: Vec<(Spake2<Ed25519Group>, Vec<u8>)>,
    handshake_hash: Vec<u8>,
}

impl ClientLogin {
    /// Starts a login with the password the user typed. Returns the state
    /// and the `AuthStart` messages (one per slot).
    pub fn start(password: &str, handshake_hash: &[u8]) -> (ClientLogin, Vec<Vec<u8>>) {
        Self::start_with(password, handshake_hash, |pw, a, b| {
            Spake2::<Ed25519Group>::start_a(pw, a, b)
        })
    }

    pub(crate) fn start_with(
        password: &str,
        handshake_hash: &[u8],
        mut start_a: impl FnMut(&Password, &Identity, &Identity) -> (Spake2<Ed25519Group>, Vec<u8>),
    ) -> (ClientLogin, Vec<Vec<u8>>) {
        let mut states = Vec::with_capacity(SLOTS);
        let mut messages = Vec::with_capacity(SLOTS);
        for slot in 0..SLOTS {
            let (id_a, id_b) = identities(slot, handshake_hash);
            let pw = slot_password(slot, password);
            let (state, msg) = start_a(&Password::new(&pw), &id_a, &id_b);
            messages.push(msg.clone());
            states.push((state, msg));
        }
        (
            ClientLogin {
                states,
                handshake_hash: handshake_hash.to_vec(),
            },
            messages,
        )
    }

    /// Reads `AuthFinish` (the host's SPAKE2 messages) and returns the
    /// `AuthConfirm` MACs to send, plus the state needed to check the host.
    pub fn respond(
        self,
        host_messages: &[Vec<u8>],
    ) -> Result<(ClientConfirmed, Vec<Vec<u8>>), CryptoError> {
        if host_messages.len() != SLOTS {
            return Err(CryptoError::Malformed("wrong number of host messages"));
        }
        let mut slots = Vec::with_capacity(SLOTS);
        let mut macs = Vec::with_capacity(SLOTS);
        for (slot, ((state, msg_a), msg_b)) in
            self.states.into_iter().zip(host_messages).enumerate()
        {
            let key = Zeroizing::new(
                state
                    .finish(msg_b)
                    .map_err(|_| CryptoError::Malformed("bad host SPAKE2 message"))?,
            );
            macs.push(
                confirmation_mac(&key, b"client", slot, &self.handshake_hash, &msg_a, msg_b)
                    .to_vec(),
            );
            slots.push(SlotKeys {
                key,
                msg_a,
                msg_b: msg_b.clone(),
            });
        }
        Ok((
            ClientConfirmed {
                slots,
                handshake_hash: self.handshake_hash,
            },
            macs,
        ))
    }
}

/// Client side, after sending `AuthConfirm`.
pub struct ClientConfirmed {
    slots: Vec<SlotKeys>,
    handshake_hash: Vec<u8>,
}

impl ClientConfirmed {
    /// Checks the host's confirmation from a successful `LoginResult`.
    /// Returns which password matched.
    pub fn verify_host(&self, host_mac: &[u8]) -> Result<PasswordKind, CryptoError> {
        for (slot, s) in self.slots.iter().enumerate() {
            let expected = confirmation_mac(
                &s.key,
                b"host",
                slot,
                &self.handshake_hash,
                &s.msg_a,
                &s.msg_b,
            );
            if mac_eq(host_mac, &expected) {
                return Ok(PasswordKind::from_slot(slot));
            }
        }
        Err(CryptoError::HostNotVerified)
    }
}

/// The host's passwords as login inputs.
pub struct HostSecrets<'a> {
    /// Current temporary password.
    pub temporary: Option<&'a str>,
    /// Permanent password, if set.
    pub permanent: Option<&'a str>,
}

/// Host side, after sending `AuthFinish`.
pub struct HostLogin {
    slots: Vec<SlotKeys>,
    enabled: [bool; SLOTS],
    handshake_hash: Vec<u8>,
}

impl HostLogin {
    /// Answers `AuthStart`. Returns the state and the `AuthFinish` messages.
    pub fn start(
        secrets: &HostSecrets<'_>,
        handshake_hash: &[u8],
        client_messages: &[Vec<u8>],
    ) -> Result<(HostLogin, Vec<Vec<u8>>), CryptoError> {
        Self::start_with(secrets, handshake_hash, client_messages, |pw, a, b| {
            Spake2::<Ed25519Group>::start_b(pw, a, b)
        })
    }

    pub(crate) fn start_with(
        secrets: &HostSecrets<'_>,
        handshake_hash: &[u8],
        client_messages: &[Vec<u8>],
        mut start_b: impl FnMut(&Password, &Identity, &Identity) -> (Spake2<Ed25519Group>, Vec<u8>),
    ) -> Result<(HostLogin, Vec<Vec<u8>>), CryptoError> {
        if client_messages.len() != SLOTS {
            return Err(CryptoError::Malformed("wrong number of client messages"));
        }
        let passwords = [secrets.temporary, secrets.permanent];
        let mut enabled = [false; SLOTS];
        let mut slots = Vec::with_capacity(SLOTS);
        let mut replies = Vec::with_capacity(SLOTS);
        for (slot, msg_a) in client_messages.iter().enumerate() {
            let pw = match passwords[slot] {
                Some(p) if !p.is_empty() => {
                    enabled[slot] = true;
                    slot_password(slot, p)
                }
                // Unset slot: a random password nobody can match, so the
                // reply looks the same as for a real one.
                _ => {
                    let mut random = Zeroizing::new(vec![0u8; 32]);
                    random_bytes(&mut random)?;
                    random
                }
            };
            let (id_a, id_b) = identities(slot, handshake_hash);
            let (state, msg_b) = start_b(&Password::new(&pw), &id_a, &id_b);
            let key = Zeroizing::new(
                state
                    .finish(msg_a)
                    .map_err(|_| CryptoError::Malformed("bad client SPAKE2 message"))?,
            );
            replies.push(msg_b.clone());
            slots.push(SlotKeys {
                key,
                msg_a: msg_a.clone(),
                msg_b,
            });
        }
        Ok((
            HostLogin {
                slots,
                enabled,
                handshake_hash: handshake_hash.to_vec(),
            },
            replies,
        ))
    }

    /// Checks `AuthConfirm`. On success returns the matched password kind and
    /// the host confirmation MAC to send in `LoginResult`.
    pub fn verify(self, client_macs: &[Vec<u8>]) -> Result<(PasswordKind, Vec<u8>), CryptoError> {
        if client_macs.len() != SLOTS {
            return Err(CryptoError::WrongPassword);
        }
        // Check every slot (no early exit) to keep timing uniform.
        let mut matched = None;
        for (slot, s) in self.slots.iter().enumerate() {
            let expected = confirmation_mac(
                &s.key,
                b"client",
                slot,
                &self.handshake_hash,
                &s.msg_a,
                &s.msg_b,
            );
            if mac_eq(&client_macs[slot], &expected) && self.enabled[slot] && matched.is_none() {
                matched = Some(slot);
            }
        }
        let slot = matched.ok_or(CryptoError::WrongPassword)?;
        let kind = PasswordKind::from_slot(slot);
        let s = &self.slots[kind.slot()];
        let host_mac = confirmation_mac(
            &s.key,
            b"host",
            slot,
            &self.handshake_hash,
            &s.msg_a,
            &s.msg_b,
        );
        Ok((kind, host_mac.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &[u8] = &[7u8; 32];

    /// Runs a full login; returns (host result, client view of the host).
    fn login(
        typed: &str,
        temporary: Option<&str>,
        permanent: Option<&str>,
    ) -> Result<(PasswordKind, PasswordKind), CryptoError> {
        let (client, start) = ClientLogin::start(typed, H);
        let secrets = HostSecrets {
            temporary,
            permanent,
        };
        let (host, finish) = HostLogin::start(&secrets, H, &start)?;
        let (confirmed, macs) = client.respond(&finish)?;
        let (kind, host_mac) = host.verify(&macs)?;
        Ok((kind, confirmed.verify_host(&host_mac)?))
    }

    #[test]
    fn temporary_password_logs_in() {
        let (host, client) = login("k7mq4z2p", Some("k7mq4z2p"), None).unwrap();
        assert_eq!(
            (host, client),
            (PasswordKind::Temporary, PasswordKind::Temporary)
        );
    }

    #[test]
    fn temporary_password_ignores_case_and_spaces() {
        assert!(login("  K7MQ4Z2P ", Some("k7mq4z2p"), None).is_ok());
    }

    #[test]
    fn permanent_password_logs_in_and_is_case_sensitive() {
        let (host, client) =
            login("Correct Horse", Some("k7mq4z2p"), Some("Correct Horse")).unwrap();
        assert_eq!(
            (host, client),
            (PasswordKind::Permanent, PasswordKind::Permanent)
        );
        assert!(matches!(
            login("correct horse", Some("k7mq4z2p"), Some("Correct Horse")),
            Err(CryptoError::WrongPassword)
        ));
    }

    #[test]
    fn wrong_password_is_rejected() {
        assert!(matches!(
            login("guess123", Some("k7mq4z2p"), Some("Correct Horse")),
            Err(CryptoError::WrongPassword)
        ));
        // An unset permanent slot never matches, whatever is typed.
        assert!(matches!(
            login("", Some("k7mq4z2p"), None),
            Err(CryptoError::WrongPassword)
        ));
        assert!(matches!(
            login("anything", None, None),
            Err(CryptoError::WrongPassword)
        ));
    }

    #[test]
    fn different_handshake_hashes_do_not_mix() {
        // A transcript bound to one Noise session fails in another.
        let (client, start) = ClientLogin::start("k7mq4z2p", &[1u8; 32]);
        let secrets = HostSecrets {
            temporary: Some("k7mq4z2p"),
            permanent: None,
        };
        let (host, finish) = HostLogin::start(&secrets, &[2u8; 32], &start).unwrap();
        let (_, macs) = client.respond(&finish).unwrap();
        assert!(matches!(
            host.verify(&macs),
            Err(CryptoError::WrongPassword)
        ));
    }

    #[test]
    fn replayed_confirmation_from_another_session_is_rejected() {
        let secrets = HostSecrets {
            temporary: Some("k7mq4z2p"),
            permanent: None,
        };
        // Session 1 completes; an eavesdropper records AuthStart and AuthConfirm.
        let (client, start) = ClientLogin::start("k7mq4z2p", H);
        let (host, finish) = HostLogin::start(&secrets, H, &start).unwrap();
        let (_, recorded_macs) = client.respond(&finish).unwrap();
        assert!(host.verify(&recorded_macs).is_ok());

        // Session 2 (even with the same handshake hash, which Noise never
        // produces twice): the host picks fresh SPAKE2 randomness, so the
        // recorded confirmation does not match.
        let (host2, _) = HostLogin::start(&secrets, H, &start).unwrap();
        assert!(matches!(
            host2.verify(&recorded_macs),
            Err(CryptoError::WrongPassword)
        ));
    }

    #[test]
    fn tampered_messages_are_detected() {
        let secrets = HostSecrets {
            temporary: Some("k7mq4z2p"),
            permanent: None,
        };
        // Tampered client SPAKE2 message: either malformed or a wrong key.
        let (client, mut start) = ClientLogin::start("k7mq4z2p", H);
        start[0][10] ^= 0x01;
        match HostLogin::start(&secrets, H, &start) {
            Err(CryptoError::Malformed(_)) => {}
            Ok((host, finish)) => {
                let (_, macs) = client.respond(&finish).unwrap();
                assert!(host.verify(&macs).is_err());
            }
            Err(e) => panic!("unexpected {e}"),
        }

        // Tampered host SPAKE2 message: the client's MAC no longer verifies.
        let (client, start) = ClientLogin::start("k7mq4z2p", H);
        let (host, mut finish) = HostLogin::start(&secrets, H, &start).unwrap();
        finish[0][10] ^= 0x01;
        if let Ok((_, macs)) = client.respond(&finish) {
            assert!(host.verify(&macs).is_err());
        }

        // Tampered confirmation.
        let (client, start) = ClientLogin::start("k7mq4z2p", H);
        let (host, finish) = HostLogin::start(&secrets, H, &start).unwrap();
        let (confirmed, mut macs) = client.respond(&finish).unwrap();
        macs[0][0] ^= 0x01;
        assert!(host.verify(&macs).is_err());

        // Tampered host confirmation.
        let (client, start) = ClientLogin::start("k7mq4z2p", H);
        let (host, finish) = HostLogin::start(&secrets, H, &start).unwrap();
        let (confirmed2, macs) = client.respond(&finish).unwrap();
        let (_, mut host_mac) = host.verify(&macs).unwrap();
        host_mac[31] ^= 0x01;
        assert!(matches!(
            confirmed2.verify_host(&host_mac),
            Err(CryptoError::HostNotVerified)
        ));
        assert!(confirmed.verify_host(&[0u8; 32]).is_err());
    }

    #[test]
    fn wrong_shapes_are_rejected() {
        let secrets = HostSecrets {
            temporary: Some("k7mq4z2p"),
            permanent: None,
        };
        assert!(HostLogin::start(&secrets, H, &[vec![1, 2, 3]]).is_err());
        assert!(HostLogin::start(&secrets, H, &[vec![], vec![]]).is_err());
        let (client, start) = ClientLogin::start("x", H);
        let (host, _) = HostLogin::start(&secrets, H, &start).unwrap();
        assert!(client.respond(&[]).is_err());
        assert!(host.verify(&[vec![0; 32]]).is_err());
    }
}
