//! The input service: applies a logged-in controller's input stream (07).
//!
//! [`serve_input`] reads `InputMessage`s and hands them to an
//! [`Injector`], which enforces the keyboard/mouse permissions, maps display
//! positions onto the virtual desktop and tracks what is held. Permission
//! changes arrive on a `watch` channel and take effect at once (revoking
//! releases what the controller holds). When the stream ends, for whatever
//! reason, everything still held is released, so no key stays stuck.
//!
//! A viewer that holds something sends a keep-alive every second. If the
//! host holds keys or buttons for a viewer and hears nothing for
//! [`HOLD_TIMEOUT`], it releases them: the viewer is gone or cut off, and
//! QUIC would only notice at its idle timeout (30 s) — far too long for a
//! stuck Ctrl.

use std::time::Duration;

use kanodesk_input::event::is_keep_alive;
use kanodesk_input::{Applied, Injector, InputEvent, InputSink, Permissions};
use kanodesk_proto::MessageReader;
use kanodesk_proto::v1::InputMessage;
use kanodesk_transport::TransportError;
use tokio::io::AsyncRead;
use tokio::sync::watch;
use tokio::time::Instant;

/// How long the host keeps keys or buttons held without hearing from the viewer.
pub const HOLD_TIMEOUT: Duration = Duration::from_secs(3);

/// What happened to a session's input.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InputStats {
    /// Events passed to the OS.
    pub injected: u64,
    /// Events dropped: not allowed, unknown display, release without press,
    /// unsupported, malformed, or out of order.
    pub dropped: u64,
    /// Events the OS refused.
    pub failed: u64,
    /// Times everything held was released because the viewer went quiet.
    pub quiet_releases: u64,
}

/// Applies the input stream (`kanodesk_transport::streams::InputReader`, or
/// any other reader of `InputMessage`s) until it ends, then releases
/// everything held. `permissions` is read at the start and followed while
/// the stream lasts. Held keys and buttons are also released after
/// `hold_timeout` without any message ([`HOLD_TIMEOUT`] normally).
pub async fn serve_input<R: AsyncRead + Unpin, S: InputSink>(
    reader: &mut MessageReader<R, InputMessage>,
    injector: &mut Injector<S>,
    permissions: &mut watch::Receiver<Permissions>,
    hold_timeout: Duration,
) -> Result<InputStats, TransportError> {
    let mut stats = InputStats::default();
    let initial = *permissions.borrow_and_update();
    if let Err(e) = injector.set_permissions(initial) {
        tracing::warn!("input: applying permissions: {e}");
    }
    let mut watching = true;
    let mut last_seq: Option<u64> = None;
    let mut heard = Instant::now();
    let result = loop {
        let holding = injector.anything_held();
        tokio::select! {
            msg = reader.next() => match msg {
                Ok(Some(msg)) => {
                    heard = Instant::now();
                    // Sequence numbers only grow; anything else is stale.
                    if last_seq.is_some_and(|last| msg.seq <= last) {
                        stats.dropped += 1;
                        continue;
                    }
                    last_seq = Some(msg.seq);
                    if is_keep_alive(&msg) {
                        continue;
                    }
                    let event = match InputEvent::from_proto(&msg) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::debug!("input: {e}");
                            stats.dropped += 1;
                            continue;
                        }
                    };
                    match injector.apply(&event) {
                        Ok(Applied::Injected) => stats.injected += 1,
                        Ok(other) => {
                            tracing::trace!("input: {event:?} dropped: {other:?}");
                            stats.dropped += 1;
                        }
                        Err(e) => {
                            tracing::warn!("input: {event:?}: {e}");
                            stats.failed += 1;
                        }
                    }
                }
                Ok(None) => break Ok(stats),
                Err(e) => break Err(TransportError::from(e)),
            },
            _ = tokio::time::sleep_until(heard + hold_timeout), if holding => {
                tracing::warn!("input: viewer quiet for {hold_timeout:?} with keys held; releasing them");
                stats.quiet_releases += 1;
                if let Err(e) = injector.release_all() {
                    tracing::warn!("input: releasing held keys: {e}");
                }
            }
            changed = permissions.changed(), if watching => match changed {
                Ok(()) => {
                    let now = *permissions.borrow_and_update();
                    tracing::info!("input permissions: keyboard {}, mouse {}", now.keyboard, now.mouse);
                    if let Err(e) = injector.set_permissions(now) {
                        tracing::warn!("input: applying permissions: {e}");
                    }
                }
                // Nobody can change them any more; keep the last value.
                Err(_) => watching = false,
            },
        }
    };
    if let Err(e) = injector.release_all() {
        tracing::warn!("input: releasing held keys: {e}");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanodesk_input::{DisplayRect, KeyCode, Modifiers, RecordingSink, SinkCall};
    use kanodesk_proto::MessageWriter;
    use tokio::io::{DuplexStream, duplex};

    type Writer = MessageWriter<DuplexStream, InputMessage>;
    type Reader = MessageReader<DuplexStream, InputMessage>;

    // An in-memory pipe stands in for the QUIC stream (the real one is
    // tested end to end in kanodesk-client's tests).
    fn pipe() -> (Writer, Reader) {
        let (w, r) = duplex(64 * 1024);
        (MessageWriter::new(w), MessageReader::new(r))
    }

    fn key(code: u32, down: bool) -> InputEvent {
        InputEvent::key(KeyCode(code), down, Modifiers::NONE)
    }

    fn injector() -> (Injector<RecordingSink>, RecordingSink) {
        let sink = RecordingSink::default();
        let displays = vec![DisplayRect {
            id: 0,
            x: 100,
            y: 0,
            width: 800,
            height: 600,
        }];
        (
            Injector::new(sink.clone(), displays, Permissions::NONE),
            sink,
        )
    }

    #[tokio::test]
    async fn events_are_applied_and_held_keys_released_at_the_end() {
        let (mut w, mut r) = pipe();
        let (mut inj, sink) = injector();
        let (_tx, mut rx) = watch::channel(Permissions::ALL);
        let events = [
            InputEvent::MouseMove {
                display: 0,
                x: 5,
                y: 6,
            },
            key(0x04, true),
        ];
        for (seq, e) in events.iter().enumerate() {
            w.send(&e.to_proto(seq as u64)).await.unwrap();
        }
        // A stale (repeated) sequence number is dropped.
        w.send(&key(0x05, true).to_proto(1)).await.unwrap();
        drop(w);
        let stats = serve_input(&mut r, &mut inj, &mut rx, HOLD_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(
            stats,
            InputStats {
                injected: 2,
                dropped: 1,
                failed: 0,
                quiet_releases: 0,
            }
        );
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Move(105, 6),
                SinkCall::Key(KeyCode(0x04), true),
                SinkCall::Key(KeyCode(0x04), false),
            ]
        );
    }

    #[tokio::test]
    async fn revoking_mid_stream_releases_and_blocks() {
        let (mut w, mut r) = pipe();
        let (mut inj, sink) = injector();
        let (tx, mut rx) = watch::channel(Permissions::ALL);
        let task = tokio::spawn(async move {
            let stats = serve_input(&mut r, &mut inj, &mut rx, HOLD_TIMEOUT)
                .await
                .unwrap();
            (stats, inj.held_keys().count())
        });
        w.send(&key(0x04, true).to_proto(0)).await.unwrap();
        // Wait until the press has been applied before revoking.
        while sink.calls().is_empty() {
            tokio::task::yield_now().await;
        }
        tx.send(Permissions::NONE).unwrap();
        while sink.calls().len() < 2 {
            tokio::task::yield_now().await;
        }
        w.send(&key(0x05, true).to_proto(1)).await.unwrap();
        drop(w);
        let (stats, held) = task.await.unwrap();
        assert_eq!(held, 0);
        assert_eq!(stats.injected, 1);
        assert_eq!(stats.dropped, 1);
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Key(KeyCode(0x04), true),
                SinkCall::Key(KeyCode(0x04), false),
            ]
        );
    }

    #[tokio::test]
    async fn view_only_from_the_start_injects_nothing() {
        let (mut w, mut r) = pipe();
        let (mut inj, sink) = injector();
        let (_tx, mut rx) = watch::channel(Permissions::NONE);
        w.send(&key(0x04, true).to_proto(0)).await.unwrap();
        w.send(&InputEvent::Wheel { dx: 0, dy: 120 }.to_proto(1))
            .await
            .unwrap();
        drop(w);
        let stats = serve_input(&mut r, &mut inj, &mut rx, HOLD_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(stats.injected, 0);
        assert_eq!(stats.dropped, 2);
        assert!(sink.calls().is_empty());
    }

    #[tokio::test]
    async fn a_quiet_viewer_loses_its_held_keys_but_keep_alives_keep_them() {
        let timeout = Duration::from_millis(150);
        let (mut w, mut r) = pipe();
        let (mut inj, sink) = injector();
        let (_tx, mut rx) = watch::channel(Permissions::ALL);
        let task = tokio::spawn(async move {
            let stats = serve_input(&mut r, &mut inj, &mut rx, timeout)
                .await
                .unwrap();
            (stats, inj.anything_held())
        });
        w.send(&key(0xE1, true).to_proto(0)).await.unwrap();
        // Held for 3 timeouts, kept alive: still held.
        for seq in 1..=6 {
            tokio::time::sleep(timeout / 2).await;
            w.send(&kanodesk_input::event::keep_alive(seq))
                .await
                .unwrap();
        }
        assert_eq!(sink.calls(), vec![SinkCall::Key(KeyCode(0xE1), true)]);
        // Then silence: released after one timeout.
        tokio::time::sleep(timeout * 3).await;
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Key(KeyCode(0xE1), true),
                SinkCall::Key(KeyCode(0xE1), false),
            ]
        );
        // A late release of the released key is just dropped.
        w.send(&key(0xE1, false).to_proto(7)).await.unwrap();
        drop(w);
        let (stats, held) = task.await.unwrap();
        assert!(!held);
        assert_eq!(stats.quiet_releases, 1);
        assert_eq!(stats.injected, 1);
        assert_eq!(stats.dropped, 1);
    }

    #[tokio::test]
    async fn a_broken_stream_still_releases_held_keys() {
        let (mut w, mut r) = pipe();
        let (mut inj, sink) = injector();
        let (_tx, mut rx) = watch::channel(Permissions::ALL);
        w.send(&key(0xE1, true).to_proto(0)).await.unwrap();
        // A truncated frame: the length says 100 bytes, 2 follow, then EOF.
        let mut raw = w.into_inner();
        tokio::io::AsyncWriteExt::write_all(&mut raw, &[0, 0, 0, 100, 1, 2])
            .await
            .unwrap();
        drop(raw);
        assert!(
            serve_input(&mut r, &mut inj, &mut rx, HOLD_TIMEOUT)
                .await
                .is_err()
        );
        assert_eq!(
            sink.calls(),
            vec![
                SinkCall::Key(KeyCode(0xE1), true),
                SinkCall::Key(KeyCode(0xE1), false),
            ]
        );
    }
}
