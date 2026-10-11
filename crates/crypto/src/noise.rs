//! Noise IK session handshake (16 §5 item 3).
//!
//! Pattern `Noise_IK_25519_ChaChaPoly_BLAKE2s`:
//!
//! ```text
//! client (initiator)                                 host (responder)
//!   knows host device key H
//!   -> e, es, s, ss   payload: client ed25519 key  ─►  checks payload ↔ s
//!                                                ◄─  <- e, ee, se   (empty payload)
//!   both: SecureChannel{transport keys, handshake hash, remote key}
//! ```
//!
//! The prologue is `"kanodesk/v1 noise ik" ‖ len ‖ channel_binding`. Callers
//! pass the QUIC TLS keying-material exporter as `channel_binding`, so the
//! handshake only succeeds if both peers share the same TLS session: a
//! man-in-the-middle terminating TLS on each side gets two different
//! bindings. The 32-byte handshake hash then feeds the SPAKE2 login.
//!
//! Known IK property: the first message can be replayed to a host, which
//! answers it, but without the client's ephemeral key nobody can complete the
//! session or read the answer.

use snow::{Builder, HandshakeState, TransportState};
use zeroize::Zeroizing;

use crate::CryptoError;
use crate::identity::{DeviceIdentity, DevicePublicKey};

/// The Noise protocol name.
pub const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// Domain-separation label at the start of the prologue.
pub const PROLOGUE_LABEL: &[u8] = b"kanodesk/v1 noise ik";
/// Length of the handshake hash (BLAKE2s).
pub const HANDSHAKE_HASH_LEN: usize = 32;
/// Largest Noise message.
pub const MAX_MESSAGE: usize = 65535;
/// Largest plaintext that fits one sealed message (message minus the AEAD tag).
pub const MAX_PLAINTEXT: usize = MAX_MESSAGE - 16;

fn noise_err(e: snow::Error) -> CryptoError {
    CryptoError::Noise(e.to_string())
}

/// Builds the prologue from a channel binding.
pub fn prologue(channel_binding: &[u8]) -> Vec<u8> {
    let mut p = PROLOGUE_LABEL.to_vec();
    p.extend_from_slice(&(channel_binding.len() as u16).to_be_bytes());
    p.extend_from_slice(channel_binding);
    p
}

fn builder<'a>(private: &'a [u8], prologue: &'a [u8]) -> Result<Builder<'a>, CryptoError> {
    Builder::new(NOISE_PARAMS.parse().map_err(noise_err)?)
        .local_private_key(private)
        .map_err(noise_err)?
        .prologue(prologue)
        .map_err(noise_err)
}

/// An established session: Noise transport keys plus what the handshake proved.
pub struct SecureChannel {
    transport: TransportState,
    handshake_hash: [u8; HANDSHAKE_HASH_LEN],
    remote: DevicePublicKey,
}

impl SecureChannel {
    fn new(state: HandshakeState, remote: DevicePublicKey) -> Result<Self, CryptoError> {
        let handshake_hash: [u8; HANDSHAKE_HASH_LEN] = state
            .get_handshake_hash()
            .try_into()
            .map_err(|_| CryptoError::Noise("unexpected handshake hash length".into()))?;
        Ok(SecureChannel {
            transport: state.into_transport_mode().map_err(noise_err)?,
            handshake_hash,
            remote,
        })
    }

    /// The peer's authenticated device key.
    pub fn remote(&self) -> &DevicePublicKey {
        &self.remote
    }

    /// Hash of the whole handshake transcript; unique per session. The login
    /// binds to it.
    pub fn handshake_hash(&self) -> &[u8; HANDSHAKE_HASH_LEN] {
        &self.handshake_hash
    }

    /// Encrypts one message. Messages must be opened in the order sealed.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(CryptoError::MessageTooLarge {
                len: plaintext.len(),
            });
        }
        let mut out = vec![0u8; plaintext.len() + 16];
        let n = self
            .transport
            .write_message(plaintext, &mut out)
            .map_err(noise_err)?;
        out.truncate(n);
        Ok(out)
    }

    /// Decrypts one message. Fails on tampering, reordering or replay.
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if ciphertext.len() > MAX_MESSAGE {
            return Err(CryptoError::MessageTooLarge {
                len: ciphertext.len(),
            });
        }
        let mut out = vec![0u8; ciphertext.len()];
        let n = self
            .transport
            .read_message(ciphertext, &mut out)
            .map_err(noise_err)?;
        out.truncate(n);
        Ok(out)
    }
}

impl std::fmt::Debug for SecureChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureChannel")
            .field("remote", &self.remote)
            .finish_non_exhaustive()
    }
}

/// Client side of the handshake, between sending message 1 and reading message 2.
pub struct Initiator {
    state: HandshakeState,
    remote: DevicePublicKey,
}

impl Initiator {
    /// Starts a handshake to the host with device key `remote`. Returns the
    /// state and message 1.
    pub fn start(
        local: &DeviceIdentity,
        remote: &DevicePublicKey,
        channel_binding: &[u8],
    ) -> Result<(Initiator, Vec<u8>), CryptoError> {
        Self::start_inner(local, remote, channel_binding, None)
    }

    pub(crate) fn start_inner(
        local: &DeviceIdentity,
        remote: &DevicePublicKey,
        channel_binding: &[u8],
        fixed_ephemeral: Option<&[u8; 32]>,
    ) -> Result<(Initiator, Vec<u8>), CryptoError> {
        let private = local.noise_private();
        let prologue = prologue(channel_binding);
        let remote_static = remote.noise_public();
        let mut b = builder(private.as_ref(), &prologue)?
            .remote_public_key(&remote_static)
            .map_err(noise_err)?;
        if let Some(e) = fixed_ephemeral {
            b = b.fixed_ephemeral_key_for_testing_only(e);
        }
        let mut state = b.build_initiator().map_err(noise_err)?;
        let mut msg = vec![0u8; MAX_MESSAGE];
        let n = state
            .write_message(local.public_key().as_bytes(), &mut msg)
            .map_err(noise_err)?;
        msg.truncate(n);
        Ok((
            Initiator {
                state,
                remote: *remote,
            },
            msg,
        ))
    }

    /// Reads message 2 and completes the handshake.
    pub fn finish(mut self, msg2: &[u8]) -> Result<SecureChannel, CryptoError> {
        let mut payload = vec![0u8; msg2.len()];
        let n = self
            .state
            .read_message(msg2, &mut payload)
            .map_err(noise_err)?;
        if n != 0 {
            return Err(CryptoError::Noise("unexpected payload in message 2".into()));
        }
        SecureChannel::new(self.state, self.remote)
    }
}

/// Host side: reads message 1 and answers with message 2. The client's
/// device key is learned (and authenticated) here.
pub fn respond(
    local: &DeviceIdentity,
    channel_binding: &[u8],
    msg1: &[u8],
) -> Result<(SecureChannel, Vec<u8>), CryptoError> {
    respond_inner(local, channel_binding, msg1, None)
}

pub(crate) fn respond_inner(
    local: &DeviceIdentity,
    channel_binding: &[u8],
    msg1: &[u8],
    fixed_ephemeral: Option<&[u8; 32]>,
) -> Result<(SecureChannel, Vec<u8>), CryptoError> {
    if msg1.len() > MAX_MESSAGE {
        return Err(CryptoError::MessageTooLarge { len: msg1.len() });
    }
    let private: Zeroizing<[u8; 32]> = local.noise_private();
    let prologue = prologue(channel_binding);
    let mut b = builder(private.as_ref(), &prologue)?;
    if let Some(e) = fixed_ephemeral {
        b = b.fixed_ephemeral_key_for_testing_only(e);
    }
    let mut state = b.build_responder().map_err(noise_err)?;

    let mut payload = vec![0u8; msg1.len()];
    let n = state.read_message(msg1, &mut payload).map_err(noise_err)?;
    // The payload is the client's ed25519 key; it must convert to the Noise
    // static key the handshake just authenticated.
    let claimed: [u8; 32] = payload[..n]
        .try_into()
        .map_err(|_| CryptoError::Noise("message 1 payload is not a device key".into()))?;
    let client = DevicePublicKey::from_bytes(claimed)?;
    let remote_static = state
        .get_remote_static()
        .ok_or_else(|| CryptoError::Noise("no remote static key".into()))?;
    if remote_static != client.noise_public() {
        return Err(CryptoError::IdentityMismatch);
    }

    let mut msg2 = vec![0u8; MAX_MESSAGE];
    let n = state.write_message(&[], &mut msg2).map_err(noise_err)?;
    msg2.truncate(n);
    Ok((SecureChannel::new(state, client)?, msg2))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BINDING: &[u8] = b"tls exporter bytes";

    fn pair() -> (DeviceIdentity, DeviceIdentity) {
        (
            DeviceIdentity::generate().unwrap(),
            DeviceIdentity::generate().unwrap(),
        )
    }

    fn handshake(client: &DeviceIdentity, host: &DeviceIdentity) -> (SecureChannel, SecureChannel) {
        let (init, m1) = Initiator::start(client, &host.public_key(), BINDING).unwrap();
        let (host_ch, m2) = respond(host, BINDING, &m1).unwrap();
        let client_ch = init.finish(&m2).unwrap();
        (client_ch, host_ch)
    }

    #[test]
    fn handshake_authenticates_both_sides() {
        let (client, host) = pair();
        let (c, h) = handshake(&client, &host);
        assert_eq!(c.remote(), &host.public_key());
        assert_eq!(h.remote(), &client.public_key());
        assert_eq!(c.handshake_hash(), h.handshake_hash());
    }

    #[test]
    fn messages_flow_both_ways_in_order() {
        let (client, host) = pair();
        let (mut c, mut h) = handshake(&client, &host);
        for i in 0..3u8 {
            let sealed = c.seal(&[i; 10]).unwrap();
            assert_eq!(h.open(&sealed).unwrap(), vec![i; 10]);
            let back = h.seal(b"ack").unwrap();
            assert_eq!(c.open(&back).unwrap(), b"ack");
        }
    }

    #[test]
    fn sessions_have_distinct_handshake_hashes() {
        let (client, host) = pair();
        let (a, _) = handshake(&client, &host);
        let (b, _) = handshake(&client, &host);
        assert_ne!(a.handshake_hash(), b.handshake_hash());
    }

    #[test]
    fn wrong_host_key_fails() {
        let (client, host) = pair();
        let impostor = DeviceIdentity::generate().unwrap();
        // The client expects `host` but talks to `impostor`.
        let (init, m1) = Initiator::start(&client, &host.public_key(), BINDING).unwrap();
        assert!(respond(&impostor, BINDING, &m1).is_err());
        // Even if the impostor answered, the client would reject it.
        let (_, m1b) = Initiator::start(&client, &impostor.public_key(), BINDING).unwrap();
        let (_, m2) = respond(&impostor, BINDING, &m1b).unwrap();
        assert!(init.finish(&m2).is_err());
    }

    #[test]
    fn channel_binding_mismatch_fails() {
        // A MITM terminating TLS on each side sees two different exporters.
        let (client, host) = pair();
        let (_, m1) = Initiator::start(&client, &host.public_key(), b"session A").unwrap();
        assert!(respond(&host, b"session B", &m1).is_err());
    }

    #[test]
    fn tampered_handshake_messages_fail() {
        let (client, host) = pair();
        let (_, m1) = Initiator::start(&client, &host.public_key(), BINDING).unwrap();
        for i in [0, 40, m1.len() - 1] {
            let mut bad = m1.clone();
            bad[i] ^= 0x01;
            assert!(respond(&host, BINDING, &bad).is_err(), "byte {i}");
        }
        let (init, m1) = Initiator::start(&client, &host.public_key(), BINDING).unwrap();
        let (_, mut m2) = respond(&host, BINDING, &m1).unwrap();
        m2[5] ^= 0x80;
        assert!(init.finish(&m2).is_err());
    }

    #[test]
    fn tampered_reordered_or_replayed_transport_messages_fail() {
        let (client, host) = pair();
        let (mut c, mut h) = handshake(&client, &host);
        let first = c.seal(b"one").unwrap();
        let second = c.seal(b"two").unwrap();
        // Reordered: the second message cannot be opened first.
        assert!(h.open(&second).is_err());

        let (mut c, mut h) = handshake(&client, &host);
        let first_b = c.seal(b"one").unwrap();
        let mut tampered = first_b.clone();
        tampered[0] ^= 1;
        assert!(h.open(&tampered).is_err());

        let (mut c, mut h) = handshake(&client, &host);
        let m = c.seal(b"one").unwrap();
        assert_eq!(h.open(&m).unwrap(), b"one");
        // Replayed within the session.
        assert!(h.open(&m).is_err());
        // Replayed from another session.
        assert!(h.open(&first).is_err());
    }

    #[test]
    fn replayed_message_one_cannot_complete_a_session() {
        let (client, host) = pair();
        let (init, m1) = Initiator::start(&client, &host.public_key(), BINDING).unwrap();
        let (mut real, m2) = respond(&host, BINDING, &m1).unwrap();
        let mut client_ch = init.finish(&m2).unwrap();
        let sealed = client_ch.seal(b"login").unwrap();
        assert_eq!(real.open(&sealed).unwrap(), b"login");

        // An attacker replays message 1 and then the client's sealed message:
        // the host answers, but the new session has new keys.
        let (mut replayed, _m2) = respond(&host, BINDING, &m1).unwrap();
        assert_ne!(replayed.handshake_hash(), real.handshake_hash());
        assert!(replayed.open(&sealed).is_err());
    }

    #[test]
    fn identity_payload_must_match_static_key() {
        // A client that claims someone else's ed25519 key in the payload.
        let (client, host) = pair();
        let victim = DeviceIdentity::generate().unwrap();
        let private = client.noise_private();
        let prologue = prologue(BINDING);
        let remote = host.public_key().noise_public();
        let mut state = builder(private.as_ref(), &prologue)
            .unwrap()
            .remote_public_key(&remote)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut m1 = vec![0u8; MAX_MESSAGE];
        let n = state
            .write_message(victim.public_key().as_bytes(), &mut m1)
            .unwrap();
        m1.truncate(n);
        assert!(matches!(
            respond(&host, BINDING, &m1),
            Err(CryptoError::IdentityMismatch)
        ));
    }

    #[test]
    fn oversized_input_is_rejected() {
        let (client, host) = pair();
        let (mut c, _) = handshake(&client, &host);
        assert!(matches!(
            c.seal(&vec![0u8; MAX_PLAINTEXT + 1]),
            Err(CryptoError::MessageTooLarge { .. })
        ));
        assert!(respond(&host, BINDING, &vec![0u8; MAX_MESSAGE + 1]).is_err());
        assert!(respond(&host, BINDING, &[]).is_err());
    }
}
