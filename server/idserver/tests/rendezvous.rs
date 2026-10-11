//! End-to-end tests: a real ID server (QUIC on loopback, SQLite in a temp
//! directory) and real `kanodesk-rendezvous` clients.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::time::Duration;

use kanodesk_crypto::DeviceIdentity;
use kanodesk_idserver::{IdServer, Limits, Rate, ServerConfig, start};
use kanodesk_rendezvous::{
    ClientError, DeviceId, RendezvousClient, RendezvousConfig, RendezvousError,
};
use kanodesk_transport::tls::ServerTrust;
use kanodesk_transport::{Endpoint, endpoint};

fn localhost(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

/// A fresh temp directory per test.
fn temp_dir() -> PathBuf {
    let mut name = [0u8; 8];
    getrandom::fill(&mut name).unwrap();
    let hex: String = name.iter().map(|b| format!("{b:02x}")).collect();
    let dir = std::env::temp_dir().join(format!("kanodesk-idserver-{hex}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    server: IdServer,
    signing_seed: [u8; 32],
    dir: PathBuf,
    config: RendezvousConfig,
}

async fn start_server(limits: Limits) -> Fixture {
    let dir = temp_dir();
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).unwrap();
    let server = start(
        ServerConfig {
            limits,
            ..ServerConfig::new(localhost(0), dir.join("idserver.sqlite"))
        },
        DeviceIdentity::from_seed(&seed),
    )
    .await
    .unwrap();
    let config = RendezvousConfig {
        server_addr: server.local_addr().unwrap(),
        server_key: server.public_key(),
    };
    Fixture {
        server,
        signing_seed: seed,
        dir,
        config,
    }
}

fn client_endpoint() -> Endpoint {
    // The ID server is authenticated by its signed hello, not its certificate.
    endpoint::client(localhost(0), ServerTrust::InsecureAcceptAny).unwrap()
}

/// A device: identity, its own endpoint, and a connected client.
struct Device {
    identity: DeviceIdentity,
    endpoint: Endpoint,
    client: RendezvousClient,
}

async fn device(config: RendezvousConfig) -> Device {
    let endpoint = client_endpoint();
    let client = RendezvousClient::connect(&endpoint, config).await.unwrap();
    Device {
        identity: DeviceIdentity::generate().unwrap(),
        endpoint,
        client,
    }
}

fn server_error(e: ClientError) -> RendezvousError {
    match e {
        ClientError::Server { error, .. } => error,
        other => panic!("expected a server error, got {other}"),
    }
}

/// The prompt's done criterion: two clients, A looks up B by ID and gets
/// B's verified public key.
#[tokio::test]
async fn client_a_looks_up_client_b_and_gets_its_verified_key() {
    let f = start_server(Limits::default()).await;
    let a = device(f.config).await;
    let b = device(f.config).await;

    let a_record = a.client.register(&a.identity, None).await.unwrap();
    let b_record = b.client.register(&b.identity, None).await.unwrap();
    assert_ne!(a_record.id, b_record.id);
    assert_eq!(b_record.key, b.identity.public_key());

    let found = a.client.lookup(b_record.id).await.unwrap();
    assert_eq!(found.record.id, b_record.id);
    assert_eq!(
        found.record.key,
        b.identity.public_key(),
        "verified key is B's"
    );
    assert!(found.online);

    assert_eq!(f.server.device_count(), 2);
    assert_eq!(f.server.online_count(), 2);
    f.server.shutdown().await;
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[tokio::test]
async fn ids_are_nine_digits_and_stable_for_a_key() {
    let f = start_server(Limits::default()).await;
    let d = device(f.config).await;
    let first = d.client.register(&d.identity, None).await.unwrap();
    assert_eq!(first.id.to_string().len(), 9);
    // Same key, no saved ID (lost config): same ID back.
    let again = d.client.register(&d.identity, None).await.unwrap();
    assert_eq!(again.id, first.id);
    // A new connection with the saved ID.
    let other_conn = RendezvousClient::connect(&d.endpoint, f.config)
        .await
        .unwrap();
    let saved = other_conn
        .register(&d.identity, Some(first.id))
        .await
        .unwrap();
    assert_eq!(saved.id, first.id);
    f.server.shutdown().await;
}

#[tokio::test]
async fn an_id_cannot_be_taken_by_another_key() {
    let f = start_server(Limits::default()).await;
    let victim = device(f.config).await;
    let id = victim
        .client
        .register(&victim.identity, None)
        .await
        .unwrap()
        .id;
    let attacker = device(f.config).await;
    let err = attacker
        .client
        .register(&attacker.identity, Some(id))
        .await
        .unwrap_err();
    assert_eq!(server_error(err), RendezvousError::KeyMismatch);
    // The binding is unchanged.
    let found = attacker.client.lookup(id).await.unwrap();
    assert_eq!(found.record.key, victim.identity.public_key());
    f.server.shutdown().await;
}

#[tokio::test]
async fn connect_request_reaches_the_host_with_both_addresses() {
    let f = start_server(Limits::default()).await;
    let controller = device(f.config).await;
    let host = device(f.config).await;
    let c_id = controller
        .client
        .register(&controller.identity, None)
        .await
        .unwrap()
        .id;
    let h_id = host.client.register(&host.identity, None).await.unwrap().id;

    let info = controller.client.request_connection(h_id).await.unwrap();
    assert_eq!(info.host.key, host.identity.public_key());
    assert_eq!(info.host_addr, host.endpoint.local_addr().unwrap());
    assert_eq!(
        info.controller_addr,
        controller.endpoint.local_addr().unwrap()
    );

    let incoming = tokio::time::timeout(Duration::from_secs(5), host.client.next_incoming())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(incoming.controller.id, c_id);
    assert_eq!(incoming.controller.key, controller.identity.public_key());
    assert_eq!(incoming.controller_addr, info.controller_addr);
    assert_eq!(incoming.host_addr, info.host_addr);
    f.server.shutdown().await;
}

#[tokio::test]
async fn requests_need_registration_and_known_online_targets() {
    let f = start_server(Limits::default()).await;
    let a = device(f.config).await;
    let missing: DeviceId = "123456789".parse().unwrap();
    // Not registered yet.
    let err = a.client.request_connection(missing).await.unwrap_err();
    assert_eq!(server_error(err), RendezvousError::NotRegistered);
    a.client.register(&a.identity, None).await.unwrap();
    assert_eq!(
        server_error(a.client.request_connection(missing).await.unwrap_err()),
        RendezvousError::IdNotFound
    );
    assert_eq!(
        server_error(a.client.lookup(missing).await.unwrap_err()),
        RendezvousError::IdNotFound
    );

    // Registered but offline.
    let b = device(f.config).await;
    let b_id = b.client.register(&b.identity, None).await.unwrap().id;
    b.client.close();
    let mut offline = false;
    for _ in 0..50 {
        if !a.client.lookup(b_id).await.unwrap().online {
            offline = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(offline, "closing the connection marks the device offline");
    assert_eq!(
        server_error(a.client.request_connection(b_id).await.unwrap_err()),
        RendezvousError::Offline
    );
    f.server.shutdown().await;
}

#[tokio::test]
async fn online_query_and_ping() {
    let f = start_server(Limits::default()).await;
    let a = device(f.config).await;
    let b = device(f.config).await;
    let a_id = a.client.register(&a.identity, None).await.unwrap().id;
    let b_id = b.client.register(&b.identity, None).await.unwrap().id;
    let unknown: DeviceId = "999999999".parse().unwrap();
    let states = a.client.query_online(&[b_id, unknown, a_id]).await.unwrap();
    assert_eq!(states, vec![true, false, true]);
    assert!(a.client.ping().await.unwrap() < Duration::from_secs(1));
    f.server.shutdown().await;
}

#[tokio::test]
async fn wrong_pinned_server_key_is_refused() {
    let f = start_server(Limits::default()).await;
    let wrong = RendezvousConfig {
        server_key: DeviceIdentity::generate().unwrap().public_key(),
        ..f.config
    };
    let result = RendezvousClient::connect(&client_endpoint(), wrong).await;
    assert!(matches!(result, Err(ClientError::ServerNotTrusted)));
    f.server.shutdown().await;
}

#[tokio::test]
async fn bindings_survive_a_restart() {
    let f = start_server(Limits::default()).await;
    let d = device(f.config).await;
    let id = d.client.register(&d.identity, None).await.unwrap().id;
    drop(d.client);
    let (seed, dir) = (f.signing_seed, f.dir.clone());
    f.server.shutdown().await;

    let server = start(
        ServerConfig::new(localhost(0), dir.join("idserver.sqlite")),
        DeviceIdentity::from_seed(&seed),
    )
    .await
    .unwrap();
    assert_eq!(server.device_count(), 1);
    let config = RendezvousConfig {
        server_addr: server.local_addr().unwrap(),
        server_key: server.public_key(),
    };
    let other = device(config).await;
    let found = other.client.lookup(id).await.unwrap();
    assert_eq!(found.record.key, d.identity.public_key());
    assert!(!found.online);
    // The device gets its ID back after the restart.
    let again = other_conn(&d.endpoint, config).await;
    assert_eq!(again.register(&d.identity, None).await.unwrap().id, id);
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

async fn other_conn(endpoint: &Endpoint, config: RendezvousConfig) -> RendezvousClient {
    RendezvousClient::connect(endpoint, config).await.unwrap()
}

#[tokio::test]
async fn rate_limits_per_ip_and_per_target() {
    // Per target: 3 lookups, then refused, even from different connections.
    let tight_target = Limits {
        per_target: Rate::new(3, Duration::from_secs(3600)),
        ..Limits::default()
    };
    let f = start_server(tight_target).await;
    let a = device(f.config).await;
    let b = device(f.config).await;
    let b_id = b.client.register(&b.identity, None).await.unwrap().id;
    for _ in 0..3 {
        a.client.lookup(b_id).await.unwrap();
    }
    match a.client.lookup(b_id).await.unwrap_err() {
        ClientError::Server {
            error: RendezvousError::RateLimited,
            retry_after: Some(d),
        } => assert!(d > Duration::ZERO),
        other => panic!("expected rate limiting, got {other}"),
    }
    let c = device(f.config).await;
    assert_eq!(
        server_error(c.client.lookup(b_id).await.unwrap_err()),
        RendezvousError::RateLimited
    );
    f.server.shutdown().await;

    // Per IP: 5 requests in a burst for everything from 127.0.0.1.
    let tight_ip = Limits {
        per_ip: Rate::new(5, Duration::from_secs(3600)),
        ..Limits::default()
    };
    let f = start_server(tight_ip).await;
    let a = device(f.config).await;
    for _ in 0..5 {
        a.client.ping().await.unwrap();
    }
    assert_eq!(
        server_error(a.client.ping().await.unwrap_err()),
        RendezvousError::RateLimited
    );
    f.server.shutdown().await;

    // New IDs per IP: 2, then refused; existing devices still re-register.
    let tight_ids = Limits {
        new_ids_per_ip: Rate::new(2, Duration::from_secs(3600)),
        ..Limits::default()
    };
    let f = start_server(tight_ids).await;
    let d1 = device(f.config).await;
    d1.client.register(&d1.identity, None).await.unwrap();
    let d2 = device(f.config).await;
    d2.client.register(&d2.identity, None).await.unwrap();
    let d3 = device(f.config).await;
    assert_eq!(
        server_error(d3.client.register(&d3.identity, None).await.unwrap_err()),
        RendezvousError::RateLimited
    );
    assert!(d1.client.register(&d1.identity, None).await.is_ok());
    f.server.shutdown().await;
}

/// Like `start_server`, with full control over the config.
async fn start_with(edit: impl FnOnce(&mut ServerConfig)) -> Fixture {
    let dir = temp_dir();
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).unwrap();
    let mut config = ServerConfig::new(localhost(0), dir.join("idserver.sqlite"));
    edit(&mut config);
    let server = start(config, DeviceIdentity::from_seed(&seed))
        .await
        .unwrap();
    let config = RendezvousConfig {
        server_addr: server.local_addr().unwrap(),
        server_key: server.public_key(),
    };
    Fixture {
        server,
        signing_seed: seed,
        dir,
        config,
    }
}

#[tokio::test]
async fn a_device_can_move_to_a_free_id() {
    let f = start_server(Limits::default()).await;
    let d = device(f.config).await;
    let other = device(f.config).await;
    let old = d.client.register(&d.identity, None).await.unwrap().id;
    let taken = other
        .client
        .register(&other.identity, None)
        .await
        .unwrap()
        .id;

    let wanted: DeviceId = "424242424".parse().unwrap();
    let moved = d.client.change_id(&d.identity, wanted).await.unwrap();
    assert_eq!((moved.id, moved.key), (wanted, d.identity.public_key()));

    // The old ID is free, the new one resolves to the same key and is online.
    assert_eq!(
        server_error(other.client.lookup(old).await.unwrap_err()),
        RendezvousError::IdNotFound
    );
    let found = other.client.lookup(wanted).await.unwrap();
    assert_eq!(found.record.key, d.identity.public_key());
    assert!(found.online);
    // Connect requests reach it under the new ID.
    other.client.request_connection(wanted).await.unwrap();
    let incoming = tokio::time::timeout(Duration::from_secs(5), d.client.next_incoming())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(incoming.controller.id, taken);

    // A taken ID is refused, and so is a change before registering.
    assert_eq!(
        server_error(d.client.change_id(&d.identity, taken).await.unwrap_err()),
        RendezvousError::IdTaken
    );
    let fresh = device(f.config).await;
    assert_eq!(
        server_error(
            fresh
                .client
                .change_id(&fresh.identity, old)
                .await
                .unwrap_err()
        ),
        RendezvousError::NotRegistered
    );
    // The change is persisted.
    f.server.shutdown().await;
    let rows = kanodesk_idserver::admin::list(&f.dir.join("idserver.sqlite"))
        .await
        .unwrap();
    assert!(
        rows.iter()
            .any(|r| r.id == wanted && r.key == d.identity.public_key())
    );
    assert!(!rows.iter().any(|r| r.id == old));
}

#[tokio::test]
async fn relays_are_assigned_round_robin_to_both_sides() {
    let f = start_with(|c| c.relays = vec!["relay-a:21117".into(), "relay-b:21117".into()]).await;
    let controller = device(f.config).await;
    let host = device(f.config).await;
    controller
        .client
        .register(&controller.identity, None)
        .await
        .unwrap();
    let h_id = host.client.register(&host.identity, None).await.unwrap().id;

    let mut seen = Vec::new();
    for _ in 0..2 {
        let info = controller.client.request_connection(h_id).await.unwrap();
        let incoming = tokio::time::timeout(Duration::from_secs(5), host.client.next_incoming())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.relay, incoming.relay, "both sides get the same relay");
        seen.push(info.relay.unwrap());
    }
    assert_eq!(seen, vec!["relay-a:21117", "relay-b:21117"]);

    // Without configured relays the field is absent.
    let g = start_server(Limits::default()).await;
    let c2 = device(g.config).await;
    let h2 = device(g.config).await;
    c2.client.register(&c2.identity, None).await.unwrap();
    let h2_id = h2.client.register(&h2.identity, None).await.unwrap().id;
    assert_eq!(
        c2.client.request_connection(h2_id).await.unwrap().relay,
        None
    );
    f.server.shutdown().await;
    g.server.shutdown().await;
}

#[tokio::test]
async fn devices_behind_one_nat_do_not_share_a_budget() {
    // Per IP only 3 requests, per device 50: registered devices keep working.
    let f = start_with(|c| {
        c.limits.per_ip = Rate::new(3, Duration::from_secs(3600));
        c.limits.per_device = Rate::new(50, Duration::from_secs(3600));
    })
    .await;
    let a = device(f.config).await;
    let b = device(f.config).await;
    a.client.register(&a.identity, None).await.unwrap();
    b.client.register(&b.identity, None).await.unwrap();
    for _ in 0..20 {
        a.client.ping().await.unwrap();
        b.client.ping().await.unwrap();
    }
    // An unregistered connection from the same IP is out of per-IP tokens.
    let c = device(f.config).await;
    c.client.ping().await.unwrap();
    assert_eq!(
        server_error(c.client.ping().await.unwrap_err()),
        RendezvousError::RateLimited
    );
    f.server.shutdown().await;
}

#[tokio::test]
async fn admin_remove_frees_the_id_for_a_device_that_lost_its_key() {
    let f = start_server(Limits::default()).await;
    let lost = device(f.config).await;
    let id = lost.client.register(&lost.identity, None).await.unwrap().id;
    let database = f.dir.join("idserver.sqlite");

    // Admin commands refuse to run while the server holds the database.
    assert!(matches!(
        kanodesk_idserver::admin::list(&database).await,
        Err(kanodesk_idserver::ServerError::Locked(_))
    ));
    drop(lost);
    let seed = f.signing_seed;
    f.server.shutdown().await;

    let row = kanodesk_idserver::admin::show(&database, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.id, id);
    assert!(row.last_addr.is_some());
    assert!(
        kanodesk_idserver::admin::remove(&database, id)
            .await
            .unwrap()
    );
    assert!(
        !kanodesk_idserver::admin::remove(&database, id)
            .await
            .unwrap()
    );
    assert!(
        kanodesk_idserver::admin::show(&database, id)
            .await
            .unwrap()
            .is_none()
    );

    // The device, with a brand-new key, asks for its old ID again.
    let server = start(
        ServerConfig::new(localhost(0), database.clone()),
        DeviceIdentity::from_seed(&seed),
    )
    .await
    .unwrap();
    let config = RendezvousConfig {
        server_addr: server.local_addr().unwrap(),
        server_key: server.public_key(),
    };
    let recovered = device(config).await;
    let record = recovered
        .client
        .register(&recovered.identity, Some(id))
        .await
        .unwrap();
    assert_eq!(
        (record.id, record.key),
        (id, recovered.identity.public_key())
    );
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[tokio::test]
async fn a_second_server_cannot_open_the_same_database() {
    let f = start_server(Limits::default()).await;
    let second = start(
        ServerConfig::new(localhost(0), f.dir.join("idserver.sqlite")),
        DeviceIdentity::generate().unwrap(),
    )
    .await;
    assert!(matches!(
        second,
        Err(kanodesk_idserver::ServerError::Locked(_))
    ));
    f.server.shutdown().await;
}

#[tokio::test]
async fn last_seen_is_refreshed_while_online() {
    let f = start_with(|c| c.touch_interval = Duration::from_millis(200)).await;
    let d = device(f.config).await;
    let id = d.client.register(&d.identity, None).await.unwrap().id;
    let db = kanodesk_idserver::db::Db::open(&f.dir.join("idserver.sqlite"))
        .await
        .unwrap();
    let first = db.get(id).await.unwrap().unwrap().last_seen;
    // Unix-second resolution: wait past the next second boundary.
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let later = db.get(id).await.unwrap().unwrap().last_seen;
    assert!(later > first, "last_seen {first} -> {later}");
    db.close().await;
    f.server.shutdown().await;
}

fn server_identity(f: &Fixture) -> DeviceIdentity {
    DeviceIdentity::from_seed(&f.signing_seed)
}

#[tokio::test]
async fn relay_list_is_pushed_on_register_and_on_change_and_persisted() {
    let f = start_with(|c| c.relays = vec!["relay-a:21117".into()]).await;
    let d = device(f.config).await;
    assert!(d.client.relays().is_empty(), "nothing before registering");
    d.client.register(&d.identity, None).await.unwrap();
    assert_eq!(
        d.client.relays(),
        vec!["relay-a:21117"],
        "known when register returns"
    );

    let mut updates = d.client.relay_updates();
    let admin =
        kanodesk_idserver::admin::OnlineAdmin::connect(f.config.server_addr, &server_identity(&f))
            .await
            .unwrap();
    admin
        .set_relays(&["relay-b:21117".into(), "relay-c:21117".into()])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), updates.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.client.relays(), vec!["relay-b:21117", "relay-c:21117"]);
    assert_eq!(f.server.relays(), vec!["relay-b:21117", "relay-c:21117"]);

    // New connect requests use the new list.
    let c = device(f.config).await;
    c.client.register(&c.identity, None).await.unwrap();
    let id = d.client.register(&d.identity, None).await.unwrap().id;
    let info = c.client.request_connection(id).await.unwrap();
    assert!(info.relay.unwrap().starts_with("relay-"));

    // Persisted: a restart without --relay keeps it.
    let (seed, dir) = (f.signing_seed, f.dir.clone());
    drop((admin, c, d));
    f.server.shutdown().await;
    assert_eq!(
        kanodesk_idserver::admin::relays(&dir.join("idserver.sqlite"))
            .await
            .unwrap(),
        vec!["relay-b:21117", "relay-c:21117"]
    );
    let server = start(
        ServerConfig::new(localhost(0), dir.join("idserver.sqlite")),
        DeviceIdentity::from_seed(&seed),
    )
    .await
    .unwrap();
    assert_eq!(server.relays(), vec!["relay-b:21117", "relay-c:21117"]);
    server.shutdown().await;
}

#[tokio::test]
async fn relay_session_is_shared_by_both_sides_and_fresh_per_request() {
    let f = start_with(|c| c.relays = vec!["relay-a:21117".into()]).await;
    let controller = device(f.config).await;
    let host = device(f.config).await;
    controller
        .client
        .register(&controller.identity, None)
        .await
        .unwrap();
    let h_id = host.client.register(&host.identity, None).await.unwrap().id;
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let info = controller.client.request_connection(h_id).await.unwrap();
        let incoming = tokio::time::timeout(Duration::from_secs(5), host.client.next_incoming())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.relay_session, incoming.relay_session);
        sessions.push(info.relay_session.unwrap());
    }
    assert_ne!(sessions[0], sessions[1]);
    f.server.shutdown().await;
}

#[tokio::test]
async fn online_admin_lists_and_removes_while_running() {
    let f = start_server(Limits::default()).await;
    let victim = device(f.config).await;
    let id = victim
        .client
        .register(&victim.identity, None)
        .await
        .unwrap()
        .id;
    let offline = device(f.config).await;
    let offline_id = offline
        .client
        .register(&offline.identity, None)
        .await
        .unwrap()
        .id;
    offline.client.close();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let admin =
        kanodesk_idserver::admin::OnlineAdmin::connect(f.config.server_addr, &server_identity(&f))
            .await
            .unwrap();
    let all = admin.list().await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|s| s.row.id == id && s.online));
    assert!(all.iter().any(|s| s.row.id == offline_id && !s.online));
    assert_eq!(
        admin.show(id).await.unwrap().unwrap().row.key,
        victim.identity.public_key()
    );

    // Remove: the device is disconnected at once and its ID is free.
    assert!(admin.remove(id).await.unwrap());
    assert!(!admin.remove(id).await.unwrap());
    assert!(admin.show(id).await.unwrap().is_none());
    let gone = tokio::time::timeout(Duration::from_secs(5), victim.client.next_incoming())
        .await
        .unwrap();
    assert!(gone.is_none(), "removed device's connection is closed");
    let newcomer = device(f.config).await;
    let record = newcomer
        .client
        .register(&newcomer.identity, Some(id))
        .await
        .unwrap();
    assert_eq!(
        (record.id, record.key),
        (id, newcomer.identity.public_key())
    );
    f.server.shutdown().await;
}

#[tokio::test]
async fn admin_requests_need_the_server_key() {
    use kanodesk_proto::v1::{self, rendezvous_message::Msg};
    let f = start_server(Limits::default()).await;
    let d = device(f.config).await;
    let err = d
        .client
        .call(Msg::AdminList(v1::AdminList {}))
        .await
        .unwrap_err();
    assert_eq!(server_error(err), RendezvousError::Forbidden);
    // Signing with a device key instead of the server key is refused.
    let forged =
        kanodesk_rendezvous::record::sign_admin_auth(&d.identity, d.client.channel_binding());
    assert_eq!(
        server_error(d.client.call(Msg::AdminAuth(forged)).await.unwrap_err()),
        RendezvousError::BadSignature
    );
    assert_eq!(
        server_error(
            d.client
                .call(Msg::AdminList(v1::AdminList {}))
                .await
                .unwrap_err()
        ),
        RendezvousError::Forbidden
    );
    // A wrong key fails already at the server-identity check.
    let wrong = kanodesk_idserver::admin::OnlineAdmin::connect(
        f.config.server_addr,
        &DeviceIdentity::generate().unwrap(),
    )
    .await;
    assert!(wrong.is_err());
    f.server.shutdown().await;
}

#[tokio::test]
async fn trusted_networks_skip_the_per_ip_limits() {
    let tight = |c: &mut ServerConfig| {
        c.limits.per_ip = Rate::new(1, Duration::from_secs(3600));
        c.limits.new_ids_per_ip = Rate::new(1, Duration::from_secs(3600));
    };
    // Loopback trusted: many unregistered requests and new IDs are fine.
    let f = start_with(|c| {
        tight(c);
        c.trusted_networks = vec!["127.0.0.0/8".parse().unwrap()];
    })
    .await;
    for _ in 0..4 {
        let d = device(f.config).await;
        d.client.ping().await.unwrap();
        d.client.ping().await.unwrap();
        d.client.register(&d.identity, None).await.unwrap();
    }
    f.server.shutdown().await;

    // Another network trusted: loopback is limited as usual.
    let g = start_with(|c| {
        tight(c);
        c.trusted_networks = vec!["10.0.0.0/8".parse().unwrap()];
    })
    .await;
    let d = device(g.config).await;
    d.client.ping().await.unwrap();
    assert_eq!(
        server_error(d.client.ping().await.unwrap_err()),
        RendezvousError::RateLimited
    );
    g.server.shutdown().await;
}
