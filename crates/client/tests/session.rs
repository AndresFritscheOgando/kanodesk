//! The viewer's session engine end to end on loopback: an ID server, a
//! host (engine + presence, two displays with real VP9), and a viewer that
//! connects by ID. Covers the password prompts, login, displays, video,
//! display switches, quality presets, closing, and the errors a user sees
//! for unknown and offline IDs.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kanodesk_client::connect::ConnectOptions;
use kanodesk_client::session::{
    EndReason, PasswordPrompt, Session, SessionConfig, SessionEvent, Stage, Target,
};
use kanodesk_codec::{Encoder, EncoderConfig, I420Frame, Vp9Encoder};
use kanodesk_config::ApproveMode;
use kanodesk_crypto::DeviceIdentity;
use kanodesk_crypto::password::{HostPasswords, TemporaryPolicy};
use kanodesk_crypto::throttle::{LoginThrottle, ThrottleConfig};
use kanodesk_host::cm::{ApprovalFuture, ConnectionManager, Decision};
use kanodesk_host::engine::{DisplayList, EngineParts, EngineSettings, HostEngine};
use kanodesk_host::incoming::HostOptions;
use kanodesk_host::presence::{Presence, PresenceConfig};
use kanodesk_host::video::{Producer, ProducerFn, VideoHub};
use kanodesk_idserver::{IdServer, ServerConfig};
use kanodesk_input::{InputEvent, Permissions, RecordingSink};
use kanodesk_ipc::cm::{SessionInfo, SessionState};
use kanodesk_ipc::control::IdServerState;
use kanodesk_proto::v1::{self, ImageQuality};
use kanodesk_rendezvous::DeviceId;
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let mut name = [0u8; 8];
    getrandom::fill(&mut name).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "kanodesk-{tag}-{}",
        name.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn idserver() -> IdServer {
    let dir = temp_dir("session-ids");
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), dir.join("ids.sqlite"));
    config.nat_test_listen = Some("127.0.0.1:0".parse().unwrap());
    kanodesk_idserver::start(config, DeviceIdentity::generate().unwrap())
        .await
        .unwrap()
}

/// Display 0 is 64×32, display 1 is 32×64: the decoded size tells which
/// one a frame shows. The last quality asked for is recorded.
fn vp9_producer(quality: Arc<AtomicI32>) -> ProducerFn {
    Arc::new(move |display, out: &Producer| {
        let (w, h) = if display == 0 { (64, 32) } else { (32, 64) };
        let mut enc = Vp9Encoder::new(EncoderConfig::new(w, h)).map_err(|e| e.to_string())?;
        out.send(
            v1::StreamStart {
                display,
                codec: v1::Codec::Vp9 as i32,
                width: w,
                height: h,
            }
            .into(),
        );
        let picture = I420Frame::new(w, h);
        let mut seq = 0u64;
        loop {
            quality.store(out.quality() as i32, Ordering::SeqCst);
            let key = out.take_keyframe_request();
            for p in enc
                .encode(&picture, seq as i64, key)
                .map_err(|e| e.to_string())?
            {
                let frame = v1::VideoFrame {
                    display,
                    seq,
                    key: p.key,
                    data: p.data,
                    ..Default::default()
                };
                seq += 1;
                if !out.send(frame.into()) {
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })
}

/// Accepts everyone (like `AutoAccept`) and remembers why sessions ended.
#[derive(Default)]
struct RecordingCm {
    ended: Mutex<Vec<String>>,
}

impl ConnectionManager for RecordingCm {
    fn session_changed(&self, info: &SessionInfo) {
        if let SessionState::Ended { reason } = &info.state {
            self.ended.lock().unwrap().push(reason.clone());
        }
    }

    fn request_approval(&self, info: &SessionInfo) -> ApprovalFuture {
        let p = info.permissions;
        Box::pin(async move { Some(Decision::Accept(p)) })
    }
}

struct Host {
    cm: Arc<RecordingCm>,
    engine: HostEngine,
    _presence: Presence,
    id: DeviceId,
    quality: Arc<AtomicI32>,
}

async fn host(ids: &IdServer, mode: ApproveMode) -> Host {
    let identity_seed = {
        let mut s = [0u8; 32];
        getrandom::fill(&mut s).unwrap();
        s
    };
    let displays: DisplayList = Arc::new(|| {
        Ok(vec![
            v1::DisplayInfo {
                id: 0,
                width: 64,
                height: 32,
                primary: true,
                ..Default::default()
            },
            v1::DisplayInfo {
                id: 1,
                x: 64,
                width: 32,
                height: 64,
                ..Default::default()
            },
        ])
    });
    let quality = Arc::new(AtomicI32::new(0));
    let cm = Arc::new(RecordingCm::default());
    let sink = RecordingSink::default();
    let engine = HostEngine::new(EngineParts {
        identity: DeviceIdentity::from_seed(&identity_seed),
        passwords: HostPasswords::new(TemporaryPolicy::default()).unwrap(),
        throttle: LoginThrottle::new(ThrottleConfig::default()),
        throttle_path: None,
        settings: EngineSettings {
            approve_mode: mode,
            approve_timeout: Duration::from_secs(10),
            click_delay: Duration::ZERO,
            permissions: Permissions::ALL,
            display: None,
            agent: "session-test-host".into(),
            listen: None,
        },
        cm: cm.clone(),
        video: VideoHub::new(vp9_producer(quality.clone())),
        displays,
        input: Arc::new(move || Ok(Box::new(sink.clone()))),
    });
    let dir = temp_dir("session-host");
    let presence = Presence::start(
        engine.clone(),
        DeviceIdentity::from_seed(&identity_seed),
        PresenceConfig {
            server: ids.local_addr().unwrap().to_string(),
            server_key: ids.public_key(),
            nat_test: ids.nat_test_addr().map(|a| a.to_string()),
            port: 0,
            id_file: Some(dir.join("device-id")),
            options: HostOptions::default(),
        },
    );
    let mut status = presence.status();
    let id = loop {
        {
            let s = status.borrow_and_update();
            if s.state == IdServerState::Online {
                break s.id.unwrap();
            }
        }
        tokio::time::timeout(Duration::from_secs(10), status.changed())
            .await
            .expect("host registers")
            .unwrap();
    };
    assert_eq!(
        std::fs::read_to_string(dir.join("device-id")).unwrap(),
        id.to_string(),
        "the ID is remembered"
    );
    Host {
        cm,
        engine,
        _presence: presence,
        id,
        quality,
    }
}

impl Host {
    fn cm_reasons(&self) -> Vec<String> {
        self.cm.ended.lock().unwrap().clone()
    }
}

struct Viewer {
    session: Session,
    events: mpsc::UnboundedReceiver<SessionEvent>,
    seen: Arc<Mutex<Vec<SessionEvent>>>,
}

fn viewer(ids: &IdServer, target: DeviceId, password: Option<&str>) -> Viewer {
    let dir = temp_dir("session-viewer");
    let (tx, events) = mpsc::unbounded_channel();
    let seen: Arc<Mutex<Vec<SessionEvent>>> = Arc::default();
    let log = seen.clone();
    let session = Session::start(
        SessionConfig {
            target: Target::Id {
                id: target,
                server: ids.local_addr().unwrap().to_string(),
                server_key: ids.public_key(),
                nat_test: ids.nat_test_addr().map(|a| a.to_string()),
                id_file: Some(dir.join("device-id")),
            },
            identity: DeviceIdentity::generate().unwrap(),
            agent: "session-test-viewer".into(),
            options: ConnectOptions::default(),
            quality: Some(ImageQuality::Low),
            password: password.map(str::to_owned),
        },
        move |e| {
            if e != SessionEvent::FrameReady {
                log.lock().unwrap().push(e.clone());
            }
            let _ = tx.send(e);
        },
    );
    Viewer {
        session,
        events,
        seen,
    }
}

impl Viewer {
    async fn wait(&mut self, what: &str, pred: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(e)) if pred(&e) => return e,
                Ok(Some(SessionEvent::Ended(r))) => panic!("ended with {r:?} waiting for {what}"),
                Ok(Some(_)) => {}
                Ok(None) => panic!("events closed waiting for {what}"),
                Err(_) => panic!(
                    "timed out waiting for {what}; saw {:?}",
                    self.seen.lock().unwrap()
                ),
            }
        }
    }

    /// Like [`Self::wait`], but also finds events already received (for
    /// events from independent streams, whose order varies).
    async fn saw(&mut self, what: &str, pred: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(e) = self.seen.lock().unwrap().iter().find(|e| pred(e)) {
                return e.clone();
            }
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(SessionEvent::Ended(r))) => panic!("ended with {r:?} waiting for {what}"),
                Ok(Some(_)) => {}
                _ => panic!("no {what}; saw {:?}", self.seen.lock().unwrap()),
            }
        }
    }

    async fn ended(&mut self) -> EndReason {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(SessionEvent::Ended(r))) => return r,
                Ok(Some(_)) => {}
                other => panic!("no end: {other:?}"),
            }
        }
    }

    /// Waits for a decoded frame of this width.
    async fn frame_of_width(&mut self, width: u32) {
        for _ in 0..200 {
            self.wait("a frame", |e| *e == SessionEvent::FrameReady)
                .await;
            if let Some(f) = self.session.take_frame()
                && f.frame.width() == width
            {
                return;
            }
        }
        panic!("no frame {width} wide");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn password_prompts_login_video_display_switch_quality_and_close() {
    let ids = idserver().await;
    let host = host(&ids, ApproveMode::Password).await;
    let mut v = viewer(&ids, host.id, None);

    // No password first: the host asks for one.
    v.wait("connecting", |e| {
        *e == SessionEvent::Stage(Stage::Connecting)
    })
    .await;
    v.wait("path", |e| matches!(e, SessionEvent::Connected(Some(_))))
        .await;
    v.wait("prompt", |e| {
        *e == SessionEvent::PasswordNeeded(PasswordPrompt::Required)
    })
    .await;
    v.session.send_password(Some("wrong-one".into()));
    v.wait("wrong", |e| {
        *e == SessionEvent::PasswordNeeded(PasswordPrompt::Wrong)
    })
    .await;
    v.session
        .send_password(Some(host.engine.temporary_password()));
    let SessionEvent::LoggedIn(remote) = v
        .wait("login", |e| matches!(e, SessionEvent::LoggedIn(_)))
        .await
    else {
        unreachable!()
    };
    assert_eq!(remote.agent, "session-test-host");
    assert!(remote.multi_display && remote.image_quality);
    assert_eq!(
        remote.password,
        Some(kanodesk_crypto::PasswordKind::Temporary)
    );

    let SessionEvent::Displays(d) = v
        .saw("displays", |e| matches!(e, SessionEvent::Displays(_)))
        .await
    else {
        unreachable!()
    };
    assert_eq!(d.len(), 2);
    v.saw("permissions", |e| {
        *e == SessionEvent::Permissions(Permissions::ALL)
    })
    .await;
    v.saw("video 0", |e| *e == SessionEvent::VideoStarted(0))
        .await;
    v.frame_of_width(64).await;
    // The quality asked for in the config reached the display's encoder.
    for _ in 0..100 {
        if host.quality.load(Ordering::SeqCst) == ImageQuality::Low as i32 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        host.quality.load(Ordering::SeqCst),
        ImageQuality::Low as i32
    );

    // Switch displays: a new stream, frames of the other size.
    v.session.select_display(1);
    v.wait("video 1", |e| *e == SessionEvent::VideoStarted(1))
        .await;
    v.frame_of_width(32).await;
    // The viewer's quality follows it to the new display.
    v.session.set_quality(ImageQuality::Best);
    for _ in 0..100 {
        if host.quality.load(Ordering::SeqCst) == ImageQuality::Best as i32 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        host.quality.load(Ordering::SeqCst),
        ImageQuality::Best as i32
    );
    assert_eq!(host.engine.sessions().len(), 1);

    // Input goes through; closing ends the host's session too.
    v.session.input(InputEvent::MouseMove {
        display: 1,
        x: 3,
        y: 4,
    });
    let Viewer {
        session,
        mut events,
        ..
    } = v;
    let closing = tokio::task::spawn_blocking(move || session.close());
    let mut end = None;
    while let Some(e) = events.recv().await {
        if let SessionEvent::Ended(r) = e {
            end = Some(r);
        }
    }
    closing.await.unwrap();
    assert_eq!(end, Some(EndReason::Closed));
    for _ in 0..100 {
        if host.engine.sessions().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(host.engine.sessions().is_empty(), "the host saw the close");
    assert!(
        host.cm_reasons()
            .iter()
            .any(|r| r == "the viewer closed the session"),
        "a clean close, not a lost connection: {:?}",
        host.cm_reasons()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_password_given_up_front_logs_in_directly() {
    let ids = idserver().await;
    let host = host(&ids, ApproveMode::Password).await;
    let mut v = viewer(&ids, host.id, Some(&host.engine.temporary_password()));
    v.wait("login", |e| matches!(e, SessionEvent::LoggedIn(_)))
        .await;
    assert!(
        !v.seen
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, SessionEvent::PasswordNeeded(_))),
        "never asked"
    );
    // Giving up a prompt ends the session as closed.
    let mut w = viewer(&ids, host.id, None);
    w.wait("prompt", |e| matches!(e, SessionEvent::PasswordNeeded(_)))
        .await;
    w.session.send_password(None);
    assert_eq!(w.ended().await, EndReason::Closed);
}

#[tokio::test(flavor = "multi_thread")]
async fn click_mode_waits_for_the_host_user_without_a_password() {
    let ids = idserver().await;
    // AutoAccept approves at once, but the viewer is still told to wait.
    let host = host(&ids, ApproveMode::Click).await;
    let mut v = viewer(&ids, host.id, None);
    v.wait("waiting", |e| {
        *e == SessionEvent::Stage(Stage::WaitingForApproval)
    })
    .await;
    let SessionEvent::LoggedIn(remote) = v
        .wait("login", |e| matches!(e, SessionEvent::LoggedIn(_)))
        .await
    else {
        unreachable!()
    };
    assert_eq!(remote.password, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_and_offline_ids_and_a_wrong_server_key_are_reported() {
    let ids = idserver().await;
    let mut v = viewer(&ids, "999999999".parse().unwrap(), None);
    assert_eq!(v.ended().await, EndReason::UnknownId);

    // A viewer asking for its own ID gets told so (its ID file says which).
    let dir = temp_dir("own-id");
    let identity_seed = [7u8; 32];
    let own = |target| {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let s = Session::start(
            SessionConfig {
                target: Target::Id {
                    id: target,
                    server: ids.local_addr().unwrap().to_string(),
                    server_key: ids.public_key(),
                    nat_test: ids.nat_test_addr().map(|a| a.to_string()),
                    id_file: Some(dir.join("device-id")),
                },
                identity: DeviceIdentity::from_seed(&identity_seed),
                agent: "own".into(),
                options: ConnectOptions::default(),
                quality: None,
                password: None,
            },
            move |e| {
                let _ = tx.send(e);
            },
        );
        async move {
            let _keep = s;
            loop {
                if let Some(SessionEvent::Ended(r)) =
                    tokio::time::timeout(Duration::from_secs(20), rx.recv())
                        .await
                        .unwrap()
                {
                    return r;
                }
            }
        }
    };
    // First run: registers (the target does not exist).
    assert_eq!(
        own("999999998".parse().unwrap()).await,
        EndReason::UnknownId
    );
    let mine: DeviceId = std::fs::read_to_string(dir.join("device-id"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(own(mine).await, EndReason::OwnId);

    // A host that registered and went away.
    let gone = host(&ids, ApproveMode::Password).await;
    let id = gone.id;
    drop(gone);
    let mut ended = None;
    for _ in 0..50 {
        let mut v = viewer(&ids, id, None);
        let r = v.ended().await;
        if r == EndReason::Offline {
            ended = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(ended, Some(EndReason::Offline));

    // The pinned key is not the server's.
    let (tx, mut events) = mpsc::unbounded_channel();
    let _s = Session::start(
        SessionConfig {
            target: Target::Id {
                id,
                server: ids.local_addr().unwrap().to_string(),
                server_key: DeviceIdentity::generate().unwrap().public_key(),
                nat_test: None,
                id_file: None,
            },
            identity: DeviceIdentity::generate().unwrap(),
            agent: "x".into(),
            options: ConnectOptions::default(),
            quality: None,
            password: None,
        },
        move |e| {
            let _ = tx.send(e);
        },
    );
    let end = loop {
        match tokio::time::timeout(Duration::from_secs(20), events.recv())
            .await
            .unwrap()
        {
            Some(SessionEvent::Ended(r)) => break r,
            Some(_) => {}
            None => panic!("no end"),
        }
    };
    assert_eq!(end, EndReason::ServerNotTrusted);
}
