//! The host engine end to end over QUIC: approval modes, the connection
//! manager, live permissions, and that denial and disconnection stop video.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kanodesk_config::ApproveMode;
use kanodesk_crypto::password::{HostPasswords, TemporaryPolicy};
use kanodesk_crypto::throttle::{LoginThrottle, ThrottleConfig};
use kanodesk_crypto::{DeviceIdentity, DevicePublicKey, PasswordKind};
use kanodesk_host::cm::{ApprovalFuture, ConnectionManager, Decision};
use kanodesk_host::engine::{DisplayList, EngineParts, EngineSettings, HostEngine};
use kanodesk_host::video::{Producer, ProducerFn, VideoHub};
use kanodesk_input::{InputEvent, KeyCode, Modifiers, Permissions, RecordingSink, SinkCall};
use kanodesk_ipc::cm::{HostStatus, Perms, SessionInfo, SessionState};
use kanodesk_proto::hello;
use kanodesk_proto::v1::{
    self, Capability, LoginError, PermissionKind, control_message, video_message,
};
use kanodesk_transport::session::{Credentials, SecureControl, SessionError, client_handshake};
use kanodesk_transport::streams::{self, IncomingStream};
use kanodesk_transport::tls::{Identity, ServerTrust};
use kanodesk_transport::{Connection, RecvStream, SendStream, endpoint};
use tokio::sync::{Notify, oneshot};

/// A connection manager the test drives.
#[derive(Default)]
struct ScriptedCm {
    events: Mutex<Vec<SessionInfo>>,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Decision>>>,
    changed: Notify,
    statuses: Mutex<Vec<HostStatus>>,
}

impl ConnectionManager for ScriptedCm {
    fn session_changed(&self, info: &SessionInfo) {
        self.events.lock().unwrap().push(info.clone());
        self.changed.notify_waiters();
    }

    fn request_approval(&self, info: &SessionInfo) -> ApprovalFuture {
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(info.id, tx);
        self.events.lock().unwrap().push(info.clone());
        self.changed.notify_waiters();
        Box::pin(async move { rx.await.ok() })
    }

    fn status_changed(&self, status: &HostStatus) {
        self.statuses.lock().unwrap().push(status.clone());
    }
}

impl ScriptedCm {
    /// Waits for an event matching `pred`.
    async fn wait_for(&self, pred: impl Fn(&SessionInfo) -> bool) -> SessionInfo {
        for _ in 0..500 {
            if let Some(e) = self.events.lock().unwrap().iter().find(|e| pred(e)) {
                return e.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "no such session event; saw {:?}",
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|e| (e.id, e.state.clone()))
                .collect::<Vec<_>>()
        );
    }

    fn decide(&self, id: u64, decision: Decision) {
        let tx = self.waiting.lock().unwrap().remove(&id).expect("waiting");
        tx.send(decision).unwrap();
    }

    fn approvals_asked(&self) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e.state, SessionState::AwaitingApproval { .. }))
            .count()
    }
}

fn awaiting(e: &SessionInfo) -> bool {
    matches!(e.state, SessionState::AwaitingApproval { .. })
}

fn active(e: &SessionInfo) -> bool {
    e.state == SessionState::Active
}

fn ended(e: &SessionInfo) -> bool {
    matches!(e.state, SessionState::Ended { .. })
}

/// Stream start, then a frame every 5 ms (key when asked) while watched.
fn producer(starts: Arc<AtomicU32>) -> ProducerFn {
    Arc::new(move |display, out: &Producer| {
        starts.fetch_add(1, Ordering::SeqCst);
        out.send(
            v1::StreamStart {
                display,
                codec: v1::Codec::Vp9 as i32,
                width: 64,
                height: 32,
            }
            .into(),
        );
        let mut seq = 0;
        loop {
            let frame = v1::VideoFrame {
                display,
                seq,
                key: out.take_keyframe_request(),
                ..Default::default()
            };
            seq += 1;
            if !out.send(frame.into()) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })
}

struct Host {
    engine: HostEngine,
    addr: SocketAddr,
    key: DevicePublicKey,
    cm: Arc<ScriptedCm>,
    sink: RecordingSink,
    starts: Arc<AtomicU32>,
}

fn start(mode: ApproveMode) -> Host {
    let identity = DeviceIdentity::generate().unwrap();
    let key = identity.public_key();
    let cm = Arc::new(ScriptedCm::default());
    let sink = RecordingSink::default();
    let starts = Arc::new(AtomicU32::new(0));
    let displays: DisplayList = Arc::new(|| {
        Ok(vec![v1::DisplayInfo {
            id: 0,
            name: "test".into(),
            width: 64,
            height: 32,
            primary: true,
            ..Default::default()
        }])
    });
    let factory_sink = sink.clone();
    let engine = HostEngine::new(EngineParts {
        identity,
        passwords: HostPasswords::new(TemporaryPolicy::default()).unwrap(),
        throttle: LoginThrottle::new(ThrottleConfig::default()),
        throttle_path: None,
        settings: EngineSettings {
            approve_mode: mode,
            approve_timeout: Duration::from_secs(10),
            click_delay: Duration::from_millis(0),
            permissions: Permissions::ALL,
            display: None,
            agent: "engine-test".into(),
            listen: None,
        },
        cm: cm.clone(),
        video: VideoHub::new(producer(starts.clone())),
        displays,
        input: Arc::new(move || Ok(Box::new(factory_sink.clone()))),
    });
    let server = endpoint::server(
        "127.0.0.1:0".parse().unwrap(),
        &Identity::self_signed().unwrap(),
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    let serving = engine.clone();
    tokio::spawn(async move { serving.serve(server).await });
    Host {
        engine,
        addr,
        key,
        cm,
        sink,
        starts,
    }
}

type Control = SecureControl<RecvStream, SendStream>;

struct Viewer {
    conn: Connection,
    control: Control,
    password: Option<PasswordKind>,
    waits: u32,
}

async fn connect(
    addr: SocketAddr,
    key: DevicePublicKey,
    credentials: Credentials<'_>,
) -> Result<Viewer, SessionError> {
    let client = endpoint::client(
        "127.0.0.1:0".parse().unwrap(),
        ServerTrust::InsecureAcceptAny,
    )
    .unwrap();
    let conn = endpoint::connect(&client, addr).await.unwrap();
    let identity = DeviceIdentity::generate().unwrap();
    let ours = hello::hello(
        &[Capability::VideoVp9, Capability::KeyframeRequest],
        "viewer-test",
    );
    let waits = AtomicU32::new(0);
    let mut on_pending = || {
        waits.fetch_add(1, Ordering::SeqCst);
    };
    let (control, _, password) =
        client_handshake(&conn, &identity, &key, &ours, credentials, &mut on_pending).await?;
    Ok(Viewer {
        conn,
        control,
        password,
        waits: waits.load(Ordering::SeqCst),
    })
}

/// Reads the post-login control messages: displays and two permissions.
async fn read_welcome(control: &mut Control) -> (Vec<v1::DisplayInfo>, Permissions) {
    let mut perms = Permissions::NONE;
    let mut displays = Vec::new();
    for _ in 0..3 {
        match control.recv().await.unwrap().and_then(|m| m.msg) {
            Some(control_message::Msg::Displays(d)) => displays = d.displays,
            Some(control_message::Msg::Permission(p)) => perms = perms.with(&p),
            other => panic!("unexpected {other:?}"),
        }
    }
    (displays, perms)
}

async fn first_video(conn: &Connection) -> v1::VideoMessage {
    let IncomingStream::Video { mut reader, .. } =
        tokio::time::timeout(Duration::from_secs(5), streams::accept_uni(conn))
            .await
            .unwrap()
            .unwrap();
    reader.next().await.unwrap().unwrap()
}

async fn producers_stop(host: &Host) {
    for _ in 0..300 {
        if host.engine.video().active_displays().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("video kept running");
}

#[tokio::test]
async fn password_mode_runs_the_session_without_asking() {
    let host = start(ApproveMode::Password);
    let password = host.engine.temporary_password();
    let mut v = connect(host.addr, host.key, Credentials::Password(&password))
        .await
        .unwrap();
    assert_eq!(v.password, Some(PasswordKind::Temporary));
    assert_eq!(v.waits, 0);
    let (displays, perms) = read_welcome(&mut v.control).await;
    assert_eq!(displays.len(), 1);
    assert_eq!(perms, Permissions::ALL);
    assert!(matches!(
        first_video(&v.conn).await.msg,
        Some(video_message::Msg::StreamStart(_))
    ));
    let info = host.cm.wait_for(active).await;
    assert_eq!(info.agent, "viewer-test");
    assert_eq!(info.password.as_deref(), Some("temporary"));
    assert_eq!(host.cm.approvals_asked(), 0);
    // A one-time password: the CM heard about the new one.
    assert!(host.cm.statuses.lock().unwrap().len() >= 2);

    // The viewer leaves: the session ends and the producer stops.
    endpoint::close(&v.conn, b"bye");
    host.cm.wait_for(ended).await;
    producers_stop(&host).await;
    assert!(host.engine.sessions().is_empty());
}

#[tokio::test]
async fn click_mode_denial_never_starts_video() {
    let host = start(ApproveMode::Click);
    let (addr, key) = (host.addr, host.key);
    let viewer = tokio::spawn(async move { connect(addr, key, Credentials::AskHost).await.err() });
    let info = host.cm.wait_for(awaiting).await;
    assert_eq!(info.agent, "viewer-test");
    assert_eq!(info.password, None);
    host.cm.decide(info.id, Decision::Deny);
    let error = viewer.await.unwrap();
    assert!(
        matches!(
            error,
            Some(SessionError::Rejected {
                error: LoginError::Denied,
                ..
            })
        ),
        "{error:?}"
    );
    let ended = host.cm.wait_for(ended).await;
    assert_eq!(
        ended.state,
        SessionState::Ended {
            reason: "denied".into()
        }
    );
    assert_eq!(
        host.starts.load(Ordering::SeqCst),
        0,
        "no video was ever produced"
    );
}

#[tokio::test]
async fn click_mode_accept_then_disconnect_stops_video_immediately() {
    let host = start(ApproveMode::Click);
    let key = host.key;
    let addr = host.addr;
    let viewer = tokio::spawn(async move { connect(addr, key, Credentials::AskHost).await });
    let info = host.cm.wait_for(awaiting).await;
    // Accept with the keyboard off.
    host.cm.decide(
        info.id,
        Decision::Accept(Perms {
            keyboard: false,
            mouse: true,
        }),
    );
    let mut v = viewer.await.unwrap().unwrap();
    assert_eq!(v.password, None, "accepted without a password");
    assert_eq!(v.waits, 1, "told to wait");
    let (_, perms) = read_welcome(&mut v.control).await;
    assert_eq!(
        perms,
        Permissions {
            keyboard: false,
            mouse: true
        }
    );
    let IncomingStream::Video { mut reader, .. } = streams::accept_uni(&v.conn).await.unwrap();
    reader.next().await.unwrap().unwrap();
    assert_eq!(host.engine.sessions()[0].state, SessionState::Active);

    // The host's user disconnects it: video stops at once.
    let t = std::time::Instant::now();
    assert!(
        host.engine
            .disconnect(info.id, "disconnected by the host's user")
    );
    while let Ok(Some(_)) = reader.next().await {}
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "video ended {:?} after the disconnect",
        t.elapsed()
    );
    let ended = host.cm.wait_for(ended).await;
    assert_eq!(
        ended.state,
        SessionState::Ended {
            reason: "disconnected by the host's user".into()
        }
    );
    producers_stop(&host).await;
    assert!(!host.engine.disconnect(info.id, "again"), "already gone");
}

#[tokio::test]
async fn permissions_change_live() {
    let host = start(ApproveMode::Password);
    let password = host.engine.temporary_password();
    let mut v = connect(host.addr, host.key, Credentials::Password(&password))
        .await
        .unwrap();
    let _ = read_welcome(&mut v.control).await;
    let info = host.cm.wait_for(active).await;
    let mut input = streams::open_input(&v.conn).await.unwrap();
    let a = |down| InputEvent::key(KeyCode(0x04), down, Modifiers::NONE);
    input.send(&a(true).to_proto(0)).await.unwrap();
    input.send(&a(false).to_proto(1)).await.unwrap();
    for _ in 0..200 {
        if host.sink.calls().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        host.sink.calls(),
        vec![
            SinkCall::Key(KeyCode(0x04), true),
            SinkCall::Key(KeyCode(0x04), false)
        ]
    );

    // The host's user turns the keyboard off: the viewer is told, the CM
    // sees it, and keys are dropped.
    assert!(host.engine.set_permissions(
        info.id,
        Permissions {
            keyboard: false,
            mouse: true
        }
    ));
    let mut told = Permissions::ALL;
    for _ in 0..2 {
        if let Some(control_message::Msg::Permission(p)) =
            v.control.recv().await.unwrap().and_then(|m| m.msg)
        {
            told = told.with(&p);
            if p.kind() == PermissionKind::Keyboard {
                assert!(!p.enabled);
            }
        }
    }
    assert!(!told.keyboard && told.mouse);
    host.cm
        .wait_for(|e| e.id == info.id && !e.permissions.keyboard)
        .await;
    input.send(&a(true).to_proto(2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(host.sink.calls().len(), 2, "no key after revoking");
    assert!(!host.engine.set_permissions(999, Permissions::NONE));
}

#[tokio::test]
async fn both_mode_checks_the_password_before_asking() {
    let host = start(ApproveMode::Both);
    let error = connect(host.addr, host.key, Credentials::Password("wrong-guess"))
        .await
        .err();
    assert!(matches!(
        error,
        Some(SessionError::Rejected {
            error: LoginError::WrongPassword,
            ..
        })
    ));
    let error = connect(host.addr, host.key, Credentials::AskHost)
        .await
        .err();
    assert!(matches!(
        error,
        Some(SessionError::Rejected {
            error: LoginError::PasswordRequired,
            ..
        })
    ));
    assert_eq!(host.cm.approvals_asked(), 0, "nobody was bothered");

    // Right password, then the click.
    let password = host.engine.temporary_password();
    let key = host.key;
    let addr = host.addr;
    let viewer =
        tokio::spawn(async move { connect(addr, key, Credentials::Password(&password)).await });
    let info = host.cm.wait_for(awaiting).await;
    assert_eq!(info.password.as_deref(), Some("temporary"));
    host.cm.decide(info.id, Decision::Accept(info.permissions));
    let v = viewer.await.unwrap().unwrap();
    assert_eq!(v.password, Some(PasswordKind::Temporary));
}

#[tokio::test]
async fn a_viewer_that_gives_up_waiting_is_removed() {
    let host = start(ApproveMode::Click);
    let key = host.key;
    let addr = host.addr;
    let viewer = tokio::spawn(async move { connect(addr, key, Credentials::AskHost).await });
    let info = host.cm.wait_for(awaiting).await;
    viewer.abort();
    let _ = viewer.await;
    let ended = host.cm.wait_for(|e| e.id == info.id && ended(e)).await;
    assert!(matches!(ended.state, SessionState::Ended { .. }));
    assert!(host.engine.sessions().is_empty());
}
