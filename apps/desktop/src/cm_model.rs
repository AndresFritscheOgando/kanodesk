//! The connection manager's state, independent of the window: what the host
//! reported, what the user chose, and the messages the user's actions send.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use kanodesk_ipc::cm::{CmToHost, HostStatus, HostToCm, Perms, SessionInfo, SessionState};

/// How long an ended session stays in the list.
pub const KEEP_ENDED: Duration = Duration::from_secs(10);

/// One session in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// As the host reported it.
    pub info: SessionInfo,
    /// Permissions chosen in the accept dialog (for a waiting session).
    pub choice: Perms,
    /// When it ended, if it did.
    pub ended_at: Option<Instant>,
}

impl Entry {
    /// Whether it waits for Accept/Deny.
    pub fn awaiting(&self) -> bool {
        matches!(self.info.state, SessionState::AwaitingApproval { .. })
    }

    /// Whether it is logged in and running.
    pub fn active(&self) -> bool {
        self.info.state == SessionState::Active
    }
}

/// Whether a message from the host needs the user's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// Nothing to do.
    None,
    /// Someone asks to connect: bring the window to the front.
    NewRequest,
}

/// The connection manager's state.
#[derive(Debug, Default)]
pub struct CmModel {
    /// The host's status, once known.
    pub status: Option<HostStatus>,
    /// Whether the host process is connected.
    pub connected: bool,
    entries: BTreeMap<u64, Entry>,
    /// When the last live session went away (for closing the window).
    idle_since: Option<Instant>,
    /// Whether any session was ever shown.
    seen_any: bool,
}

impl CmModel {
    /// Applies one message from the host.
    pub fn apply(&mut self, msg: HostToCm, now: Instant) -> Attention {
        let attention = match msg {
            HostToCm::Hello {
                status, sessions, ..
            } => {
                self.status = Some(status);
                self.connected = true;
                self.entries.retain(|_, e| e.ended_at.is_some());
                let mut attention = Attention::None;
                for info in sessions {
                    if self.upsert(info, now) {
                        attention = Attention::NewRequest;
                    }
                }
                attention
            }
            HostToCm::Status(status) => {
                self.status = Some(status);
                Attention::None
            }
            HostToCm::Session(info) => {
                if self.upsert(info, now) {
                    Attention::NewRequest
                } else {
                    Attention::None
                }
            }
        };
        self.update_idle(now);
        attention
    }

    /// Inserts or updates; `true` if this is a new approval request.
    fn upsert(&mut self, info: SessionInfo, now: Instant) -> bool {
        self.seen_any = true;
        let ended = matches!(info.state, SessionState::Ended { .. });
        match self.entries.get_mut(&info.id) {
            Some(e) => {
                let was_awaiting = e.awaiting();
                e.info = info;
                if ended && e.ended_at.is_none() {
                    e.ended_at = Some(now);
                }
                // A request that turned into something else keeps no choice.
                !was_awaiting && e.awaiting()
            }
            None => {
                let new_request = matches!(info.state, SessionState::AwaitingApproval { .. });
                self.entries.insert(
                    info.id,
                    Entry {
                        choice: info.permissions,
                        ended_at: ended.then_some(now),
                        info,
                    },
                );
                new_request
            }
        }
    }

    fn update_idle(&mut self, now: Instant) {
        let live = self.entries.values().any(|e| e.ended_at.is_none());
        if live {
            self.idle_since = None;
        } else if self.idle_since.is_none() {
            self.idle_since = Some(now);
        }
    }

    /// The host process went away: its sessions are gone too.
    pub fn disconnected(&mut self, now: Instant) {
        self.connected = false;
        for e in self.entries.values_mut() {
            if e.ended_at.is_none() {
                e.ended_at = Some(now);
                e.info.state = SessionState::Ended {
                    reason: "the KanoDesk host stopped".into(),
                };
            }
        }
        self.update_idle(now);
    }

    /// Drops ended sessions after [`KEEP_ENDED`].
    pub fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, e| {
            e.ended_at
                .is_none_or(|t| now.duration_since(t) < KEEP_ENDED)
        });
    }

    /// The sessions, oldest first.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// Whether the window has nothing left to show: a session was shown,
    /// none is live, and the last one ended at least `after` ago.
    pub fn done(&self, now: Instant, after: Duration) -> bool {
        self.seen_any
            && self
                .idle_since
                .is_some_and(|t| now.duration_since(t) >= after)
    }

    /// Changes the permissions offered with Accept for a waiting session.
    pub fn choose(&mut self, id: u64, choice: Perms) {
        if let Some(e) = self.entries.get_mut(&id)
            && e.awaiting()
        {
            e.choice = choice;
        }
    }

    /// Accept a waiting session with the chosen permissions.
    pub fn accept(&self, id: u64, now_unix_ms: u64) -> Option<CmToHost> {
        let e = self.entries.get(&id)?;
        accept_enabled(&e.info, now_unix_ms).then_some(CmToHost::Decide {
            id,
            accept: true,
            permissions: e.choice,
        })
    }

    /// Deny a waiting session.
    pub fn deny(&self, id: u64) -> Option<CmToHost> {
        let e = self.entries.get(&id)?;
        e.awaiting().then_some(CmToHost::Decide {
            id,
            accept: false,
            permissions: e.choice,
        })
    }

    /// Change an active session's permissions (the host confirms with an update).
    pub fn set_permissions(&self, id: u64, permissions: Perms) -> Option<CmToHost> {
        let e = self.entries.get(&id)?;
        (e.active() && e.info.permissions != permissions)
            .then_some(CmToHost::SetPermissions { id, permissions })
    }

    /// End an active session.
    pub fn disconnect(&self, id: u64) -> Option<CmToHost> {
        let e = self.entries.get(&id)?;
        e.active().then_some(CmToHost::Disconnect { id })
    }
}

/// Whether Accept may be clicked now: the request is waiting and its
/// anti-spoof delay has passed.
pub fn accept_enabled(info: &SessionInfo, now_unix_ms: u64) -> bool {
    match info.state {
        SessionState::AwaitingApproval {
            accept_after_unix_ms,
            ..
        } => now_unix_ms >= accept_after_unix_ms,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanodesk_ipc::cm::CM_PROTOCOL;

    const ALL: Perms = Perms {
        keyboard: true,
        mouse: true,
    };

    fn session(id: u64, state: SessionState) -> SessionInfo {
        SessionInfo {
            id,
            device_key: "00".repeat(32),
            hostname: "LAPTOP".into(),
            username: "ana".into(),
            platform: "windows".into(),
            agent: "kanodesk".into(),
            address: "10.0.0.2:5000".into(),
            password: None,
            started_unix_ms: 0,
            permissions: ALL,
            state,
        }
    }

    fn waiting(accept_after: u64) -> SessionState {
        SessionState::AwaitingApproval {
            accept_after_unix_ms: accept_after,
            expires_unix_ms: accept_after + 60_000,
        }
    }

    fn status() -> HostStatus {
        HostStatus {
            device_key: "ab".into(),
            temporary_password: None,
            approve_mode: "click".into(),
            listen: None,
            has_permanent_password: false,
        }
    }

    #[test]
    fn requests_ask_for_attention_and_accept_waits_for_the_delay() {
        let t0 = Instant::now();
        let mut m = CmModel::default();
        assert_eq!(
            m.apply(
                HostToCm::Hello {
                    protocol: CM_PROTOCOL,
                    status: status(),
                    sessions: vec![],
                },
                t0
            ),
            Attention::None
        );
        assert!(m.connected);
        assert_eq!(
            m.apply(HostToCm::Session(session(1, waiting(1_000))), t0),
            Attention::NewRequest
        );
        // Accept is disabled until the delay has passed; Deny works at once.
        assert_eq!(m.accept(1, 999), None);
        assert!(m.deny(1).is_some());
        // The permissions chosen in the dialog go with Accept.
        let choice = Perms {
            keyboard: false,
            mouse: true,
        };
        m.choose(1, choice);
        assert_eq!(
            m.accept(1, 1_000),
            Some(CmToHost::Decide {
                id: 1,
                accept: true,
                permissions: choice
            })
        );
        // An update to the same request is not a new request.
        assert_eq!(
            m.apply(HostToCm::Session(session(1, waiting(1_000))), t0),
            Attention::None
        );
        assert_eq!(m.entries().next().unwrap().choice, choice, "choice kept");
    }

    #[test]
    fn active_sessions_toggle_and_disconnect() {
        let t0 = Instant::now();
        let mut m = CmModel::default();
        m.apply(HostToCm::Session(session(2, SessionState::Active)), t0);
        assert_eq!(m.accept(2, u64::MAX), None, "nothing to accept");
        assert_eq!(m.deny(2), None);
        assert_eq!(m.set_permissions(2, ALL), None, "no change");
        let off = Perms {
            keyboard: false,
            mouse: false,
        };
        assert_eq!(
            m.set_permissions(2, off),
            Some(CmToHost::SetPermissions {
                id: 2,
                permissions: off
            })
        );
        assert_eq!(m.disconnect(2), Some(CmToHost::Disconnect { id: 2 }));
        assert_eq!(m.disconnect(9), None);
    }

    #[test]
    fn ended_sessions_linger_then_go_and_the_window_can_close() {
        let t0 = Instant::now();
        let mut m = CmModel::default();
        assert!(!m.done(t0, Duration::ZERO), "nothing shown yet");
        m.apply(HostToCm::Session(session(3, SessionState::Active)), t0);
        assert!(!m.done(t0 + Duration::from_secs(60), Duration::ZERO));
        m.apply(
            HostToCm::Session(session(
                3,
                SessionState::Ended {
                    reason: "denied".into(),
                },
            )),
            t0,
        );
        assert_eq!(m.disconnect(3), None, "ended");
        assert!(!m.done(t0 + Duration::from_secs(1), Duration::from_secs(5)));
        assert!(m.done(t0 + Duration::from_secs(5), Duration::from_secs(5)));
        m.prune(t0 + Duration::from_secs(1));
        assert_eq!(m.entries().count(), 1, "still listed");
        m.prune(t0 + KEEP_ENDED);
        assert_eq!(m.entries().count(), 0);
    }

    #[test]
    fn losing_the_host_ends_everything() {
        let t0 = Instant::now();
        let mut m = CmModel::default();
        m.apply(
            HostToCm::Hello {
                protocol: CM_PROTOCOL,
                status: status(),
                sessions: vec![session(4, SessionState::Active), session(5, waiting(0))],
            },
            t0,
        );
        m.disconnected(t0);
        assert!(!m.connected);
        assert!(m.entries().all(|e| e.ended_at.is_some()));
        assert_eq!(m.deny(5), None);
        // A reconnect replaces the live list but keeps recently ended ones.
        m.apply(
            HostToCm::Hello {
                protocol: CM_PROTOCOL,
                status: status(),
                sessions: vec![session(6, SessionState::Active)],
            },
            t0,
        );
        assert_eq!(m.entries().count(), 3);
        assert!(m.status.as_ref().is_some_and(|s| s.device_key == "ab"));
        m.apply(
            HostToCm::Status(HostStatus {
                temporary_password: Some("x".into()),
                ..status()
            }),
            t0,
        );
        assert_eq!(m.status.unwrap().temporary_password.as_deref(), Some("x"));
    }
}
