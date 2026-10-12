//! Session security on the control stream (16 §5): Noise IK bound to the
//! TLS session, then every control message sealed, then `Hello` and SPAKE2
//! login (see `control.proto` for the order).
//!
//! Logins are by password (SPAKE2), by the host's user clicking "accept"
//! (the viewer sends no password; the host says it is waiting with
//! `LoginPending`), or both. [`host_authenticate`] stops before the decision
//! and returns a [`PendingLogin`] for the caller to accept or reject.
//!
//! [`client_handshake`] / [`host_login`] / [`host_handshake`] run it on a QUIC
//! connection; [`client_session`] / [`host_authenticate`] / [`host_session`]
//! are generic over the byte streams so the whole flow is testable in memory.

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
    /// Client side, after a successful login: what the host said about itself.
    peer_info: Option<v1::PeerInfo>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> SecureControl<R, W> {
    /// The peer's authenticated device key.
    pub fn remote(&self) -> &DevicePublicKey {
        self.channel.remote()
    }

    /// Client side, after a successful login: the host's self-description
    /// (host name, user, platform, agent) from its `LoginResult`.
    pub fn peer_info(&self) -> Option<&v1::PeerInfo> {
        self.peer_info.as_ref()
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

/// A client login: the sealed control stream, the agreed protocol, and the
/// password slot that matched (`None`: the host's user accepted it).
pub type ClientLoginResult<R, W> =
    Result<(SecureControl<R, W>, Negotiated, Option<PasswordKind>), SessionError>;

/// How a viewer logs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credentials<'a> {
    /// With the host's temporary or permanent password (SPAKE2).
    Password(&'a str),
    /// Without a password: the host's user accepts or denies (click approval).
    AskHost,
}

/// Client side: Noise handshake to the host with device key `host_key`,
/// `Hello`, then the login with `credentials`. `on_pending` runs whenever
/// the host says the login waits for its user's approval. Returns the
/// password slot that matched, or `None` if the host's user accepted the
/// login without a password.
#[allow(clippy::too_many_arguments)]
pub async fn client_session<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: MessageReader<R, ControlMessage>,
    mut writer: MessageWriter<W, ControlMessage>,
    identity: &DeviceIdentity,
    host_key: &DevicePublicKey,
    channel_binding: &[u8],
    ours: &Hello,
    credentials: Credentials<'_>,
    on_pending: &mut (dyn FnMut() + Send),
) -> ClientLoginResult<R, W> {
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
        peer_info: None,
    };

    let negotiated = exchange_hello(&mut control, ours, true).await?;

    let hash = *control.channel.handshake_hash();
    let (login, start) = match credentials {
        Credentials::Password(password) => {
            let (login, start) = ClientLogin::start(password, &hash);
            (Some(login), start)
        }
        Credentials::AskHost => (None, Vec::new()),
    };
    send_or_rejection(
        &mut control,
        &v1::AuthStart {
            spake2_messages: start,
            client: Some(local_peer_info(&ours.agent, &[])),
        }
        .into(),
    )
    .await?;

    let mut confirmed = None;
    if let Some(login) = login {
        // A click-only host answers with the result directly (the password
        // is not used); otherwise the SPAKE2 exchange continues.
        let finish = loop {
            match control.expect().await? {
                Msg::AuthFinish(f) => break Some(f.spake2_messages),
                Msg::LoginPending(_) => on_pending(),
                Msg::LoginResult(r) => return finish_login(control, negotiated, &r, None),
                other => return Err(unexpected("AuthFinish", &other)),
            }
        };
        if let Some(finish) = finish {
            let (c, macs) = login.respond(&finish)?;
            send_or_rejection(
                &mut control,
                &v1::AuthConfirm {
                    confirmations: macs,
                }
                .into(),
            )
            .await?;
            confirmed = Some(c);
        }
    }
    let result = loop {
        match control.expect().await? {
            Msg::LoginPending(_) => on_pending(),
            Msg::LoginResult(r) => break r,
            other => return Err(unexpected("LoginResult", &other)),
        }
    };
    finish_login(control, negotiated, &result, confirmed)
}

/// Checks the final `LoginResult`: success needs the host's key
/// confirmation when a password was used.
fn finish_login<R, W>(
    mut control: SecureControl<R, W>,
    negotiated: Negotiated,
    result: &v1::LoginResult,
    confirmed: Option<kanodesk_crypto::login::ClientConfirmed>,
) -> ClientLoginResult<R, W> {
    match result.result {
        Some(login_result::Result::PeerInfo(ref info)) => {
            let kind = match confirmed {
                Some(c) => Some(c.verify_host(&result.confirmation)?),
                None => None,
            };
            control.peer_info = Some(info.clone());
            Ok((control, negotiated, kind))
        }
        _ => Err(rejection(result)),
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

/// A logged-in viewer.
pub struct HostSession<R, W> {
    /// The sealed control stream.
    pub control: SecureControl<R, W>,
    /// Agreed protocol version and capabilities.
    pub negotiated: Negotiated,
    /// Which password the viewer used (`None`: accepted by the host's user
    /// without one). After a temporary-password login, the temporary
    /// password has already been replaced.
    pub password: Option<PasswordKind>,
}

/// A viewer that passed the handshake and, if required, the password check,
/// and now waits for the host's decision: [`PendingLogin::accept`] or
/// [`PendingLogin::reject`]. Nothing but control messages has been exchanged.
pub struct PendingLogin<R, W> {
    control: SecureControl<R, W>,
    negotiated: Negotiated,
    password: Option<PasswordKind>,
    client: v1::PeerInfo,
    confirmation: Vec<u8>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> PendingLogin<R, W> {
    /// The viewer's authenticated device key.
    pub fn remote(&self) -> &DevicePublicKey {
        self.control.remote()
    }

    /// What the viewer says about itself (host name, user, platform, agent).
    pub fn client(&self) -> &v1::PeerInfo {
        &self.client
    }

    /// The password slot that matched, if a password was checked.
    pub fn password(&self) -> Option<PasswordKind> {
        self.password
    }

    /// Agreed protocol version and capabilities.
    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    /// Tells the viewer that the host's user is being asked.
    pub async fn notify_pending(&mut self) -> Result<(), SessionError> {
        self.control.send(&v1::LoginPending {}.into()).await
    }

    /// Resolves when the viewer gives up (closes the control stream or the
    /// connection) while waiting. Cancel-safe.
    pub async fn closed(&mut self) {
        while let Ok(Some(_)) = self.control.recv().await {}
    }

    /// Lets the viewer in.
    pub async fn accept(
        mut self,
        peer_info: v1::PeerInfo,
    ) -> Result<HostSession<R, W>, SessionError> {
        let result = v1::LoginResult {
            result: Some(login_result::Result::PeerInfo(peer_info)),
            retry_after_ms: 0,
            confirmation: std::mem::take(&mut self.confirmation),
        };
        self.control.send(&result.into()).await?;
        Ok(HostSession {
            control: self.control,
            negotiated: self.negotiated,
            password: self.password,
        })
    }

    /// Turns the viewer away with `error` (for example `Denied` or `Timeout`).
    pub async fn reject(mut self, error: LoginError) -> Result<(), SessionError> {
        let result = v1::LoginResult {
            result: Some(login_result::Result::Error(error as i32)),
            retry_after_ms: 0,
            confirmation: Vec::new(),
        };
        self.control.send(&result.into()).await
    }
}

async fn refuse<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    control: &mut SecureControl<R, W>,
    error: LoginError,
    retry_after: Option<Duration>,
    rotated_temporary: bool,
) -> SessionError {
    let result = v1::LoginResult {
        result: Some(login_result::Result::Error(error as i32)),
        retry_after_ms: retry_after.map_or(0, |d| d.as_millis().min(u32::MAX as u128) as u32),
        confirmation: Vec::new(),
    };
    if let Err(e) = control.send(&result.into()).await {
        return e;
    }
    SessionError::Rejected {
        error,
        retry_after,
        rotated_temporary,
    }
}

/// Host side, first step: answers the Noise handshake and `Hello`, then the
/// login. With `require_password`, the viewer must prove a password (and a
/// viewer offering none is refused with `PasswordRequired`); without it, any
/// password is ignored and the caller decides (click approval). Refusals are
/// sent to the viewer here; a [`PendingLogin`] still needs `accept` or `reject`.
pub async fn host_authenticate<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: MessageReader<R, ControlMessage>,
    mut writer: MessageWriter<W, ControlMessage>,
    auth: &HostAuth<'_>,
    channel_binding: &[u8],
    ours: &Hello,
    peer: IpAddr,
    require_password: bool,
) -> Result<PendingLogin<R, W>, SessionError> {
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
        peer_info: None,
    };

    let negotiated = exchange_hello(&mut control, ours, false).await?;

    // Throttled peers get no login exchange (and no approval prompt) at all.
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
        return Err(refuse(&mut control, error, retry_after, false).await);
    }

    let (start, client) = match control.expect().await? {
        Msg::AuthStart(s) => (s.spake2_messages, s.client.unwrap_or_default()),
        other => return Err(unexpected("AuthStart", &other)),
    };
    if !require_password {
        return Ok(PendingLogin {
            control,
            negotiated,
            password: None,
            client,
            confirmation: Vec::new(),
        });
    }
    if start.is_empty() {
        return Err(refuse(&mut control, LoginError::PasswordRequired, None, false).await);
    }
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
                    return Err(refuse(&mut control, LoginError::PasswordUsed, None, false).await);
                }
            }
            auth.throttle
                .lock()
                .expect("not poisoned")
                .record_success(&peer);
            Ok(PendingLogin {
                control,
                negotiated,
                password: Some(kind),
                client,
                confirmation: host_mac,
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
            Err(refuse(
                &mut control,
                LoginError::WrongPassword,
                None,
                outcome.rotate_temporary,
            )
            .await)
        }
    }
}

/// Host side, password only: [`host_authenticate`] with a required password,
/// then accept. Sends the `LoginResult` either way; only a successful return
/// may lead to video.
pub async fn host_session<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: MessageReader<R, ControlMessage>,
    writer: MessageWriter<W, ControlMessage>,
    auth: &HostAuth<'_>,
    channel_binding: &[u8],
    ours: &Hello,
    peer: IpAddr,
) -> Result<HostSession<R, W>, SessionError> {
    host_authenticate(reader, writer, auth, channel_binding, ours, peer, true)
        .await?
        .accept(auth.peer_info.clone())
        .await
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
    credentials: Credentials<'_>,
    on_pending: &mut (dyn FnMut() + Send),
) -> ClientLoginResult<quinn::RecvStream, quinn::SendStream> {
    let binding = crate::endpoint::channel_binding(conn)
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    let (writer, reader) = crate::streams::open_control(conn)
        .await
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    client_session(
        reader,
        writer,
        identity,
        host_key,
        &binding,
        ours,
        credentials,
        on_pending,
    )
    .await
}

/// Host side on a QUIC connection, first step: accepts the control stream and
/// runs [`host_authenticate`]. Throttling is keyed by the peer's IP
/// (canonical form).
pub async fn host_login(
    conn: &quinn::Connection,
    auth: &HostAuth<'_>,
    ours: &Hello,
    require_password: bool,
) -> Result<PendingLogin<quinn::RecvStream, quinn::SendStream>, SessionError> {
    let binding = crate::endpoint::channel_binding(conn)
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    let (writer, reader) = crate::streams::accept_control(conn)
        .await
        .map_err(|e| SessionError::Protocol(e.to_string()))?;
    let peer = crate::peer::canonical(conn.remote_address()).ip();
    host_authenticate(reader, writer, auth, &binding, ours, peer, require_password).await
}

/// Host side on a QUIC connection, password only: [`host_login`] with a
/// required password, then accept.
pub async fn host_handshake(
    conn: &quinn::Connection,
    auth: &HostAuth<'_>,
    ours: &Hello,
) -> Result<HostSession<quinn::RecvStream, quinn::SendStream>, SessionError> {
    host_login(conn, auth, ours, true)
        .await?
        .accept(auth.peer_info.clone())
        .await
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
                Option<PasswordKind>,
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
        let mut nothing = || {};
        tokio::join!(
            client_session(
                cr,
                cw,
                &client_id,
                &host_key,
                client_binding,
                &hello,
                Credentials::Password(password),
                &mut nothing,
            ),
            host_session(hr, hw, &auth, b"binding", &hello, PEER),
        )
    }

    type ClientResult = Result<
        (
            SecureControl<DuplexStream, DuplexStream>,
            Negotiated,
            Option<PasswordKind>,
        ),
        SessionError,
    >;

    /// Runs a login that the host's "user" decides with `decide` after
    /// `host_authenticate`. Returns the client's result, how often it was
    /// told to wait, and the client info the host saw.
    async fn run_approval(
        host: &Host,
        credentials: Credentials<'_>,
        require_password: bool,
        accept: bool,
    ) -> (ClientResult, u32, Option<v1::PeerInfo>) {
        let ((cr, cw), (hr, hw)) = pipes();
        let client_id = DeviceIdentity::generate().unwrap();
        let hello = make_hello(&[v1::Capability::VideoVp9], "approval-test");
        let auth = HostAuth {
            identity: &host.identity,
            passwords: &host.passwords,
            throttle: &host.throttle,
            peer_info: local_peer_info("test", &[Codec::Vp9]),
        };
        let waits = std::sync::atomic::AtomicU32::new(0);
        let mut count_wait = || {
            waits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        };
        let key = host.identity.public_key();
        let (client, seen) = tokio::join!(
            client_session(
                cr,
                cw,
                &client_id,
                &key,
                b"binding",
                &hello,
                credentials,
                &mut count_wait,
            ),
            async {
                let Ok(mut pending) =
                    host_authenticate(hr, hw, &auth, b"binding", &hello, PEER, require_password)
                        .await
                else {
                    return None;
                };
                let info = pending.client().clone();
                pending.notify_pending().await.unwrap();
                if accept {
                    let _keep = pending.accept(auth.peer_info.clone()).await.unwrap();
                    // Keep the host side open until the client has read the result.
                    tokio::task::yield_now().await;
                    Some(info)
                } else {
                    pending.reject(LoginError::Denied).await.unwrap();
                    Some(info)
                }
            }
        );
        (
            client,
            waits.load(std::sync::atomic::Ordering::Relaxed),
            seen,
        )
    }

    #[tokio::test]
    async fn click_approval_lets_a_viewer_in_without_a_password() {
        let host = Host::new(ThrottleConfig::default());
        let (client, waits, seen) = run_approval(&host, Credentials::AskHost, false, true).await;
        let (control, _, kind) = client.unwrap();
        assert_eq!(kind, None, "no password used");
        assert_eq!(
            control.peer_info().map(|p| p.agent.as_str()),
            Some("test"),
            "the viewer learned who the host is"
        );
        assert_eq!(waits, 1, "told to wait once");
        let seen = seen.unwrap();
        assert_eq!(
            seen.agent, "approval-test",
            "the host saw who is connecting"
        );
        assert!(!seen.platform.is_empty());
        // A password offered to a click-only host is simply not used.
        let temporary = host.temporary();
        let (client, _, _) =
            run_approval(&host, Credentials::Password(&temporary), false, true).await;
        assert_eq!(client.unwrap().2, None);
        assert_eq!(host.temporary(), temporary, "temporary password untouched");
    }

    #[tokio::test]
    async fn click_denial_is_reported_to_the_viewer() {
        let host = Host::new(ThrottleConfig::default());
        let (client, waits, _) = run_approval(&host, Credentials::AskHost, false, false).await;
        assert!(matches!(
            client,
            Err(SessionError::Rejected {
                error: LoginError::Denied,
                ..
            })
        ));
        assert_eq!(waits, 1);
    }

    #[tokio::test]
    async fn password_and_click_needs_both() {
        let host = Host::new(ThrottleConfig::default());
        // Right password, then accepted.
        let temporary = host.temporary();
        let (client, waits, _) =
            run_approval(&host, Credentials::Password(&temporary), true, true).await;
        assert_eq!(client.unwrap().2, Some(PasswordKind::Temporary));
        assert_eq!(waits, 1);
        // Right password, then denied.
        let temporary = host.temporary();
        let (client, _, _) =
            run_approval(&host, Credentials::Password(&temporary), true, false).await;
        assert!(matches!(
            client,
            Err(SessionError::Rejected {
                error: LoginError::Denied,
                ..
            })
        ));
        // Wrong password: refused before anyone is asked.
        let (client, waits, seen) =
            run_approval(&host, Credentials::Password("nope-nope"), true, true).await;
        assert!(matches!(
            client,
            Err(SessionError::Rejected {
                error: LoginError::WrongPassword,
                ..
            })
        ));
        assert_eq!((waits, seen), (0, None));
        // No password at all: refused, the host requires one.
        let (client, _, seen) = run_approval(&host, Credentials::AskHost, true, true).await;
        assert!(matches!(
            client,
            Err(SessionError::Rejected {
                error: LoginError::PasswordRequired,
                ..
            })
        ));
        assert!(seen.is_none());
    }

    #[tokio::test]
    async fn correct_password_opens_a_sealed_session() {
        let host = Host::new(ThrottleConfig::default());
        let key = host.identity.public_key();
        let (client, server) = run(&host, key, &host.temporary(), b"binding").await;
        let (mut client, negotiated, kind) = client.unwrap();
        let mut server = server.unwrap();
        assert_eq!(kind, Some(PasswordKind::Temporary));
        assert_eq!(server.password, Some(PasswordKind::Temporary));
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
            assert_eq!(client.unwrap().2, Some(PasswordKind::Permanent));
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
