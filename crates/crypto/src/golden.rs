//! Golden test vectors.
//!
//! - RFC 8032 §7.1 test 1 checks our ed25519 usage against the standard.
//! - The rest are **regression vectors** produced by this implementation
//!   with fixed keys, fixed Noise ephemerals and a deterministic SPAKE2 RNG.
//!   They pin the wire format and key schedule: if one changes, either a
//!   dependency changed behaviour or the protocol did, and old and new peers
//!   would no longer interoperate. Change them only together with
//!   `PROTOCOL_VERSION`.

use rand_core_06::{CryptoRng, RngCore};
use spake2::{Ed25519Group, Spake2};

use crate::identity::DeviceIdentity;
use crate::login::{ClientLogin, HostLogin, HostSecrets, confirmation_mac};
use crate::noise::{Initiator, respond_inner};
use crate::password::{Charset, TemporaryPolicy, generate_with};
use crate::{from_hex, to_hex};

/// Deterministic RNG for SPAKE2: the BLAKE3 XOF of a label.
struct TestRng(blake3::OutputReader);

impl TestRng {
    fn new(label: &str) -> Self {
        TestRng(
            blake3::Hasher::new()
                .update(label.as_bytes())
                .finalize_xof(),
        )
    }
}

impl RngCore for TestRng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.0.fill(&mut b);
        u32::from_le_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.0.fill(&mut b);
        u64::from_le_bytes(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill(dest);
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core_06::Error> {
        self.0.fill(dest);
        Ok(())
    }
}

impl CryptoRng for TestRng {}

const CLIENT_SEED: [u8; 32] = [1u8; 32];
const HOST_SEED: [u8; 32] = [2u8; 32];
const CLIENT_EPHEMERAL: [u8; 32] = [3u8; 32];
const HOST_EPHEMERAL: [u8; 32] = [4u8; 32];
const BINDING: &[u8] = b"golden channel binding";

// RFC 8032 §7.1, TEST 1 (empty message).
const RFC8032_SECRET: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const RFC8032_PUBLIC: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
const RFC8032_SIGNATURE: &str = "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";

// Regression vectors (see module docs).
const RFC8032_NOISE_PUBLIC: &str =
    "d85e07ec22b0ad881537c2f44d662d1a143cf830c57aca4305d85c7a90f6b62e";
const NOISE_MSG1: &str = "5dfedd3b6bd47f6fa28ee15d969d5bb0ea53774d488bdaf9df1c6e0124b3ef22847b462007e671e59564cf6d7473c384cfd5aa5e787b199015dd7c5aef9db8ff52b71db09a9eef8c3e3a904aeb138efdbe81bfa9fae6c925c044224af7e7928d3bc65c4bc2aec28292fb8a4fdba2ac54fc72a8199605f71708d6c9700fe0d217";
const NOISE_MSG2: &str = "ac01b2209e86354fb853237b5de0f4fab13c7fcbf433a61c019369617fecf10be5f06ef525ac6ac76feb8de5543df383";
const NOISE_HASH: &str = "ffe7c68b097318c5c4ab65d881b8594d30b45def1f2fbe04e2bd800406371361";
const NOISE_FIRST_SEALED: &str = "8be111de0d3232dc204983a0485ea495ea57108482";
const LOGIN_START_0: &str = "4190156ff138da04dd23d413c31d982c8922fe34b42226ff1cce925fd9f97e1f68";
const LOGIN_FINISH_0: &str = "420d476a6e42ffa1307271f3b88410255d1f0c0a9b492b6758c8ae55fcc156764b";
const LOGIN_CLIENT_MAC_0: &str = "46bd7106d237c082621dc1a39da61623fe8931083181bd9f4911e24e095dd767";
const LOGIN_HOST_MAC: &str = "bcd118339e1c6582d55c4e1ff1b69061db7d7481cf53ba1d2f4f06ebf8050801";
const MAC_KAT: &str = "eb3729265e798af8289a29f019ae0155247d07cae3f6d50fec624c63531d3361";
const TEMPORARY_PASSWORD: &str = "du8iifvg2k";

#[test]
fn rfc8032_test_1() {
    let seed = from_hex::<32>(RFC8032_SECRET).unwrap();
    let id = DeviceIdentity::from_seed(&seed);
    assert_eq!(id.public_key().to_string(), RFC8032_PUBLIC);
    assert_eq!(to_hex(&id.sign(b"")), RFC8032_SIGNATURE);
    let sig = from_hex::<64>(RFC8032_SIGNATURE).unwrap();
    assert!(id.public_key().verify(b"", &sig));
}

#[test]
fn rfc8032_key_as_noise_key() {
    let id = DeviceIdentity::from_seed(&from_hex::<32>(RFC8032_SECRET).unwrap());
    assert_eq!(
        to_hex(&id.public_key().noise_public()),
        RFC8032_NOISE_PUBLIC
    );
}

/// Runs the fixed Noise handshake; returns (msg1, msg2, client channel, host channel).
fn noise() -> (Vec<u8>, Vec<u8>, crate::SecureChannel, crate::SecureChannel) {
    let client = DeviceIdentity::from_seed(&CLIENT_SEED);
    let host = DeviceIdentity::from_seed(&HOST_SEED);
    let (init, m1) = Initiator::start_inner(
        &client,
        &host.public_key(),
        BINDING,
        Some(&CLIENT_EPHEMERAL),
    )
    .unwrap();
    let (host_ch, m2) = respond_inner(&host, BINDING, &m1, Some(&HOST_EPHEMERAL)).unwrap();
    let client_ch = init.finish(&m2).unwrap();
    (m1, m2, client_ch, host_ch)
}

#[test]
fn noise_ik_vectors() {
    let (m1, m2, mut c, mut h) = noise();
    assert_eq!(to_hex(&m1), NOISE_MSG1);
    assert_eq!(to_hex(&m2), NOISE_MSG2);
    assert_eq!(to_hex(c.handshake_hash()), NOISE_HASH);
    let sealed = c.seal(b"hello").unwrap();
    assert_eq!(to_hex(&sealed), NOISE_FIRST_SEALED);
    assert_eq!(h.open(&sealed).unwrap(), b"hello");
    // Sizes: 32 (e) + 48 (s) + 48 (32-byte payload + tag); 32 (e) + 16 (tag).
    assert_eq!((m1.len(), m2.len()), (128, 48));
}

#[test]
fn login_vectors() {
    let (_, _, c, _) = noise();
    let hash = *c.handshake_hash();
    let mut client_rng = TestRng::new("golden client");
    let mut host_rng = TestRng::new("golden host");
    let (client, start) = ClientLogin::start_with("k7mq4z2p", &hash, |pw, a, b| {
        Spake2::<Ed25519Group>::start_a_with_rng(pw, a, b, &mut client_rng)
    });
    let secrets = HostSecrets {
        temporary: Some("k7mq4z2p"),
        permanent: Some("Correct Horse"),
    };
    let (host, finish) = HostLogin::start_with(&secrets, &hash, &start, |pw, a, b| {
        Spake2::<Ed25519Group>::start_b_with_rng(pw, a, b, &mut host_rng)
    })
    .unwrap();
    let (confirmed, macs) = client.respond(&finish).unwrap();
    let (kind, host_mac) = host.verify(&macs).unwrap();
    assert_eq!(kind, crate::PasswordKind::Temporary);
    assert_eq!(confirmed.verify_host(&host_mac).unwrap(), kind);

    assert_eq!(to_hex(&start[0]), LOGIN_START_0);
    assert_eq!(to_hex(&finish[0]), LOGIN_FINISH_0);
    assert_eq!(to_hex(&macs[0]), LOGIN_CLIENT_MAC_0);
    assert_eq!(to_hex(&host_mac), LOGIN_HOST_MAC);
}

#[test]
fn confirmation_mac_known_answer() {
    let mac = confirmation_mac(&[9u8; 32], b"client", 1, &[8u8; 32], b"A", b"B");
    assert_eq!(to_hex(&mac), MAC_KAT);
}

#[test]
fn temporary_password_known_answer() {
    let mut xof = blake3::Hasher::new()
        .update(b"golden password")
        .finalize_xof();
    let policy = TemporaryPolicy::new(10, Charset::Alphanumeric).unwrap();
    let pw = generate_with(&policy, |buf| {
        xof.fill(buf);
        Ok(())
    })
    .unwrap();
    assert_eq!(&*pw, TEMPORARY_PASSWORD);
}
