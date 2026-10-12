//! End to end over QUIC: viewer events → `ViewerInput` → coalesced input
//! stream → host `serve_input` → `Injector` (with a recording sink instead
//! of the OS).

use std::net::SocketAddr;
use std::time::Duration;

use kanodesk_client::input::{SendOptions, ViewMapping, ViewerInput, send_input};
use kanodesk_host::input::{HOLD_TIMEOUT, serve_input};
use kanodesk_input::{
    DisplayRect, Injector, InputEvent, KeyCode, MouseButton, Permissions, RecordingSink, SinkCall,
};
use kanodesk_transport::endpoint;
use kanodesk_transport::streams;
use kanodesk_transport::tls::{Identity, ServerTrust};
use tokio::sync::{mpsc, watch};
use winit::keyboard::{KeyCode as Wk, PhysicalKey};

fn localhost() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// A host whose display 1 sits left of the primary, 3 px lower.
fn displays() -> Vec<DisplayRect> {
    vec![
        DisplayRect {
            id: 0,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        },
        DisplayRect {
            id: 1,
            x: -1920,
            y: 3,
            width: 1920,
            height: 1080,
        },
    ]
}

struct Host {
    addr: SocketAddr,
    fingerprint: kanodesk_transport::tls::Fingerprint,
    sink: RecordingSink,
    done: tokio::task::JoinHandle<Result<kanodesk_host::input::InputStats, String>>,
    permissions: watch::Sender<Permissions>,
}

fn start_host(allowed: Permissions) -> Host {
    start_host_with(allowed, HOLD_TIMEOUT)
}

fn start_host_with(allowed: Permissions, hold_timeout: Duration) -> Host {
    let identity = Identity::self_signed().unwrap();
    let server = endpoint::server(localhost(), &identity).unwrap();
    let addr = server.local_addr().unwrap();
    let sink = RecordingSink::default();
    let (permissions, mut rx) = watch::channel(allowed);
    let host_sink = sink.clone();
    let done = tokio::spawn(async move {
        let conn = endpoint::accept(&server).await.unwrap().unwrap();
        let mut reader = streams::accept_input(&conn)
            .await
            .map_err(|e| e.to_string())?;
        let mut injector = Injector::new(host_sink, displays(), Permissions::NONE);
        let stats = serve_input(&mut reader, &mut injector, &mut rx, hold_timeout)
            .await
            .map_err(|e| e.to_string());
        // Keep the server alive until the stream is done.
        drop(server);
        stats
    });
    Host {
        addr,
        fingerprint: identity.fingerprint(),
        sink,
        done,
        permissions,
    }
}

#[tokio::test]
async fn viewer_input_reaches_the_host_display() {
    let host = start_host(Permissions::ALL);
    let client = endpoint::client(localhost(), ServerTrust::Pinned(host.fingerprint)).unwrap();
    let conn = endpoint::connect(&client, host.addr).await.unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let sender = {
        let conn = conn.clone();
        let options = SendOptions {
            move_interval: Duration::from_millis(16),
            ..SendOptions::default()
        };
        tokio::spawn(async move { send_input(&conn, rx, options).await })
    };

    // Half-size window showing host display 1.
    let mut viewer = ViewerInput::new();
    viewer.set_permissions(Permissions::ALL);
    viewer.set_view(
        1,
        ViewMapping {
            window: (960, 540),
            video: (1920, 1080),
            display: (1920, 1080),
        },
    );
    // A burst of pointer moves, faster than one per frame.
    for i in 0..100 {
        if let Some(e) = viewer.cursor_moved((100.0 + i as f64, 50.0)) {
            tx.send(e).unwrap();
        }
    }
    // Click, type Shift+A, and a media key.
    let mut events: Vec<InputEvent> = [
        viewer.mouse_button(winit::event::MouseButton::Left, true),
        viewer.mouse_button(winit::event::MouseButton::Left, false),
    ]
    .into_iter()
    .flatten()
    .collect();
    for (key, down) in [
        (Wk::ShiftLeft, true),
        (Wk::KeyA, true),
        (Wk::KeyA, false),
        (Wk::ShiftLeft, false),
        (Wk::AudioVolumeUp, true),
        (Wk::AudioVolumeUp, false),
    ] {
        events.extend(viewer.key(PhysicalKey::Code(key), down, None, None).events);
    }
    for e in events {
        tx.send(e).unwrap();
    }
    drop(tx);
    let sent = sender.await.unwrap().unwrap();
    let stats = host.done.await.unwrap().unwrap();

    let calls = host.sink.calls();
    let moves: Vec<_> = calls
        .iter()
        .filter(|c| matches!(c, SinkCall::Move(..)))
        .collect();
    assert!(
        moves.len() < 10,
        "{} moves reached the host; moves must be coalesced",
        moves.len()
    );
    // Window (199, 50) at half size = display 1 pixel (398, 100), placed on
    // the virtual desktop at (-1920 + 398, 3 + 100).
    let rest: Vec<_> = calls
        .iter()
        .skip_while(|c| matches!(c, SinkCall::Move(..)))
        .cloned()
        .collect();
    assert_eq!(
        **moves.last().unwrap(),
        SinkCall::Move(-1920 + 398, 103),
        "the latest position is the one that counts"
    );
    assert_eq!(
        rest,
        vec![
            SinkCall::Button(MouseButton::Left, true),
            SinkCall::Button(MouseButton::Left, false),
            SinkCall::Key(KeyCode(0xE1), true),
            SinkCall::Key(KeyCode(0x04), true),
            SinkCall::Key(KeyCode(0x04), false),
            SinkCall::Key(KeyCode(0xE1), false),
            SinkCall::Key(KeyCode::consumer(0xE9), true),
            SinkCall::Key(KeyCode::consumer(0xE9), false),
        ]
    );
    assert_eq!(stats.injected, sent);
    assert_eq!(stats.dropped + stats.failed, 0);
    endpoint::close(&conn, b"done");
}

#[tokio::test]
async fn keys_held_when_the_viewer_vanishes_are_released() {
    let host = start_host(Permissions::ALL);
    let client = endpoint::client(localhost(), ServerTrust::Pinned(host.fingerprint)).unwrap();
    let conn = endpoint::connect(&client, host.addr).await.unwrap();
    let mut writer = streams::open_input(&conn).await.unwrap();
    // Ctrl and Alt down, a button down, then the viewer dies: no releases.
    let held = [
        InputEvent::key(KeyCode(0xE0), true, kanodesk_input::Modifiers::CONTROL),
        InputEvent::key(
            KeyCode(0xE2),
            true,
            kanodesk_input::Modifiers::CONTROL | kanodesk_input::Modifiers::ALT,
        ),
        InputEvent::MouseButton {
            button: MouseButton::Right,
            down: true,
        },
    ];
    for (seq, e) in held.iter().enumerate() {
        writer.send(&e.to_proto(seq as u64)).await.unwrap();
    }
    // Wait until the host has applied them, then drop the connection hard.
    while host.sink.calls().len() < 3 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    endpoint::close(&conn, b"crashed");
    let result = tokio::time::timeout(Duration::from_secs(5), host.done)
        .await
        .expect("host noticed the close")
        .unwrap();
    assert!(result.is_err(), "the stream broke rather than ended");
    let calls = host.sink.calls();
    assert_eq!(
        &calls[3..],
        &[
            SinkCall::Key(KeyCode(0xE0), false),
            SinkCall::Key(KeyCode(0xE2), false),
            SinkCall::Button(MouseButton::Right, false),
        ]
    );
}

#[tokio::test]
async fn long_holds_survive_and_a_frozen_viewer_is_released() {
    let hold_timeout = Duration::from_millis(300);
    let host = start_host_with(Permissions::ALL, hold_timeout);
    let client = endpoint::client(localhost(), ServerTrust::Pinned(host.fingerprint)).unwrap();
    let conn = endpoint::connect(&client, host.addr).await.unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let options = SendOptions {
        keep_alive: Duration::from_millis(100),
        ..SendOptions::default()
    };
    let sender = {
        let conn = conn.clone();
        tokio::spawn(async move { send_input(&conn, rx, options).await })
    };
    // Shift held for 4 hold-timeouts: the sender's keep-alives keep it held.
    let shift = |down| InputEvent::key(KeyCode(0xE1), down, kanodesk_input::Modifiers::NONE);
    tx.send(shift(true)).unwrap();
    tokio::time::sleep(hold_timeout * 4).await;
    assert_eq!(host.sink.calls(), vec![SinkCall::Key(KeyCode(0xE1), true)]);
    // The viewer freezes (its sender stops, the connection stays up).
    sender.abort();
    tokio::time::sleep(hold_timeout * 3).await;
    assert_eq!(
        host.sink.calls(),
        vec![
            SinkCall::Key(KeyCode(0xE1), true),
            SinkCall::Key(KeyCode(0xE1), false),
        ],
        "released well before QUIC's idle timeout"
    );
    drop(tx);
    endpoint::close(&conn, b"done");
}

#[tokio::test]
async fn host_permission_flag_is_respected_live() {
    let host = start_host(Permissions::NONE);
    let client = endpoint::client(localhost(), ServerTrust::Pinned(host.fingerprint)).unwrap();
    let conn = endpoint::connect(&client, host.addr).await.unwrap();
    let mut writer = streams::open_input(&conn).await.unwrap();
    let a = |down| InputEvent::key(KeyCode(0x04), down, kanodesk_input::Modifiers::NONE);
    // View only: dropped.
    writer.send(&a(true).to_proto(0)).await.unwrap();
    writer.send(&a(false).to_proto(1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(host.sink.calls().is_empty());
    // Allowed: applied.
    host.permissions.send(Permissions::ALL).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    writer.send(&a(true).to_proto(2)).await.unwrap();
    while host.sink.calls().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Revoked while A is held: released at once.
    host.permissions.send(Permissions::NONE).unwrap();
    while host.sink.calls().len() < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        host.sink.calls(),
        vec![
            SinkCall::Key(KeyCode(0x04), true),
            SinkCall::Key(KeyCode(0x04), false),
        ]
    );
    writer.into_inner().finish().unwrap();
    let stats = host.done.await.unwrap().unwrap();
    assert_eq!(stats.injected, 1);
    assert_eq!(stats.dropped, 2);
}
