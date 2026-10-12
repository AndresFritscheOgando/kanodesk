//! Messages between the host process and the main window (15 §2, "Main").
//!
//! The host runs a second IPC server (role [`CONTROL_ROLE`]) next to the
//! connection manager's. The main window connects, says
//! [`UiToHost::Hello`], and gets [`HostToUi::Hello`] with a
//! [`HostOverview`]: this device's ID, its temporary password, whether the
//! ID server is reachable. The host pushes a new overview whenever any of
//! that changes. The window asks for a new temporary password, sets or
//! clears the permanent one, tells the host to re-read the config after the
//! user saved settings, and asks it to stop when the window started it.
//!
//! The pipe only admits processes of the same Windows session (see the
//! crate docs), which is why passwords may travel over it.

use serde::{Deserialize, Serialize};

use crate::cm::HostStatus;

/// IPC role (endpoint name) of the host's control server.
pub const CONTROL_ROLE: &str = "control";

/// Version of these messages; both sides must agree.
pub const CONTROL_PROTOCOL: u32 = 1;

/// The host's link to the ID server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdServerState {
    /// No ID server address or key in the config.
    #[default]
    NotConfigured,
    /// Connecting or registering.
    Connecting,
    /// Registered: reachable by ID.
    Online,
    /// The last attempt failed; the host retries.
    Failed(String),
}

/// Everything the main window shows about this device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostOverview {
    /// Device key, passwords, approve mode.
    pub status: HostStatus,
    /// This device's ID (9 digits), once the ID server assigned one.
    pub id: Option<String>,
    /// The ID server link.
    pub id_server: IdServerState,
    /// The configured ID server, if any.
    pub id_server_address: Option<String>,
    /// Sessions currently connected to this host.
    pub active_sessions: u32,
    /// The host's software version.
    pub version: String,
}

/// Main window to host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiToHost {
    /// First message.
    Hello {
        /// [`CONTROL_PROTOCOL`].
        protocol: u32,
    },
    /// Replace the temporary password now.
    RefreshPassword,
    /// Set (`Some`) or clear (`None`) the permanent password.
    SetPermanentPassword(Option<String>),
    /// The config file changed: apply it (approve mode, password length,
    /// default permissions, ID server).
    ReloadConfig,
    /// Stop the host process.
    Shutdown,
}

/// Host to main window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostToUi {
    /// First message: protocol and the current overview.
    Hello {
        /// [`CONTROL_PROTOCOL`].
        protocol: u32,
        /// Current state.
        overview: HostOverview,
    },
    /// The overview changed.
    Overview(HostOverview),
    /// The outcome of a request (`SetPermanentPassword`, `ReloadConfig`).
    Reply {
        /// Whether it worked.
        ok: bool,
        /// What happened, in words.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_through_postcard() {
        let overview = HostOverview {
            status: HostStatus {
                device_key: "ab".repeat(32),
                temporary_password: Some("k7mq4z2p".into()),
                approve_mode: "password".into(),
                listen: Some("0.0.0.0:21118".into()),
                has_permanent_password: false,
            },
            id: Some("123456789".into()),
            id_server: IdServerState::Failed("timed out".into()),
            id_server_address: Some("ids.example.com:21116".into()),
            active_sessions: 2,
            version: "0.1.0".into(),
        };
        for m in [
            HostToUi::Hello {
                protocol: CONTROL_PROTOCOL,
                overview: overview.clone(),
            },
            HostToUi::Overview(HostOverview {
                id_server: IdServerState::Online,
                ..overview
            }),
            HostToUi::Reply {
                ok: false,
                message: "too short".into(),
            },
        ] {
            let bytes = postcard::to_stdvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<HostToUi>(&bytes).unwrap(), m);
        }
        for m in [
            UiToHost::Hello {
                protocol: CONTROL_PROTOCOL,
            },
            UiToHost::RefreshPassword,
            UiToHost::SetPermanentPassword(Some("Correct Horse".into())),
            UiToHost::SetPermanentPassword(None),
            UiToHost::ReloadConfig,
            UiToHost::Shutdown,
        ] {
            let bytes = postcard::to_stdvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<UiToHost>(&bytes).unwrap(), m);
        }
        assert_eq!(IdServerState::default(), IdServerState::NotConfigured);
    }
}
