//! Session security on the control stream (16 §5): Noise IK bound to the
//! TLS session, then every control message sealed, then `Hello` and SPAKE2
//! login (see `control.proto` for the order).
//!
//! [`client_handshake`] / [`host_handshake`] run it on a QUIC connection;
//! [`client_session`] / [`host_session`] are generic over the byte streams
//! so the whole flow is testable in memory.

use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kanodesk_crypto::login::HostSecrets;
use kanodesk_crypto::password::HostPasswords;
use kanodesk_crypto::throttle::{LoginThrottle, Throttled};
use kanodesk_crypto::{
    ClientLogin, CryptoError, DeviceIdentity, DevicePublicKey, HostLogin, Initiator, PasswordKind,
    SecureChannel, noise,
};
use kanodesk_proto::hello::{self, Negotiated, NegotiationError};
use kanodesk_proto::v1::{
    self, CloseReason, Codec, ControlMessage, Hello, LoginError, control_message::Msg, login_result,
};
use kanodesk_proto::{CodecError, MessageReader, MessageWriter};
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};

/// Why a session could not be established or ended.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Stream or framing failure.
    #[error("{0}")]
    Codec(#[from] CodecError),
    /// Handshake, decryption or login cryptography failed.
    #[error("{0}")]
    Crypto(#[from] CryptoError),
    /// No common protocol version.
    #[error("{0}")]
    Negotiation(#[from] NegotiationError),
    /// The peer sent something out of order or malformed.
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// The stream ended.
    #[error("peer closed the control stream")]
    Closed,
    /// Client side: the host ended the connection instead of answering the
    /// Noise handshake. With Noise IK this is what a wrong host key looks
    /// like (the host cannot decrypt the first message).
    #[error("the host refused the secure handshake; check the host's device key")]
    HandshakeRefused,
    /// The host refused the login.
    #[error("login refused: {error:?}{}", retry_after.map(|d| format!(" (retry in {} s)", d.as_secs_f64().ceil())).unwrap_or_default())]
    Rejected {
        /// Reason sent by the host.
        error: LoginError,
        /// For rate limiting.
        retry_after: Option<Duration>,
        /// Host side only: this failure rotated the temporary password.
        rotated_temporary: bool,
    },
    /// The peer sent `Close`.
    #[error("peer closed the session: {reason:?} {detail}")]
    PeerClosed {
        /// Reason code.
        reason: CloseReason,
        /// Free text.
        detail: String,
    },
}

/// The control stream after the Noise handshake: every message is sealed.
pub struct SecureControl<R, W> {
    reader: MessageReader<R, ControlMessage>,
    writer: MessageWriter<W, ControlMessage>,
    channel: SecureChannel,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> SecureControl<R, W> {
    /// The peer's authenticated device key.
    pub fn remote(&self) -> &DevicePublicKey {
        self.channel.remote()
    }

    /// Seals and sends one message.
    pub async fn send(&mut self, msg: &ControlMessage) -> Result<(), SessionError> {
        let ciphertext = self.channel.seal(&msg.encode_to_vec())?;
        self.writer.send(&v1::Sealed { ciphertext }.into()).await?;
        Ok(())
    }

    /// Receives and opens one message; `None` when the stream ends. Cancel-safe.
    pub async fn recv(&mut self) -> Result<Option<ControlMessage>, SessionError> {
        let Some(outer) = self.reader.next().await? else {
            return Ok(None);
        };
        let Some(Msg::Sealed(sealed)) = outer.msg else {
            return Err(SessionError::Protocol("expected a sealed message".into()));
        };
        let inner = ControlMessage::decode(self.channel.open(&sealed.ciphertext)?.as_slice())
            .map_err(CodecError::from)?;
        if matches!(inner.msg, Some(Msg::Sealed(_) | Msg::NoiseHandshake(_))) {
            return Err(SessionError::Protocol("nested framing message".into()));
        }
        Ok(Some(inner))
    }

    /// Receives the next message, failing if the stream ends.
    async fn expect(&mut self) -> Result<Msg, SessionError> {
        match self.recv().await?.and_then(|m| m.msg) {
            Some(Msg::Close(c)) => Err(SessionError::PeerClosed {
                reason: c.reason(),
                detail: c.detail,
            }),
            Some(msg) => Ok(msg),
            None => Err(SessionError::Closed),
        }
    }

    /// Unwraps the parts (for the caller's own message loop).
    pub fn into_parts(
        self,
    ) -> (
        MessageReader<R, ControlMessage>,
        MessageWriter<W, ControlMessage>,
        SecureChannel,
    ) {
        (self.reader, self.writer, self.channel)
    }
}

fn unexpected(what: &str, got: &Msg) -> SessionError {
    SessionError::Protocol(format!("expected {what}, got {got:?}"))
}

/// Exchanges `Hello` (client sends first).
async fn exchange_hello<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    control: &mut SecureControl<R, W>,
    ours: &Hello,
    client: bool,
) -> Result<Negotiated, SessionError> {
    if client {
        control.send(&ours.clone().into()).await?;
    }
    let theirs = match control.expect().await? {
        Msg::Hello(h) => h,
        other => return Err(unexpected("Hello", &other)),
    };
    if !client {
        control.send(&ours.clone().into()).await?;
    }
    match hello::negotiate(ours, &theirs) {
        Ok(n) => Ok(n),
        Err(e) => {
            if !client {
                let close = v1::Close {
                    reason: CloseReason::IncompatibleVersion as i32,
                    detail: e.to_string(),
                };
                let _ = control.send(&close.into()).await;
            }
            Err(e.into())
        }
    }
}

/// Client side: Noise handshake to the host with device key `host_key`,
/// `Hello`, then SPAKE2 login with `password`.
#[allow(clippy::too_many_arguments)]
pub async fn client_session<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: MessageReader<R, ControlMessage>,
    mut writer: MessageWriter<W, ControlMessage>,
    identity: &DeviceIdentity,
    host_key: &DevicePublicKey,
    channel_binding: &[u8],
    ours: &Hello,
    password: &str,
) -> Result<(SecureControl<R, W>, Negotiated, PasswordKind), SessionError> {
    let (initiator, msg1) = Initiator::start(identity, host_key, channel_binding)?;
    writer
        .send(&v1::NoiseHandshake { message: msg1 }.into())
        .await?;
    let msg2 = match reader.next().await?.and_then(|m| m.msg) {
        Some(Msg::NoiseHandshake(h)) => h.message,
        Some(other) => return Err(unexpected("NoiseHandshake", &other)),
        None => return Err(SessionError::HandshakeRefused),
    };
    let channel = initiator.finish(&msg2)?;
    let mut control = SecureControl {
        reader,
        writer,
        channel,
    };

    let negotiated = exchange_hello(&mut control, ours, true).await?;

    let hash = *control.channel.handshake_hash();
    let (login, start) = ClientLogin::start(password, &hash);
    send_or_rejection(
        &mut control,
        &v1::AuthStart {
            spake2_messages: start,
        }
        .into(),
    )
    .await?;
    let finish = match control.expect().await? {
        Msg::AuthFinish(f) => f.spake2_messages,
        Msg::LoginResult(r) => return Err(rejection(&r)),
        other => return Err(unexpected("AuthFinish", &other)),
    };
    let (confirmed, macs) = login.respond(&finish)?;
    send_or_rejection(
        &mut control,
        &v1::AuthConfirm {
            confirmations: macs,
        }
        .into(),
    )
    .await?;
    let result = match control.expect().await? {
        Msg::LoginResult(r) => r,
        other => return Err(unexpected("LoginResult", &other)),
    };
    match result.result {
        Some(login_result::Result::PeerInfo(_)) => {
            let kind = confirmed.verify_host(&result.confirmation)?;
            Ok((control, negotiated, kind))
        }
        _ => Err(rejection(&result)),
    }
}

/// Sends a client message; if the host already hung up (it refuses and
/// closes without waiting), reports its buffered `LoginResult` instead of the
/// broken pipe.
async fn send_or_rejection<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    control: &mut SecureControl<R, W>,
    msg: &ControlMessage,
) -> Result<(), SessionError> {
    match control.send(msg).await {
        Err(e @ SessionError::Codec(_)) => match control.expect().await {
            Ok(Msg::LoginResult(r)) => Err(rejection(&r)),
            _ => Err(e),
        },
        other => other,
    }
}

fn rejection(result: &v1::LoginResult) -> SessionError {
    let error = match result.result {
        Some(login_result::Result::Error(e)) => {
            LoginError::try_from(e).unwrap_or(LoginError::Unspecified)
        }
        _ => LoginError::Unspecified,
    };
    SessionError::Rejected {
        error,
        retry_after: (result.retry_after_ms > 0)
            .then(|| Duration::from_millis(result.retry_after_ms as u64)),
        rotated_temporary: false,
    }
}

/// What the host needs to authenticate viewers. Locks are held only briefly,
/// never across an `await`.
pub struct HostAuth<'a> {
    /// This host's device identity.
    pub identity: &'a DeviceIdentity,
    /// Current passwords.
    pub passwords: &'a Mutex<HostPasswords>,
    /// Failure counters, keyed by peer IP.
    pub throttle: &'a Mutex<LoginThrottle<IpAddr>>,
    /// What the host reports in `LoginResult.peer_info`.
    pub peer_info: v1::PeerInfo,
}

/// A viewer that passed the handshake and the password check.
pub struct HostSession<R, W> {
    /// The sealed control stream.
    pub control: SecureControl<R, W>,
    /// Agreed protocol version and capabilities.
    pub negotiated: Negotiated,
    /// Which password the viewer used. After a temporary-password login,
    /// the temporary password has already been replaced.
    pub password: PasswordKind,
}

/// Host side: answers the Noise handshake, `Hello`, and the login. Sends the
/// `LoginResult` either way. Only a successful return may lead to video.
pub async fn host_session<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: MessageReader<R, ControlMessage>,
    mut writer: MessageWriter<W, ControlMessage>,
    auth: &HostAuth<'_>,
    channel_binding: &[u8],
    ours: &Hello,
    peer: IpAddr,
) -> Result<HostSession<R, W>, SessionError> {
    let msg1 = match reader.next().await?.and_then(|m| m.msg) {
        Some(Msg::NoiseHandshake(h)) => h.message,
        Some(other) => return Err(unexpected("NoiseHandshake", &other)),
        None => return Err(SessionError::Closed),
    };
    let (channel, msg2) = noise::respond(auth.identity, channel_binding, &msg1)?;
    writer
        .send(&v1::NoiseHandshake { message: msg2 }.into())
        .await?;
    let mut control = SecureControl {
        reader,
        writer,
        channel,
    };

    let negotiated = exchange_hello(&mut control, ours, false).await?;

    // Throttled peers get no SPAKE2 exchange at all.
    let throttled = auth
        .throttle
        .lock()
        .expect("not poisoned")
        .check(&peer, Instant::now());
    if let Err(t) = throttled {
        let (error, retry_after) = match t {
            Throttled::RetryAfter(d) => (LoginError::RateLimited, Some(d)),
            Throttled::Banned => (LoginError::Denied, None),
        };
        let result = v1::LoginResult {
            result: Some(login_result::Result::Error(error as i32)),
            retry_after_ms: retry_after.map_or(0, |d| d.as_millis().min(u32::MAX as u128) as u32),
            confirmation: Vec::new(),
        };
        control.send(&result.into()).await?;
        return Err(SessionError::Rejected {
            error,
            retry_after,
            rotated_temporary: false,
        });
    }

    let start = match control.expect().await? {
        Msg::AuthStart(s) => s.spake2_messages,
        other => return Err(unexpected("AuthStart", &other)),
    };
    let hash = *control.channel.handshake_hash();
    let (login, finish, generation) = {
        let passwords = auth.passwords.lock().expect("not poisoned");
        let secrets: HostSecrets<'_> = passwords.secrets();
        let (login, finish) = HostLogin::start(&secrets, &hash, &start)?;
        (login, finish, passwords.generation())
    };
    control
        .send(
            &v1::AuthFinish {
                spake2_messages: finish,
            }
            .into(),
        )
        .await?;
    let macs = match control.expect().await? {
        Msg::AuthConfirm(c) => c.confirmations,
        other => return Err(unexpected("AuthConfirm", &other)),
    };

    match login.verify(&macs) {
        Ok((kind, host_mac)) => {
            // A temporary password admits one login: rotate it now. If it
            // changed since this login started (another login used it, or
            // failures rotated it), the password is no longer valid.
            if kind == PasswordKind::Temporary {
                let still_valid = {
                    let mut passwords = auth.passwords.lock().expect("not poisoned");
                    let valid = passwords.generation() == generation;
                    if valid {
                        passwords.rotate_temporary()?;
                    }
                    valid
                };
                if !still_valid {
                    let result = v1::LoginResult {
                        result: Some(login_result::Result::Error(LoginError::PasswordUsed as i32)),
                        retry_after_ms: 0,
                        confirmation: Vec::new(),
                    };
                    control.send(&result.into()).await?;
                    return Err(SessionError::Rejected {
                        error: LoginError::PasswordUsed,
                        retry_after: None,
                        rotated_temporary: false,
                    });
                }
            }
            auth.throttle
                .lock()
                .expect("not poisoned")
                .record_success(&peer);
            let result = v1::LoginResult {
                result: Some(login_result::Result::PeerInfo(auth.peer_info.clone())),
                retry_after_ms: 0,
                confirmation: host_mac,
            };
            control.send(&result.into()).await?;
            Ok(HostSession {
                control,
                negotiated,
                password: kind,
            })
        }
        Err(_) => {
            let outcome = auth
                .throttle
                .lock()
                .expect("not poisoned")
                .record_failure(&peer, Instant::now());
            if outcome.rotate_temporary {
                auth.passwords
                    .lock()
                    .expect("not poisoned")
                    .rotate_temporary()?;
            }
            let result = v1::LoginResult {
                result: Some(login_result::Result::Error(
                    LoginError::WrongPassword as i32,
                )),
                retry_after_ms: 0,
                confirmation: Vec::new(),
            };
            control.send(&result.into()).await?;
            Err(SessionError::Rejected {
                error: LoginError::WrongPassword,
                retry_after: None,
                rotated_temporary: outcome.rotate_temporary,
            })
        }
    }
}

/// A basic `PeerInfo` for this machine with the given agent string.
pub fn local_peer_info(agent: &str, codecs: &[Codec]) -> v1::PeerInfo {
    v1::PeerInfo {
        hostname: std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_default(),
        username: std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_default(),
        platform: std::env::consts::OS.to_owned(),
        agent: agent.to_owned(),
        displays: None,
        codecs: codecs.iter().map(|c| *c as i32).collect(),
    }
}

/// Client side on a QUIC connection: opens the control stream and runs
/// [`client_session`] with this connection's TLS channel binding.
/// `host_key` is the host's device key, from the ID server's signed record
/// or pinned by the user.
pub async fn client_handshake(
    conn: &quinn::Connection,
    identity: &DeviceIdentity,
    host_key: &DevicePublicKey,
    ours: &Hello,
    password: &str,
) -> Result<
    (
        SecureControl<quinn::RecvStream, quinn::SendStream>,
        Negotiated,
        PasswordKind,
    ),
    SessionError,
> {
    let binding = crate::endpoint::channel_binding(conn)
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    let (writer, reader) = crate::streams::open_control(conn)
        .await
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    client_session(reader, writer, identity, host_key, &binding, ours, password).await
}

/// Host side on a QUIC connection: accepts the control stream and runs
/// [`host_session`]. Throttling is keyed by the peer's IP (canonical form).
pub async fn host_handshake(
    conn: &quinn::Connection,
    auth: &HostAuth<'_>,
    ours: &Hello,
) -> Result<HostSession<quinn::RecvStream, quinn::SendStream>, SessionError> {
    let binding = crate::endpoint::channel_binding(conn)
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    let (writer, reader) = crate::streams::accept_control(conn)
        .await
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    let peer = crate::peer::canonical(conn.remote_address()).ip();
    host_session(reader, writer, auth, &binding, ours, peer).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanodesk_crypto::password::TemporaryPolicy;
    use kanodesk_crypto::throttle::ThrottleConfig;
    use kanodesk_proto::hello::hello as make_hello;
    use std::net::Ipv4Addr;
    use tokio::io::{DuplexStream, duplex};

    type Ends = (
        (
            MessageReader<DuplexStream, ControlMessage>,
            MessageWriter<DuplexStream, ControlMessage>,
        ),
        (
            MessageReader<DuplexStream, ControlMessage>,
            MessageWriter<DuplexStream, ControlMessage>,
        ),
    );

    fn pipes() -> Ends {
        let (c_to_h_w, c_to_h_r) = duplex(64 * 1024);
        let (h_to_c_w, h_to_c_r) = duplex(64 * 1024);
        (
            (MessageReader::new(h_to_c_r), MessageWriter::new(c_to_h_w)),
            (MessageReader::new(c_to_h_r), MessageWriter::new(h_to_c_w)),
        )
    }

    struct Host {
        identity: DeviceIdentity,
        passwords: Mutex<HostPasswords>,
        throttle: Mutex<LoginThrottle<IpAddr>>,
    }

    impl Host {
        fn new(config: ThrottleConfig) -> Self {
            Host {
                identity: DeviceIdentity::generate().unwrap(),
                passwords: Mutex::new(HostPasswords::new(TemporaryPolicy::default()).unwrap()),
                throttle: Mutex::new(LoginThrottle::new(config)),
            }
        }

        fn temporary(&self) -> String {
            self.passwords.lock().unwrap().temporary().to_owned()
        }
    }

    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));

    /// Runs client and host concurrently over in-memory pipes.
    async fn run(
        host: &Host,
        host_key: DevicePublicKey,
        password: &str,
        client_binding: &[u8],
    ) -> (
        Result<
            (
                SecureControl<DuplexStream, DuplexStream>,
                Negotiated,
                PasswordKind,
            ),
            SessionError,
        >,
        Result<HostSession<DuplexStream, DuplexStream>, SessionError>,
    ) {
        let ((cr, cw), (hr, hw)) = pipes();
        let client_id = DeviceIdentity::generate().unwrap();
        let hello = make_hello(&[v1::Capability::VideoVp9], "test");
        let auth = HostAuth {
            identity: &host.identity,
            passwords: &host.passwords,
            throttle: &host.throttle,
            peer_info: local_peer_info("test", &[Codec::Vp9]),
        };
        tokio::join!(
            client_session(
                cr,
                cw,
                &client_id,
                &host_key,
                client_binding,
                &hello,
                password
            ),
            host_session(hr, hw, &auth, b"binding", &hello, PEER),
        )
    }

    #[tokio::test]
    async fn correct_password_opens_a_sealed_session() {
        let host = Host::new(ThrottleConfig::default());
        let key = host.identity.public_key();
        let (client, server) = run(&host, key, &host.temporary(), b"binding").await;
        let (mut client, negotiated, kind) = client.unwrap();
        let mut server = server.unwrap();
        assert_eq!(kind, PasswordKind::Temporary);
        assert_eq!(server.password, PasswordKind::Temporary);
        assert!(negotiated.has(v1::Capability::VideoVp9));
        assert_eq!(client.remote(), &key);

        // The control channel keeps working, sealed, both ways.
        client
            .send(&v1::KeyframeRequest { display: 0 }.into())
            .await
            .unwrap();
        let got = server.control.recv().await.unwrap().unwrap();
        assert_eq!(got, v1::KeyframeRequest { display: 0 }.into());
    }

    #[tokio::test]
    async fn a_temporary_password_admits_one_login() {
        let host = Host::new(ThrottleConfig::default());
        let key = host.identity.public_key();
        let first = host.temporary();
        let (client, _) = run(&host, key, &first, b"binding").await;
        assert!(client.is_ok());
        let second = host.temporary();
        assert_ne!(first, second, "rotated at login");
        let (client, _) = run(&host, key, &first, b"binding").await;
        assert!(matches!(
            client,
            Err(SessionError::Rejected {
                error: LoginError::WrongPassword,
                ..
            })
        ));
        let (client, _) = run(&host, key, &second, b"binding").await;
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn concurrent_logins_with_one_temporary_password_let_only_one_in() {
        let host = Host::new(ThrottleConfig::default());
        let key = host.identity.public_key();
        let password = host.temporary();
        let (a, b) = tokio::join!(
            run(&host, key, &password, b"binding"),
            run(&host, key, &password, b"binding"),
        );
        let results = [a.0, b.0];
        let ok = results.iter().filter(|r| r.is_ok()).count();
        let used = results
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    Err(SessionError::Rejected {
                        error: LoginError::PasswordUsed,
                        ..
                    })
                )
            })
            .count();
        assert_eq!((ok, used), (1, 1));
    }

    #[tokio::test]
    async fn the_permanent_password_is_reusable() {
        let host = Host::new(ThrottleConfig::default());
        let store = kanodesk_crypto::MemoryKeyStore::default();
        host.passwords
            .lock()
            .unwrap()
            .set_permanent(Some("correct horse"), &store)
            .unwrap();
        let key = host.identity.public_key();
        let temporary = host.temporary();
        for _ in 0..2 {
            let (client, _) = run(&host, key, "correct horse", b"binding").await;
            assert_eq!(client.unwrap().2, PasswordKind::Permanent);
        }
        assert_eq!(host.temporary(), temporary, "temporary password untouched");
    }

    #[tokio::test]
    async fn wrong_password_is_refused_and_counted() {
        let host = Host::new(ThrottleConfig::default());
        let (client, server) =
            run(&host, host.identity.public_key(), "wrong-guess", b"binding").await;
        assert!(matches!(
            client,
            Err(SessionError::Rejected {
                error: LoginError::WrongPassword,
                ..
            })
        ));
        assert!(matches!(server, Err(SessionError::Rejected { .. })));
    }

    #[tokio::test]
    async fn repeated_failures_are_rate_limited() {
        let host = Host::new(ThrottleConfig::default());
        for _ in 0..3 {
            let (client, _) = run(&host, host.identity.public_key(), "nope", b"binding").await;
            assert!(matches!(
                client,
                Err(SessionError::Rejected {
                    error: LoginError::WrongPassword,
                    ..
                })
            ));
        }
        // Fourth attempt, immediately: refused before any password check,
        // even with the right password.
        let right = host.temporary();
        let (client, _) = run(&host, host.identity.public_key(), &right, b"binding").await;
        match client {
            Err(SessionError::Rejected {
                error: LoginError::RateLimited,
                retry_after: Some(d),
                ..
            }) => assert!(d > Duration::ZERO),
            other => panic!("expected rate limiting, got {:?}", other.err()),
        }
    }

    #[tokio::test]
    async fn consecutive_failures_rotate_the_temporary_password() {
        let config = ThrottleConfig {
            rotate_after: 2,
            free_attempts: 100,
            ..ThrottleConfig::default()
        };
        let host = Host::new(config);
        let before = host.temporary();
        let (_, first) = run(&host, host.identity.public_key(), "x", b"binding").await;
        assert!(matches!(
            first,
            Err(SessionError::Rejected {
                rotated_temporary: false,
                ..
            })
        ));
        let (_, second) = run(&host, host.identity.public_key(), "y", b"binding").await;
        assert!(matches!(
            second,
            Err(SessionError::Rejected {
                rotated_temporary: true,
                ..
            })
        ));
        assert_ne!(host.temporary(), before);
        // The old password no longer works.
        let (client, _) = run(&host, host.identity.public_key(), &before, b"binding").await;
        assert!(client.is_err());
    }

    #[tokio::test]
    async fn wrong_host_key_or_binding_fails_before_login() {
        let host = Host::new(ThrottleConfig::default());
        let other = DeviceIdentity::generate().unwrap().public_key();
        let (client, server) = run(&host, other, &host.temporary(), b"binding").await;
        assert!(matches!(server, Err(SessionError::Crypto(_))));
        assert!(
            matches!(client, Err(SessionError::HandshakeRefused)),
            "{:?}",
            client.err()
        );

        // Channel-binding mismatch (a MITM terminating TLS twice).
        let (client, server) = run(
            &host,
            host.identity.public_key(),
            &host.temporary(),
            b"other tls session",
        )
        .await;
        assert!(matches!(server, Err(SessionError::Crypto(_))));
        assert!(client.is_err());
        // Neither counts as a password failure.
        assert!(
            host.throttle
                .lock()
                .unwrap()
                .check(&PEER, Instant::now())
                .is_ok()
        );
    }
}
