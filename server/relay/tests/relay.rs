//! End-to-end tests: a real relay and real peer sockets on loopback.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use kanodesk_proto::v1::RelayRole;
use kanodesk_relay::{RelayConfig, RelayServer, start};
use kanodesk_transport::peer::PeerSocket;
use kanodesk_transport::relay::{self, RelaySecret, issue_ticket};
use kanodesk_transport::tls::{Identity, ServerTrust};
use kanodesk_transport::{Connection, endpoint};

const SECRET: [u8; 32] = [42u8; 32];

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn token() -> [u8; 16] {
    let mut t = [0u8; 16];
    getrandom::fill(&mut t).unwrap();
    t
}

fn ticket(session: &[u8; 16]) -> Vec<u8> {
    issue_ticket(&SECRET, session, now() + 60)
}

fn peer() -> PeerSocket {
    PeerSocket::bind(
        0,
        &Identity::self_signed().unwrap(),
        ServerTrust::InsecureAcceptAny,
    )
    .unwrap()
}

async fn relay(edit: impl FnOnce(&mut RelayConfig)) -> RelayServer {
    let mut config = RelayConfig::new("127.0.0.1:0".parse().unwrap(), Some(RelaySecret(SECRET)));
    edit(&mut config);
    start(config).await.unwrap()
}

/// Both peers bind and say hello; returns the data address.
async fn join(
    relay_addr: SocketAddr,
    controller: &PeerSocket,
    host: &PeerSocket,
    session: &[u8; 16],
) -> SocketAddr {
    let t = ticket(session);
    let a = relay::bind(controller, relay_addr, session, &t, RelayRole::Controller)
        .await
        .unwrap();
    let b = relay::bind(host, relay_addr, session, &t, RelayRole::Host)
        .await
        .unwrap();
    assert_eq!(a, b, "both sides get the same data port");
    relay::send_hellos(host, b, session, RelayRole::Host, 3).await;
    relay::send_hellos(controller, a, session, RelayRole::Controller, 3).await;
    a
}

/// Host side: accept one connection and echo one stream.
fn echo_host(host: &PeerSocket) -> tokio::task::JoinHandle<SocketAddr> {
    let ep = host.endpoint().clone();
    tokio::spawn(async move {
        let conn = ep.accept().await.unwrap().await.unwrap();
        let remote = conn.remote_address();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let data = recv.read_to_end(16 * 1024 * 1024).await.unwrap();
        send.write_all(&data).await.unwrap();
        send.finish().unwrap();
        conn.closed().await;
        remote
    })
}

async fn echo(conn: &Connection, payload: &[u8]) -> Vec<u8> {
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(payload).await.unwrap();
    send.finish().unwrap();
    recv.read_to_end(16 * 1024 * 1024).await.unwrap()
}

#[tokio::test]
async fn relays_a_quic_connection_between_two_peers() {
    let r = relay(|_| {}).await;
    let (controller, host) = (peer(), peer());
    let session = token();
    let data = join(r.local_addr().unwrap(), &controller, &host, &session).await;

    let accepted = echo_host(&host);
    let conn = endpoint::connect(controller.endpoint(), data)
        .await
        .unwrap();
    assert_eq!(
        echo(&conn, b"through the relay").await,
        b"through the relay"
    );
    conn.close(0u32.into(), b"");
    let host_saw = accepted.await.unwrap();
    // The host sees the relay, not the controller.
    assert_eq!(kanodesk_transport::peer::canonical(host_saw), data);

    let stats = r.stats();
    assert_eq!(stats.total_sessions, 1);
    assert!(stats.bytes_forwarded > 2 * 17, "{stats:?}");
    r.shutdown().await;
}

#[tokio::test]
async fn binds_need_a_valid_ticket() {
    let r = relay(|_| {}).await;
    let p = peer();
    let addr = r.local_addr().unwrap();
    let session = token();
    for bad in [
        Vec::new(),
        issue_ticket(&[1u8; 32], &session, now() + 60),
        issue_ticket(&SECRET, &session, now() - 1),
    ] {
        let err = relay::bind(&p, addr, &session, &bad, RelayRole::Host)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("BadTicket"), "{err}");
    }
    // A ticket for another session does not work either.
    let other = token();
    assert!(
        relay::bind(&p, addr, &session, &ticket(&other), RelayRole::Host)
            .await
            .is_err()
    );
    assert_eq!(r.stats().total_sessions, 0);

    // Open mode accepts anything (tests only).
    let open = start(RelayConfig::new("127.0.0.1:0".parse().unwrap(), None))
        .await
        .unwrap();
    assert!(
        relay::bind(
            &p,
            open.local_addr().unwrap(),
            &session,
            &[],
            RelayRole::Host
        )
        .await
        .is_ok()
    );
    r.shutdown().await;
    open.shutdown().await;
}

#[tokio::test]
async fn strangers_cannot_join_or_inject() {
    let r = relay(|_| {}).await;
    let (controller, host, stranger) = (peer(), peer(), peer());
    let session = token();
    let data = join(r.local_addr().unwrap(), &controller, &host, &session).await;

    // Wrong session token in a hello: ignored. Plain junk: dropped.
    relay::send_hellos(&stranger, data, &token(), RelayRole::Host, 3).await;
    for _ in 0..5 {
        stranger
            .send_raw(b"junk that is not from a session member", data)
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(r.stats().dropped_unknown >= 5);

    // The real pair still works.
    let accepted = echo_host(&host);
    let conn = endpoint::connect(controller.endpoint(), data)
        .await
        .unwrap();
    assert_eq!(echo(&conn, b"still private").await, b"still private");
    conn.close(0u32.into(), b"");
    accepted.await.unwrap();
    r.shutdown().await;
}

#[tokio::test]
async fn bandwidth_limit_slows_a_bulk_transfer() {
    const LIMIT: u64 = 256 * 1024;
    let r = relay(|c| c.session_bandwidth = LIMIT).await;
    let (controller, host) = (peer(), peer());
    let session = token();
    let data = join(r.local_addr().unwrap(), &controller, &host, &session).await;

    let accepted = echo_host(&host);
    let conn = endpoint::connect(controller.endpoint(), data)
        .await
        .unwrap();
    // 384 KiB each way = 768 KiB through a 256 KiB/s session with a 1 s burst:
    // at least 2 s.
    let payload = vec![0x5a; 384 * 1024];
    let start = Instant::now();
    assert_eq!(echo(&conn, &payload).await.len(), payload.len());
    let elapsed = start.elapsed();
    conn.close(0u32.into(), b"");
    accepted.await.unwrap();
    assert!(
        elapsed >= Duration::from_millis(1500),
        "took only {elapsed:?}"
    );
    assert!(r.stats().dropped_over_limit > 0);
    r.shutdown().await;
}

#[tokio::test]
async fn idle_sessions_end_and_ports_follow_the_range() {
    let r = relay(|c| {
        c.idle_timeout = Duration::from_millis(300);
        c.port_range = Some((47100, 47120));
    })
    .await;
    let p = peer();
    let session = token();
    let data = relay::bind(
        &p,
        r.local_addr().unwrap(),
        &session,
        &ticket(&session),
        RelayRole::Host,
    )
    .await
    .unwrap();
    assert!((47100..=47120).contains(&data.port()), "{data}");
    assert_eq!(r.stats().active_sessions, 1);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(r.stats().active_sessions, 0, "idle session cleaned up");
    r.shutdown().await;
}

#[tokio::test]
async fn sessions_per_ip_are_capped() {
    let r = relay(|c| c.sessions_per_ip = 2).await;
    let p = peer();
    let addr = r.local_addr().unwrap();
    for _ in 0..2 {
        let s = token();
        relay::bind(&p, addr, &s, &ticket(&s), RelayRole::Host)
            .await
            .unwrap();
    }
    let s = token();
    let err = relay::bind(&p, addr, &s, &ticket(&s), RelayRole::Host)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Full"), "{err}");
    r.shutdown().await;
}
