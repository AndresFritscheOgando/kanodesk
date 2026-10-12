//! Typed host settings, read from the option table.

use std::time::Duration;

use crate::config::Config;

/// How incoming sessions are approved (`approve-mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApproveMode {
    /// The viewer must know a password; no click needed.
    Password,
    /// The host's user clicks Accept; no password needed.
    Click,
    /// A password and a click.
    Both,
}

impl ApproveMode {
    /// Whether the viewer must prove a password.
    pub fn needs_password(self) -> bool {
        matches!(self, ApproveMode::Password | ApproveMode::Both)
    }

    /// Whether the host's user must accept.
    pub fn needs_click(self) -> bool {
        matches!(self, ApproveMode::Click | ApproveMode::Both)
    }

    fn parse(text: &str) -> ApproveMode {
        match text {
            "click" => ApproveMode::Click,
            "both" => ApproveMode::Both,
            _ => ApproveMode::Password,
        }
    }
}

/// Everything the host engine reads from the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSettings {
    /// How sessions are approved.
    pub approve_mode: ApproveMode,
    /// Temporary password length.
    pub temporary_password_length: usize,
    /// How long Accept stays disabled after a request appears.
    pub click_delay: Duration,
    /// How long a viewer waits for the host's user.
    pub approve_timeout: Duration,
    /// Default keyboard permission of new sessions.
    pub keyboard: bool,
    /// Default mouse permission of new sessions.
    pub mouse: bool,
    /// Accept direct connections.
    pub direct_server: bool,
    /// Where direct connections arrive.
    pub direct_listen: String,
    /// ID server, if configured.
    pub rendezvous_server: Option<String>,
    /// The ID server's key (hex), if configured.
    pub rendezvous_key: Option<String>,
    /// Frames per second.
    pub fps: u32,
    /// Video bitrate, or `None` for automatic.
    pub bitrate_kbps: Option<u32>,
    /// Hide the connection manager (only honoured in password-only mode,
    /// since click approval needs it).
    pub hide_cm: bool,
}

impl HostSettings {
    /// Reads the settings from `config`.
    pub fn from_config(config: &Config) -> HostSettings {
        let approve_mode = ApproveMode::parse(&config.text("approve-mode"));
        let non_empty = |s: String| (!s.is_empty()).then_some(s);
        HostSettings {
            approve_mode,
            temporary_password_length: config.int("temporary-password-length") as usize,
            click_delay: Duration::from_millis(config.int("approve-click-delay-ms") as u64),
            approve_timeout: Duration::from_secs(config.int("approve-timeout-secs") as u64),
            keyboard: config.bool("enable-keyboard"),
            mouse: config.bool("enable-mouse"),
            direct_server: config.bool("direct-server"),
            direct_listen: config.text("direct-access-listen"),
            rendezvous_server: non_empty(config.text("custom-rendezvous-server")),
            rendezvous_key: non_empty(config.text("key")),
            fps: config.int("custom-fps") as u32,
            bitrate_kbps: match config.int("video-bitrate-kbps") {
                0 => None,
                n => Some(n as u32),
            },
            hide_cm: config.bool("hide_cm") && approve_mode == ApproveMode::Password,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let s = HostSettings::from_config(&Config::in_memory());
        assert_eq!(s.approve_mode, ApproveMode::Password);
        assert!(s.approve_mode.needs_password() && !s.approve_mode.needs_click());
        assert_eq!(s.temporary_password_length, 8);
        assert_eq!(s.click_delay, Duration::from_secs(1));
        assert_eq!(s.approve_timeout, Duration::from_secs(60));
        assert!(s.keyboard && s.mouse && s.direct_server);
        assert_eq!(s.direct_listen, "0.0.0.0:21118");
        assert_eq!((s.rendezvous_server, s.rendezvous_key), (None, None));
        assert_eq!((s.fps, s.bitrate_kbps), (30, None));
        assert!(!s.hide_cm);
    }

    #[test]
    fn modes_and_hide_cm() {
        let mut c = Config::in_memory();
        c.set("approve-mode", "both").unwrap();
        c.set("hide_cm", "Y").unwrap();
        c.set("video-bitrate-kbps", "2500").unwrap();
        let s = HostSettings::from_config(&c);
        assert!(s.approve_mode.needs_password() && s.approve_mode.needs_click());
        assert!(!s.hide_cm, "the CM cannot be hidden when clicks are needed");
        assert_eq!(s.bitrate_kbps, Some(2500));
        c.set("approve-mode", "password").unwrap();
        assert!(HostSettings::from_config(&c).hide_cm);
        c.set("approve-mode", "click").unwrap();
        let s = HostSettings::from_config(&c);
        assert!(!s.approve_mode.needs_password() && s.approve_mode.needs_click());
    }
}
