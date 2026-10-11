//! TLS identity for the spike: a self-signed certificate, pinned by fingerprint.
//!
//! The server generates a fresh certificate per run and prints its
//! [`Fingerprint`]. The client accepts the server only if the certificate
//! matches the pinned fingerprint. [`ServerTrust::InsecureAcceptAny`] skips
//! the check and exists only for local experiments.
//!
//! Phase 1 replaces this with device keys signed by the ID server and a Noise
//! IK handshake (16 §5); nothing here is meant to survive that.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};

use crate::TransportError;

/// Name put in the certificate and used as the TLS server name.
pub const SERVER_NAME: &str = "kanodesk.invalid";

/// A certificate and its private key.
pub struct Identity {
    /// DER certificate.
    pub cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    /// Generates a fresh self-signed certificate.
    pub fn self_signed() -> Result<Self, TransportError> {
        let generated = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()])?;
        Ok(Identity {
            cert: generated.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()),
        })
    }

    /// Fingerprint of the certificate, for the client to pin.
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.cert)
    }

    /// Private key in the form rustls wants.
    pub(crate) fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

/// BLAKE3 hash of a DER certificate.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// Fingerprint of a DER certificate.
    pub fn of(cert_der: &[u8]) -> Self {
        Fingerprint(*blake3::hash(cert_der).as_bytes())
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl FromStr for Fingerprint {
    type Err = TransportError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.len() != 64 || !s.is_ascii() {
            return Err(TransportError::BadFingerprint(
                "expected 64 hex digits".into(),
            ));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|e| TransportError::BadFingerprint(e.to_string()))?;
        }
        Ok(Fingerprint(out))
    }
}

/// How the client decides to trust the server certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerTrust {
    /// Accept only a certificate with this fingerprint.
    Pinned(Fingerprint),
    /// Accept any certificate. Local experiments only: offers no protection
    /// against an attacker on the path.
    InsecureAcceptAny,
}

/// Verifies the server certificate against a [`ServerTrust`] and checks the
/// handshake signature with the provider's algorithms.
#[derive(Debug)]
pub(crate) struct PinVerifier {
    trust: ServerTrust,
    provider: Arc<CryptoProvider>,
}

impl PinVerifier {
    pub(crate) fn new(trust: ServerTrust, provider: Arc<CryptoProvider>) -> Self {
        PinVerifier { trust, provider }
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.trust {
            ServerTrust::Pinned(expected) if Fingerprint::of(end_entity) != expected => Err(
                rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure),
            ),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verifier(trust: ServerTrust) -> PinVerifier {
        PinVerifier::new(trust, Arc::new(rustls::crypto::ring::default_provider()))
    }

    fn check(v: &PinVerifier, cert: &CertificateDer<'_>) -> bool {
        let name = ServerName::try_from(SERVER_NAME).unwrap();
        v.verify_server_cert(cert, &[], &name, &[], UnixTime::now())
            .is_ok()
    }

    #[test]
    fn fingerprint_hex_round_trip() {
        let fp = Fingerprint::of(b"certificate bytes");
        let text = fp.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<Fingerprint>().unwrap(), fp);
        assert_eq!(text.to_uppercase().parse::<Fingerprint>().unwrap(), fp);
    }

    #[test]
    fn fingerprint_rejects_bad_text() {
        assert!("abc".parse::<Fingerprint>().is_err());
        assert!("zz".repeat(32).parse::<Fingerprint>().is_err());
    }

    #[test]
    fn pin_accepts_only_the_pinned_certificate() {
        let a = Identity::self_signed().unwrap();
        let b = Identity::self_signed().unwrap();
        let pinned = verifier(ServerTrust::Pinned(a.fingerprint()));
        assert!(check(&pinned, &a.cert));
        assert!(!check(&pinned, &b.cert));
        assert!(check(&verifier(ServerTrust::InsecureAcceptAny), &b.cert));
    }

    #[test]
    fn fresh_identities_differ() {
        let a = Identity::self_signed().unwrap();
        let b = Identity::self_signed().unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
    }
}
