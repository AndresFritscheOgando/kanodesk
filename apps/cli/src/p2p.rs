//! Network diagnostics: `p2p-host` and `p2p-connect`.
//!
//! Exercise the real connection path end to end, across networks: ID
//! server registration, NAT detection, UPnP, the connect broker, hole
//! punching, relay fallback, and the session login (Noise IK bound to TLS,
//! then SPAKE2 with the host's temporary password). `p2p-host` registers,
//! prints its ID and password and, after a successful login, echoes
//! streams; `p2p-connect ID` connects by the best path, logs in, prints
//! which path won and measures round-trip time and throughput.

use std::io::IsTerminal as _;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kanodesk_client::connect::{ConnectOptions, ConnectPlan, connect};
use kanodesk_crypto::password::{Charset, HostPasswords, TemporaryPolicy};
use kanodesk_crypto::throttle::{self, LoginThrottle, ThrottleConfig};
use kanodesk_crypto::{DeviceIdentity, DevicePublicKey, KeyStore, PasswordKind};
use kanodesk_host::incoming::{HostOptions, Prepared, keep_relay_alive, prepare};
use kanodesk_proto::hello;
use kanodesk_rendezvous::{
    DEFAULT_PORT, DeviceId, NetworkInfo, RendezvousClient, RendezvousConfig,
};
use kanodesk_transport::peer::{PeerSocket, canonical};
use kanodesk_transport::session::{
    Credentials, HostAuth, client_handshake, host_handshake, local_peer_info,
};
use kanodesk_transport::tls::{Identity, ServerTrust};
use kanodesk_transport::{nat, upnp};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// UPnP lease asked for, and how often it is renewed.
const UPNP_LEASE: Duration = Duration::from_secs(3600);
const UPNP_RENEW: Duration = Duration::from_secs(1800);

/// Login failures, backoffs and bans, kept across restarts (in the data dir).
const THROTTLE_FILE: &str = "login-throttle";

const AGENT: &str = concat!("kanodesk-cli/", env!("CARGO_PKG_VERSION"));

/// Which diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Wait for connections.
    Host,
    /// Connect to a host.
    Connect,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Host => "p2p-host",
            Role::Connect => "p2p-connect",
        }
    }
}

/// Parsed arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// `host:port` of the ID server.
    pub server: String,
    /// Pinned ID server key.
    pub server_key: DevicePublicKey,
    /// NAT-test address; default: the server's port minus one.
    pub nat_test: Option<String>,
    /// Where this device keeps its key and ID.
    pub data_dir: PathBuf,
    /// Local UDP port (0 = any).
    pub port: u16,
    /// Host to connect to (`p2p-connect`).
    pub target: Option<DeviceId>,
    /// Which paths to try (`p2p-connect`).
    pub options: ConnectOptions,
    /// Password to log in with (`p2p-connect`); asked for if absent.
    pub password: Option<String>,
    /// Temporary password length (`p2p-host`).
    pub password_length: usize,
    /// Try a UPnP port mapping (`p2p-host`).
    pub upnp: bool,
    /// Forget saved login failures, backoffs and bans (`p2p-host`).
    pub reset_login_limits: bool,
}

fn default_data_dir(role: Role) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("KanoDesk").join(role.name())
}

/// Parses `p2p-host` / `p2p-connect` arguments (after the command name).
pub fn parse(role: Role, args: &[String]) -> Result<Args, String> {
    let mut server = None;
    let mut server_key = None;
    let mut nat_test = None;
    let mut data_dir = default_data_dir(role);
    let mut port = 0;
    let mut target = None;
    let mut options = ConnectOptions::default();
    let mut password = None;
    let mut password_length = kanodesk_crypto::password::DEFAULT_TEMPORARY_LEN;
    let mut upnp = true;
    let mut reset_login_limits = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--server" => server = Some(value()?),
            "--server-key" => {
                server_key = Some(value()?.parse().map_err(|e| format!("--server-key: {e}"))?)
            }
            "--nat-test" => nat_test = Some(value()?),
            "--data-dir" => data_dir = PathBuf::from(value()?),
            "--port" => port = value()?.parse().map_err(|e| format!("--port: {e}"))?,
            "--password" => password = Some(value()?),
            "--password-length" => {
                password_length = value()?
                    .parse()
                    .map_err(|e| format!("--password-length: {e}"))?
            }
            "--no-upnp" => upnp = false,
            "--reset-login-limits" => reset_login_limits = true,
            "--no-lan" => options.try_lan = false,
            "--no-direct" => options.try_direct = false,
            "--no-relay" => options.try_relay = false,
            "--relay-only" => {
                options.try_lan = false;
                options.try_direct = false;
            }
            "--direct-timeout" => {
                options.direct_timeout = Duration::from_secs_f64(
                    value()?
                        .parse()
                        .map_err(|e| format!("--direct-timeout: {e}"))?,
                )
            }
            id if role == Role::Connect && !id.starts_with("--") && target.is_none() => {
                target = Some(id.parse().map_err(|e| format!("{id}: {e}"))?)
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let mut server = server.ok_or("--server HOST:PORT is required")?;
    if !server.contains(':') {
        server = format!("{server}:{DEFAULT_PORT}");
    }
    if role == Role::Connect && target.is_none() {
        return Err("p2p-connect needs the host's ID".into());
    }
    TemporaryPolicy::new(password_length, Charset::Alphanumeric).map_err(|e| e.to_string())?;
    Ok(Args {
        server,
        server_key: server_key
            .ok_or("--server-key KEY is required (printed by kanodesk-idserver)")?,
        nat_test,
        data_dir,
        port,
        target,
        options,
        password,
        password_length,
        upnp,
        reset_login_limits,
    })
}

async fn resolve(name: &str) -> Result<SocketAddr, BoxError> {
    let all: Vec<SocketAddr> = tokio::net::lookup_host(name).await?.collect();
    // Prefer IPv4 for the ID server; the device socket handles both.
    all.iter()
        .find(|a| a.is_ipv4())
        .or(all.first())
        .copied()
        .ok_or_else(|| format!("cannot resolve {name}").into())
}

fn key_store(dir: &Path) -> Result<Box<dyn KeyStore>, BoxError> {
    #[cfg(windows)]
    {
        use kanodesk_crypto::keystore::{DpapiKeyStore, DpapiScope};
        Ok(Box::new(DpapiKeyStore::new(dir, DpapiScope::CurrentUser)?))
    }
    #[cfg(not(windows))]
    {
        Ok(Box::new(kanodesk_crypto::keystore::FileKeyStore::new(dir)?))
    }
}

/// A registered device ready to connect or be connected to.
struct Device {
    identity: DeviceIdentity,
    socket: PeerSocket,
    rdv: RendezvousClient,
    id: DeviceId,
    nat: kanodesk_rendezvous::NatType,
    upnp: Option<upnp::UpnpMapping>,
}

async fn set_up(args: &Args, try_upnp: bool) -> Result<Device, BoxError> {
    std::fs::create_dir_all(&args.data_dir)?;
    let store = key_store(&args.data_dir)?;
    let (identity, _) = DeviceIdentity::load_or_create(store.as_ref())?;
    let id_file = args.data_dir.join("device-id");
    let saved: Option<DeviceId> = std::fs::read_to_string(&id_file)
        .ok()
        .and_then(|s| s.trim().parse().ok());

    let socket = PeerSocket::bind(
        args.port,
        &Identity::self_signed()?,
        ServerTrust::InsecureAcceptAny,
    )?;
    let server_addr = resolve(&args.server).await?;
    let config = RendezvousConfig {
        server_addr,
        server_key: args.server_key,
    };
    let rdv = RendezvousClient::connect(socket.endpoint(), config).await?;
    let nat_test = match &args.nat_test {
        Some(name) => resolve(name).await?,
        None => SocketAddr::new(server_addr.ip(), server_addr.port().saturating_sub(1)),
    };
    let nat_type = match rdv
        .detect_nat(socket.endpoint(), nat_test, socket.port())
        .await
    {
        Ok(t) => t,
        Err(e) => {
            println!("NAT detection failed ({e}); continuing as unknown");
            kanodesk_rendezvous::NatType::Unknown
        }
    };
    let mut local_addrs = nat::local_addrs(&socket);
    let mapping = if try_upnp {
        match upnp::map_udp(socket.port(), UPNP_LEASE, Duration::from_secs(3)).await {
            Ok(m) => {
                println!("UPnP: router forwards {} to this device", m.external());
                local_addrs.push(m.external());
                Some(m)
            }
            Err(e) => {
                println!("UPnP: not available ({e})");
                None
            }
        }
    } else {
        None
    };
    let network = NetworkInfo {
        local_addrs,
        nat: nat_type,
    };
    let record = rdv.register_with(&identity, saved, &network).await?;
    std::fs::write(&id_file, record.id.to_string())?;
    println!(
        "device ID {}  ({})",
        record.id.grouped(),
        args.data_dir.display()
    );
    println!(
        "device key {}; NAT {:?}; local UDP port {}; addresses {:?}",
        identity.public_key(),
        nat_type,
        socket.port(),
        network.local_addrs
    );
    println!("relays from the ID server: {:?}", rdv.relays());
    Ok(Device {
        identity,
        socket,
        rdv,
        id: record.id,
        nat: nat_type,
        upnp: mapping,
    })
}

/// State the host's tasks share.
struct HostState {
    device: Device,
    passwords: Mutex<HostPasswords>,
    throttle: Mutex<LoginThrottle<IpAddr>>,
    throttle_path: PathBuf,
    prepared: Mutex<Vec<Prepared>>,
}

impl HostState {
    /// Saves the login throttle so a restart does not reset it.
    fn save_throttle(&self) {
        let throttle = self.throttle.lock().expect("not poisoned");
        if let Err(e) = throttle::save(&self.throttle_path, &throttle) {
            println!("could not save login limits: {e}");
        }
    }
}

/// Logs in a connection, then echoes its streams.
async fn serve(state: Arc<HostState>, conn: kanodesk_transport::Connection) {
    let remote = canonical(conn.remote_address());
    println!("connection from {remote}");
    let auth = HostAuth {
        identity: &state.device.identity,
        passwords: &state.passwords,
        throttle: &state.throttle,
        peer_info: local_peer_info(AGENT, &[]),
    };
    let ours = hello::hello(&[], AGENT);
    let result =
        tokio::time::timeout(Duration::from_secs(30), host_handshake(&conn, &auth, &ours)).await;
    state.save_throttle();
    let session = match result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            println!("login from {remote} failed: {e}");
            if let kanodesk_transport::session::SessionError::Rejected {
                rotated_temporary: true,
                ..
            } = e
            {
                println!(
                    "too many failures: new password {}",
                    state.passwords.lock().expect("not poisoned").temporary()
                );
            }
            // Let the refusal reach the client; it closes the connection.
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            kanodesk_transport::endpoint::close(&conn, b"login failed");
            return;
        }
        Err(_) => {
            println!("login from {remote} timed out");
            kanodesk_transport::endpoint::close(&conn, b"login timeout");
            return;
        }
    };
    println!(
        "{remote} logged in: device {} with the {:?} password",
        session.control.remote(),
        session.password
    );
    if session.password == Some(PasswordKind::Temporary) {
        // Each temporary password admits one login.
        println!(
            "new temporary password {}",
            state.passwords.lock().expect("not poisoned").temporary()
        );
    }
    // A relayed connection can log in before `prepare` (still punching)
    // has stored its relay session, so look for it for a few seconds.
    {
        let state = state.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            for _ in 0..20 {
                let found = state
                    .prepared
                    .lock()
                    .expect("not poisoned")
                    .iter()
                    .any(|p| keep_relay_alive(&state.device.socket, p, &conn));
                if found {
                    println!(
                        "{} is relayed; keeping the relay binding fresh",
                        canonical(conn.remote_address())
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
    }
    // Keep the sealed control stream open for the session's lifetime.
    let _control = session.control;
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let data = recv.read_to_end(64 << 20).await.unwrap_or_default();
        let _ = send.write_all(&data).await;
        let _ = send.finish();
    }
    println!("{remote} closed");
}

/// `p2p-host`: registers and serves logged-in connections until Ctrl+C.
pub async fn run_host(args: Args) -> Result<(), BoxError> {
    let mut device = set_up(&args, args.upnp).await?;
    // Owned here, not in the shared state, so it can be removed on exit.
    let mapping = device.upnp.take();
    let policy = TemporaryPolicy::new(args.password_length, Charset::Alphanumeric)?;
    let throttle_path = args.data_dir.join(THROTTLE_FILE);
    if args.reset_login_limits {
        match std::fs::remove_file(&throttle_path) {
            Ok(()) => println!("login limits reset"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let state = Arc::new(HostState {
        device,
        passwords: Mutex::new(HostPasswords::new(policy)?),
        throttle: Mutex::new(throttle::load(&throttle_path, ThrottleConfig::default())),
        throttle_path,
        prepared: Mutex::new(Vec::new()),
    });
    println!(
        "waiting for connections to {}; temporary password {}; Ctrl+C to stop",
        state.device.id.grouped(),
        state.passwords.lock().expect("not poisoned").temporary()
    );
    let ep = state.device.socket.endpoint().clone();
    let accept_state = state.clone();
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let state = accept_state.clone();
            tokio::spawn(async move {
                if let Ok(conn) = incoming.await {
                    serve(state, conn).await;
                }
            });
        }
    });
    let s = state.clone();
    let requests = tokio::spawn(async move {
        while let Some(request) = s.device.rdv.next_incoming().await {
            println!(
                "request from {} at {} (NAT {:?}, relay {:?})",
                request.controller.id.grouped(),
                request.controller_addr,
                request.controller_nat,
                request.relay
            );
            let s = s.clone();
            tokio::spawn(async move {
                let p = prepare(&s.device.socket, &request, &HostOptions::default()).await;
                println!(
                    "punched {} addresses; relay {}",
                    p.punched.len(),
                    p.relay_data
                        .map(|a| a.to_string())
                        .or(p.relay_error.clone())
                        .unwrap_or_default()
                );
                let mut prepared = s.prepared.lock().expect("not poisoned");
                prepared.push(p);
                // Keep only recent requests.
                let excess = prepared.len().saturating_sub(16);
                prepared.drain(..excess);
            });
        }
        println!("lost the ID server connection");
    });
    // Renew the UPnP lease well before it runs out.
    let renew = async {
        match &mapping {
            Some(m) => loop {
                tokio::time::sleep(UPNP_RENEW).await;
                if let Err(e) = m.renew().await {
                    println!("UPnP: renewal failed ({e})");
                }
            },
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = requests => {}
        _ = renew => {}
    }
    if let Some(mapping) = mapping {
        match mapping.remove().await {
            Ok(()) => println!("UPnP mapping removed"),
            Err(e) => println!("UPnP: could not remove the mapping ({e})"),
        }
    }
    Ok(())
}

/// Asks for the host's password: hidden input on a console, one line from
/// stdin when it is not a terminal (a pipe, a script).
fn ask_password() -> Result<String, BoxError> {
    if std::io::stdin().is_terminal() {
        return Ok(rpassword::prompt_password("password: ")?);
    }
    Ok(password_line(std::io::stdin().lock())?)
}

/// One password line from `input`, without the line ending.
fn password_line(mut input: impl std::io::BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// `p2p-connect`: connects, logs in, reports the path, measures RTT and throughput.
pub async fn run_connect(args: Args) -> Result<(), BoxError> {
    let password = match &args.password {
        Some(p) => p.clone(),
        None => ask_password()?,
    };
    let device = set_up(&args, false).await?;
    let target = args.target.expect("checked in parse");
    let info = device.rdv.request_connection(target).await?;
    println!(
        "host {} at {} (NAT {:?}); addresses offered {:?}; relay {:?}",
        target.grouped(),
        info.host_addr,
        info.host_nat,
        info.host_local_addrs,
        info.relay
    );
    let plan = ConnectPlan::from_info(&info, device.nat).await;
    let started = Instant::now();
    let est = connect(&device.socket, &plan, &args.options).await?;
    println!(
        "CONNECTED via {:?} to {} in {} ms",
        est.path,
        est.remote,
        started.elapsed().as_millis()
    );

    // The host key comes from the ID server's signed record: Noise proves
    // we reached that device, SPAKE2 proves we know its password.
    let t = Instant::now();
    let ours = hello::hello(&[], AGENT);
    let (_control, negotiated, kind) = client_handshake(
        &est.conn,
        &device.identity,
        &info.host.key,
        &ours,
        Credentials::Password(&password),
        &mut || println!("waiting for the host's user to accept"),
    )
    .await?;
    println!(
        "LOGGED IN in {} ms: host device verified, password {:?}, protocol v{}",
        t.elapsed().as_millis(),
        kind,
        negotiated.version
    );

    let mut rtts = Vec::new();
    for _ in 0..10 {
        let t = Instant::now();
        echo(&est.conn, &[0u8; 64]).await?;
        rtts.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rtts.sort_by(f64::total_cmp);
    println!(
        "round trip: min {:.1} ms, median {:.1} ms, max {:.1} ms",
        rtts[0], rtts[5], rtts[9]
    );

    let payload = vec![0x5a; 4 << 20];
    let t = Instant::now();
    let back = echo(&est.conn, &payload).await?;
    let secs = t.elapsed().as_secs_f64();
    println!(
        "echoed {} MiB in {:.2} s: {:.1} Mbit/s each way",
        back.len() >> 20,
        secs,
        payload.len() as f64 * 8.0 / secs / 1e6
    );
    est.conn.close(0u32.into(), b"done");
    device.rdv.close();
    // Let the close frames go out, but do not wait for slow peers.
    let _ =
        tokio::time::timeout(Duration::from_secs(2), device.socket.endpoint().wait_idle()).await;
    Ok(())
}

async fn echo(conn: &kanodesk_transport::Connection, data: &[u8]) -> Result<Vec<u8>, BoxError> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(data).await?;
    send.finish()?;
    Ok(recv.read_to_end(64 << 20).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_host_arguments_with_defaults() {
        let a = parse(
            Role::Host,
            &args(&["--server", "ids.example.com", "--server-key", KEY]),
        )
        .unwrap();
        assert_eq!(a.server, "ids.example.com:21116");
        assert_eq!(a.port, 0);
        assert!(a.data_dir.ends_with("p2p-host"));
        assert_eq!(a.target, None);
        assert!(a.upnp);
        assert_eq!(a.password_length, 8);
    }

    #[test]
    fn parses_connect_arguments() {
        let a = parse(
            Role::Connect,
            &args(&[
                "123 456 789",
                "--server",
                "1.2.3.4:5000",
                "--server-key",
                KEY,
                "--relay-only",
                "--port",
                "40000",
                "--password",
                "k7mq4z2p",
                "--reset-login-limits",
            ]),
        )
        .unwrap();
        assert_eq!(a.target, Some("123456789".parse().unwrap()));
        assert!(!a.options.try_lan && !a.options.try_direct && a.options.try_relay);
        assert_eq!(a.port, 40000);
        assert_eq!(a.password.as_deref(), Some("k7mq4z2p"));
        assert!(a.reset_login_limits);
    }

    #[test]
    fn piped_password_loses_only_the_line_ending() {
        assert_eq!(password_line(&b"k7mq4z2p\r\n"[..]).unwrap(), "k7mq4z2p");
        assert_eq!(
            password_line(&b"with space\nnext"[..]).unwrap(),
            "with space"
        );
    }

    #[test]
    fn reports_missing_or_bad_arguments() {
        assert!(parse(Role::Host, &args(&["--server-key", KEY])).is_err());
        assert!(parse(Role::Host, &args(&["--server", "x"])).is_err());
        assert!(
            parse(
                Role::Connect,
                &args(&["--server", "x", "--server-key", KEY])
            )
            .is_err()
        );
        assert!(
            parse(
                Role::Connect,
                &args(&["12", "--server", "x", "--server-key", KEY])
            )
            .is_err()
        );
        assert!(
            parse(
                Role::Host,
                &args(&["--server", "x", "--server-key", "nothex"])
            )
            .is_err()
        );
        assert!(
            parse(
                Role::Host,
                &args(&["--server", "x", "--server-key", KEY, "--bogus"])
            )
            .is_err()
        );
        assert!(
            parse(
                Role::Host,
                &args(&[
                    "--server",
                    "x",
                    "--server-key",
                    KEY,
                    "--password-length",
                    "3"
                ])
            )
            .is_err()
        );
    }
}
