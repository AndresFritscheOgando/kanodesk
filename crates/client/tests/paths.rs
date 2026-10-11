//! End-to-end connection paths on loopback: ID server (with NAT-test port
//! and relay secret), relay server, and two devices with their own sockets.
//! Covers NAT detection, LAN, direct and relayed paths, and fallback order.

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kanodesk_client::connect::{ConnectError, ConnectOptions, ConnectPlan, PathKind, connect};
use kanodesk_crypto::DeviceIdentity;
use kanodesk_host::incoming::{HostOptions, Prepared, prepare};
use kanodesk_idserver::{IdServer, ServerConfig};
use kanodesk_relay::{RelayConfig, RelayServer};
use kanodesk_rendezvous::{DeviceId, NatType, NetworkInfo, RendezvousClient, RendezvousConfig};
use kanodesk_transport::peer::PeerSocket;
use kanodesk_transport::relay::RelaySecret;
use kanodesk_transport::tls::{Identity, ServerTrust};
use kanodesk_transport::{Connection, nat};
use tokio::sync::mpsc;

const SECRET: [u8; 32] = [9u8; 32];

struct Net {
    idserver: IdServer,
    relay: RelayServer,
    config: RendezvousConfig,
    nat_test: SocketAddr,
    dir: PathBuf,
}

async fn net() -> Net {
    let relay = kanodesk_relay::start(RelayConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Some(RelaySecret(SECRET)),
    ))
    .await
    .unwrap();
    let mut name = [0u8; 8];
    getrandom::fill(&mut name).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "kanodesk-paths-{}",
        name.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), dir.join("idserver.sqlite"));
    config.relays = vec![relay.local_addr().unwrap().to_string()];
    config.relay_secret = Some(RelaySecret(SECRET));
    config.nat_test_listen = Some("127.0.0.1:0".parse().unwrap());
    let idserver = kanodesk_idserver::start(config, DeviceIdentity::generate().unwrap())
        .await
        .unwrap();
    let config = RendezvousConfig {
        server_addr: idserver.local_addr().unwrap(),
        server_key: idserver.public_key(),
    };
    Net {
        nat_test: idserver.nat_test_addr().unwrap(),
        idserver,
        relay,
        config,
        dir,
    }
}

struct Device {
    socket: PeerSocket,
    rdv: RendezvousClient,
    id: DeviceId,
    nat: NatType,
}

async fn device(net: &Net) -> Arc<Device> {
    let socket = PeerSocket::bind(
        0,
        &Identity::self_signed().unwrap(),
        ServerTrust::InsecureAcceptAny,
    )
    .unwrap();
    let rdv = RendezvousClient::connect(socket.endpoint(), net.config)
        .await
        .unwrap();
    let nat = rdv
        .detect_nat(socket.endpoint(), net.nat_test, socket.port())
        .await
        .unwrap();
    let identity = DeviceIdentity::generate().unwrap();
    let network = NetworkInfo {
        local_addrs: nat::local_addrs(&socket),
        nat,
    };
    let id = rdv
        .register_with(&identity, None, &network)
        .await
        .unwrap()
        .id;
    Arc::new(Device {
        socket,
        rdv,
        id,
        nat,
    })
}

/// Runs the host: prepares for each forwarded request and echoes one stream
/// on every accepted connection. Reports each preparation.
fn run_host(host: Arc<Device>, options: HostOptions) -> mpsc::UnboundedReceiver<Prepared> {
    let (tx, rx) = mpsc::unbounded_channel();
    let h = host.clone();
    tokio::spawn(async move {
        while let Some(request) = h.rdv.next_incoming().await {
            let prepared = prepare(&h.socket, &request, &options).await;
            let _ = tx.send(prepared);
        }
    });
    let ep = host.socket.endpoint().clone();
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            tokio::spawn(async move {
                let Ok(conn) = incoming.await else { return };
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let data = recv.read_to_end(1 << 20).await.unwrap_or_default();
                    let _ = send.write_all(&data).await;
                    let _ = send.finish();
                }
            });
        }
    });
    rx
}

async fn echo(conn: &Connection, payload: &[u8]) -> Vec<u8> {
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(payload).await.unwrap();
    send.finish().unwrap();
    recv.read_to_end(1 << 20).await.unwrap()
}

/// A UDP address that swallows packets (bound, never read): connections
/// to it time out, like a peer behind a NAT that was not punched.
fn black_hole() -> (UdpSocket, SocketAddr) {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    let a = s.local_addr().unwrap();
    (s, a)
}

async fn plan_for(controller: &Device, host: &Device) -> ConnectPlan {
    let info = controller.rdv.request_connection(host.id).await.unwrap();
    assert!(info.relay.is_some() && info.relay_session.is_some());
    assert_eq!(
        info.relay_ticket.len(),
        24,
        "the ID server issues relay tickets"
    );
    ConnectPlan::from_info(&info, controller.nat).await
}

#[tokio::test]
async fn nat_detection_on_loopback_reports_no_nat() {
    let n = net().await;
    let d = device(&n).await;
    assert_eq!(d.nat, NatType::Open);
    n.idserver.shutdown().await;
}

#[tokio::test]
async fn same_network_connects_over_the_lan_first() {
    let n = net().await;
    let (controller, host) = (device(&n).await, device(&n).await);
    let _prepared = run_host(host.clone(), HostOptions::default());
    let plan = plan_for(&controller, &host).await;
    let est = connect(&controller.socket, &plan, &ConnectOptions::default())
        .await
        .unwrap();
    // Both devices share a public IP (loopback), so LAN addresses are offered
    // whenever this machine has a private IPv4 address.
    let expected = if plan.lan.is_empty() {
        PathKind::Direct
    } else {
        PathKind::Lan
    };
    assert_eq!(est.path, expected, "{plan:?}");
    assert_eq!(echo(&est.conn, b"lan").await, b"lan");
    n.idserver.shutdown().await;
}

#[tokio::test]
async fn direct_ipv4_path_when_lan_is_skipped() {
    let n = net().await;
    let (controller, host) = (device(&n).await, device(&n).await);
    let mut prepared = run_host(host.clone(), HostOptions::default());
    let mut plan = plan_for(&controller, &host).await;
    plan.direct.retain(|a| a.is_ipv4());
    let options = ConnectOptions {
        try_lan: false,
        ..ConnectOptions::default()
    };
    let est = connect(&controller.socket, &plan, &options).await.unwrap();
    assert_eq!(est.path, PathKind::Direct);
    assert_eq!(
        est.remote.port(),
        host.socket.port(),
        "straight to the host's socket"
    );
    assert_eq!(echo(&est.conn, b"direct").await, b"direct");
    // The host punched toward the controller's observed address.
    let p = prepared.recv().await.unwrap();
    assert!(
        p.punched
            .iter()
            .any(|a| a.port() == controller.socket.port()),
        "{p:?}"
    );
    n.idserver.shutdown().await;
}

#[tokio::test]
async fn ipv6_path_when_the_machine_has_a_global_address() {
    let n = net().await;
    let (controller, host) = (device(&n).await, device(&n).await);
    let _prepared = run_host(host.clone(), HostOptions::default());
    let mut plan = plan_for(&controller, &host).await;
    plan.direct.retain(|a| a.is_ipv6());
    if plan.direct.is_empty() {
        eprintln!("skipped: this machine has no global IPv6 address");
        n.idserver.shutdown().await;
        return;
    }
    let options = ConnectOptions {
        try_lan: false,
        try_relay: false,
        ..ConnectOptions::default()
    };
    let est = connect(&controller.socket, &plan, &options).await.unwrap();
    assert_eq!(est.path, PathKind::Ipv6);
    assert!(est.remote.is_ipv6());
    assert_eq!(echo(&est.conn, b"v6").await, b"v6");
    n.idserver.shutdown().await;
}

#[tokio::test]
async fn falls_back_to_the_relay_when_direct_paths_fail() {
    let n = net().await;
    let (controller, host) = (device(&n).await, device(&n).await);
    let mut prepared = run_host(host.clone(), HostOptions::default());
    let mut plan = plan_for(&controller, &host).await;
    // Simulate NATs that block the LAN and direct paths.
    let (_lan_sink, lan) = black_hole();
    let (_direct_sink, direct) = black_hole();
    plan.lan = vec![lan];
    plan.direct = vec![direct];
    let options = ConnectOptions {
        lan_timeout: Duration::from_millis(300),
        direct_timeout: Duration::from_millis(500),
        ..ConnectOptions::default()
    };
    let est = connect(&controller.socket, &plan, &options).await.unwrap();
    assert_eq!(est.path, PathKind::Relay);
    let payload = vec![7u8; 100_000];
    assert_eq!(echo(&est.conn, &payload).await, payload);

    let p = prepared.recv().await.unwrap();
    assert_eq!(
        p.relay_data,
        Some(est.remote),
        "host bound the same session"
    );
    let stats = n.relay.stats();
    assert_eq!(stats.total_sessions, 1);
    assert!(stats.bytes_forwarded > 200_000, "{stats:?}");
    n.idserver.shutdown().await;
}

#[tokio::test]
async fn relay_only() {
    let n = net().await;
    let (controller, host) = (device(&n).await, device(&n).await);
    let _prepared = run_host(host.clone(), HostOptions::default());
    let plan = plan_for(&controller, &host).await;
    let options = ConnectOptions {
        try_lan: false,
        try_direct: false,
        ..ConnectOptions::default()
    };
    let est = connect(&controller.socket, &plan, &options).await.unwrap();
    assert_eq!(est.path, PathKind::Relay);
    assert_eq!(echo(&est.conn, b"relayed").await, b"relayed");
    n.idserver.shutdown().await;
}

#[tokio::test]
async fn every_path_failing_reports_each_attempt() {
    let n = net().await;
    let (controller, host) = (device(&n).await, device(&n).await);
    // The host neither punches usefully nor binds at the relay.
    let _prepared = run_host(
        host.clone(),
        HostOptions {
            use_relay: false,
            ..HostOptions::default()
        },
    );
    let mut plan = plan_for(&controller, &host).await;
    let (_sink, hole) = black_hole();
    plan.lan = vec![hole];
    plan.direct = vec![hole];
    let options = ConnectOptions {
        lan_timeout: Duration::from_millis(200),
        direct_timeout: Duration::from_millis(200),
        relay_timeout: Duration::from_millis(1500),
        ..ConnectOptions::default()
    };
    let err: ConnectError = connect(&controller.socket, &plan, &options)
        .await
        .unwrap_err();
    let kinds: Vec<_> = err.attempts.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        kinds,
        vec![PathKind::Lan, PathKind::Direct, PathKind::Relay]
    );
    n.idserver.shutdown().await;
    let _ = std::fs::remove_dir_all(&n.dir);
}
