//! The host's IPC server for the main window ([`kanodesk_ipc::control`]).
//!
//! Every connected window gets the current [`HostOverview`] on hello and
//! again whenever it changes. Its requests come out of [`ControlServer`] as
//! [`Request`]s; the application carries them out (see
//! [`crate::runtime::HostRuntime::handle`]) and may answer with a
//! [`HostToUi::Reply`].

use kanodesk_ipc::control::{CONTROL_PROTOCOL, CONTROL_ROLE, HostOverview, HostToUi, UiToHost};
use kanodesk_ipc::{IpcError, Listener};
use tokio::sync::{mpsc, oneshot, watch};

/// One request from a main window.
#[derive(Debug)]
pub struct Request {
    /// What it asks for (never `Hello`).
    pub command: UiToHost,
    reply: oneshot::Sender<Option<(bool, String)>>,
}

impl Request {
    /// A request with a channel for its answer (tests and in-process use).
    pub fn new(command: UiToHost) -> (Request, oneshot::Receiver<Option<(bool, String)>>) {
        let (reply, rx) = oneshot::channel();
        (Request { command, reply }, rx)
    }

    /// Answers the window (`None`: nothing to say; the overview shows the result).
    pub fn reply(self, answer: Option<(bool, String)>) {
        let _ = self.reply.send(answer);
    }
}

/// Serves main windows on the control endpoint.
pub struct ControlServer;

impl ControlServer {
    /// Serves this Windows session's control endpoint; returns the requests.
    pub fn start(
        overview: watch::Receiver<HostOverview>,
    ) -> Result<mpsc::UnboundedReceiver<Request>, IpcError> {
        Self::start_on(&kanodesk_ipc::endpoint_name(CONTROL_ROLE), overview)
    }

    /// Like [`Self::start`] on a given endpoint name (tests).
    pub fn start_on(
        name: &str,
        overview: watch::Receiver<HostOverview>,
    ) -> Result<mpsc::UnboundedReceiver<Request>, IpcError> {
        let mut listener = Listener::bind(name)?;
        let (requests, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(conn) = listener.accept().await {
                tokio::spawn(client(conn, overview.clone(), requests.clone()));
            }
        });
        Ok(rx)
    }
}

async fn client(
    conn: kanodesk_ipc::Connection,
    mut overview: watch::Receiver<HostOverview>,
    requests: mpsc::UnboundedSender<Request>,
) {
    let (mut tx, mut rx) = conn.split();
    match rx.recv::<UiToHost>().await {
        Ok(Some(UiToHost::Hello { protocol })) if protocol == CONTROL_PROTOCOL => {}
        _ => return,
    }
    let now = overview.borrow_and_update().clone();
    if tx
        .send(&HostToUi::Hello {
            protocol: CONTROL_PROTOCOL,
            overview: now,
        })
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            changed = overview.changed() => {
                if changed.is_err() {
                    return;
                }
                let now = overview.borrow_and_update().clone();
                if tx.send(&HostToUi::Overview(now)).await.is_err() {
                    return;
                }
            }
            msg = rx.recv::<UiToHost>() => {
                let command = match msg {
                    Ok(Some(UiToHost::Hello { .. })) => continue,
                    Ok(Some(m)) => m,
                    _ => return,
                };
                let (request, answer) = Request::new(command);
                if requests.send(request).is_err() {
                    return;
                }
                if let Ok(Some((ok, message))) = answer.await
                    && tx.send(&HostToUi::Reply { ok, message }).await.is_err()
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanodesk_ipc::control::IdServerState;

    #[tokio::test]
    async fn windows_get_the_overview_and_send_requests() {
        let name = kanodesk_ipc::endpoint_name(&format!("control-test-{}", std::process::id()));
        let (set, overview) = watch::channel(HostOverview::default());
        let mut requests = ControlServer::start_on(&name, overview).unwrap();

        // A client that skips the hello is ignored.
        let (mut bad, mut bad_rx) = kanodesk_ipc::connect(&name).await.unwrap().split();
        bad.send(&UiToHost::RefreshPassword).await.unwrap();
        assert!(!matches!(bad_rx.recv::<HostToUi>().await, Ok(Some(_))));

        let (mut tx, mut rx) = kanodesk_ipc::connect(&name).await.unwrap().split();
        tx.send(&UiToHost::Hello {
            protocol: CONTROL_PROTOCOL,
        })
        .await
        .unwrap();
        assert!(matches!(
            rx.recv::<HostToUi>().await.unwrap(),
            Some(HostToUi::Hello {
                protocol: CONTROL_PROTOCOL,
                ..
            })
        ));

        // Changes are pushed.
        set.send_modify(|o| {
            o.id = Some("123456789".into());
            o.id_server = IdServerState::Online;
        });
        match rx.recv::<HostToUi>().await.unwrap() {
            Some(HostToUi::Overview(o)) => assert_eq!(o.id.as_deref(), Some("123456789")),
            other => panic!("{other:?}"),
        }

        // A request comes out; its answer goes back.
        tx.send(&UiToHost::SetPermanentPassword(Some("short".into())))
            .await
            .unwrap();
        let r = requests.recv().await.unwrap();
        assert_eq!(
            r.command,
            UiToHost::SetPermanentPassword(Some("short".into()))
        );
        r.reply(Some((false, "too short".into())));
        assert_eq!(
            rx.recv::<HostToUi>().await.unwrap(),
            Some(HostToUi::Reply {
                ok: false,
                message: "too short".into()
            })
        );
        // No answer: nothing is sent, the connection stays usable.
        tx.send(&UiToHost::RefreshPassword).await.unwrap();
        requests.recv().await.unwrap().reply(None);
        set.send_modify(|o| o.active_sessions = 1);
        assert!(matches!(
            rx.recv::<HostToUi>().await.unwrap(),
            Some(HostToUi::Overview(o)) if o.active_sessions == 1
        ));
    }
}
