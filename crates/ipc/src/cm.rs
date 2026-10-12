//! Messages between the host process and the connection manager (11 §4).
//!
//! The host runs the IPC server (role [`CM_ROLE`]); the connection manager
//! connects, says [`CmToHost::Hello`], and gets [`HostToCm::Hello`] with the
//! host's status and every current session. From then on the host pushes
//! session changes and the CM sends decisions and commands. The host is the
//! authority for permissions: the CM asks, the host applies and reports
//! the result back as an updated [`SessionInfo`].

use serde::{Deserialize, Serialize};

/// IPC role (endpoint name) of the host's CM server.
pub const CM_ROLE: &str = "cm";

/// Version of these messages; both sides must agree.
pub const CM_PROTOCOL: u32 = 2;

/// What a session may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Perms {
    /// Keyboard input allowed.
    pub keyboard: bool,
    /// Mouse input allowed.
    pub mouse: bool,
}

/// Where a session is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    /// Waiting for the host's user to accept or deny.
    AwaitingApproval {
        /// When Accept may be clicked (Unix ms); before that it is disabled.
        accept_after_unix_ms: u64,
        /// When the request times out (Unix ms).
        expires_unix_ms: u64,
    },
    /// Logged in; video and input running.
    Active,
    /// Over.
    Ended {
        /// Why, in words.
        reason: String,
    },
}

/// One incoming session, as the host's user sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Host-assigned id.
    pub id: u64,
    /// The viewer's authenticated device key (hex).
    pub device_key: String,
    /// What the viewer says about itself.
    pub hostname: String,
    /// The viewer's user name.
    pub username: String,
    /// The viewer's OS.
    pub platform: String,
    /// The viewer's software.
    pub agent: String,
    /// Network address it connected from.
    pub address: String,
    /// Which password it used ("temporary", "permanent"), if any.
    pub password: Option<String>,
    /// When it connected (Unix ms).
    pub started_unix_ms: u64,
    /// Current permissions.
    pub permissions: Perms,
    /// Current state.
    pub state: SessionState,
}

/// The host as the CM shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    /// This host's device key (hex).
    pub device_key: String,
    /// The current temporary password, if password logins are on.
    pub temporary_password: Option<String>,
    /// `approve-mode` ("password", "click", "both").
    pub approve_mode: String,
    /// Where direct connections arrive, if enabled.
    pub listen: Option<String>,
    /// Whether a permanent password is set.
    pub has_permanent_password: bool,
}

/// Host to CM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostToCm {
    /// First message: protocol, status and all current sessions.
    Hello {
        /// [`CM_PROTOCOL`].
        protocol: u32,
        /// Host status.
        status: HostStatus,
        /// Every session not yet ended.
        sessions: Vec<SessionInfo>,
    },
    /// The status changed (e.g. a new temporary password).
    Status(HostStatus),
    /// A new session, or a change to one (state, permissions).
    Session(SessionInfo),
}

/// CM to host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmToHost {
    /// First message.
    Hello {
        /// [`CM_PROTOCOL`].
        protocol: u32,
    },
    /// Accept or deny a session awaiting approval, with its permissions.
    Decide {
        /// Session id.
        id: u64,
        /// Accept (`true`) or deny.
        accept: bool,
        /// Permissions to start with.
        permissions: Perms,
    },
    /// Change a session's permissions live.
    SetPermissions {
        /// Session id.
        id: u64,
        /// New permissions.
        permissions: Perms,
    },
    /// End a session.
    Disconnect {
        /// Session id.
        id: u64,
    },
}

/// Milliseconds since the Unix epoch.
pub fn unix_ms(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionInfo {
        SessionInfo {
            id: 3,
            device_key: "ab".repeat(32),
            hostname: "LAPTOP".into(),
            username: "ana".into(),
            platform: "windows".into(),
            agent: "kanodesk/0.1".into(),
            address: "192.168.1.20:50000".into(),
            password: Some("temporary".into()),
            started_unix_ms: 1,
            permissions: Perms {
                keyboard: true,
                mouse: false,
            },
            state: SessionState::AwaitingApproval {
                accept_after_unix_ms: 2,
                expires_unix_ms: 3,
            },
        }
    }

    #[test]
    fn messages_round_trip_through_postcard() {
        let host = [
            HostToCm::Hello {
                protocol: CM_PROTOCOL,
                status: HostStatus {
                    device_key: "cd".repeat(32),
                    temporary_password: Some("k7mq4z2p".into()),
                    approve_mode: "both".into(),
                    listen: None,
                    has_permanent_password: true,
                },
                sessions: vec![session()],
            },
            HostToCm::Session(SessionInfo {
                state: SessionState::Ended {
                    reason: "denied".into(),
                },
                ..session()
            }),
        ];
        for m in host {
            let bytes = postcard::to_stdvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<HostToCm>(&bytes).unwrap(), m);
        }
        let cm = [
            CmToHost::Hello {
                protocol: CM_PROTOCOL,
            },
            CmToHost::Decide {
                id: 3,
                accept: true,
                permissions: Perms {
                    keyboard: false,
                    mouse: true,
                },
            },
            CmToHost::SetPermissions {
                id: 3,
                permissions: Perms {
                    keyboard: true,
                    mouse: true,
                },
            },
            CmToHost::Disconnect { id: 3 },
        ];
        for m in cm {
            let bytes = postcard::to_stdvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<CmToHost>(&bytes).unwrap(), m);
        }
        assert!(unix_ms(std::time::SystemTime::now()) > 1_700_000_000_000);
    }
}
